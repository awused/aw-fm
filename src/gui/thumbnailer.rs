use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::abort;
use std::sync::Arc;
use std::time::Instant;

use ahash::AHashSet;
use dirs::{data_dir, data_local_dir};
use gnome_desktop::traits::DesktopThumbnailFactoryExt;
use gnome_desktop::{DesktopThumbnailFactory, DesktopThumbnailSize};
use gtk::gdk::Texture;
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::gio::{Cancellable, ReadInputStream};
use gtk::glib::ffi::{g_get_user_data_dir, g_main_context_default, g_thread_self, GThread};
use gtk::glib::{Bytes, WeakRef};
use gtk::prelude::{FileExt, IconExt, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use rayon::{ThreadBuilder, ThreadPool, ThreadPoolBuilder};

use self::send::SendFactory;
use super::{gui_run, ThumbPriority};
use crate::com::EntryObject;
use crate::config::CONFIG;
use crate::{closing, handle_panic};


#[derive(Debug, Default)]
struct PendingThumbs {
    // Currently visible.
    high_priority: VecDeque<WeakRef<EntryObject>>,
    // Bound but not visible. Uses the same number of threads as low but runs earlier.
    med_priority: VecDeque<WeakRef<EntryObject>>,
    low_priority: Vec<WeakRef<EntryObject>>,

    // Never bother cloning these, it's a waste, just pass them around.
    // It's marginally more efficient (2-3 seconds worth over 100k items) to not clone and drop
    // these, also avoids doing manual math on the tickets.
    factories: Vec<SendFactory>,
    // TODO -- Save a map lookup?
    // ongoing: Vec<(Arc<Path>, WeakRef<EntryObject>)>,
    processed: usize,
}


#[derive(Debug)]
pub struct Thumbnailer {
    pending: RefCell<PendingThumbs>,
    // No real advantage to rayon over other pools here, but we already have it as a dependency.
    // Did test a fully glib-async version that uses GTasks under the hood, but the performance
    // wasn't any better and was sometimes much worse.
    pool: ThreadPool,

    high: u16,
    low: u16,
}

impl Thumbnailer {
    pub fn new() -> Self {
        let high = CONFIG.max_thumbnailers as u16;
        let low = CONFIG.background_thumbnailers as u16;

        let mut pending = PendingThumbs {
            factories: SendFactory::make(high),
            ..PendingThumbs::default()
        };

        let pool = ThreadPoolBuilder::new()
            .thread_name(|n| format!("thumbnailer-{n}"))
            .panic_handler(handle_panic)
            .num_threads(high.into())
            .build()
            .unwrap();

        Self { pending: pending.into(), pool, high, low }
    }

    pub fn queue(&self, weak: WeakRef<EntryObject>, p: ThumbPriority) {
        if self.high == 0 {
            return;
        }

        match p {
            ThumbPriority::Low => {
                if self.low == 0 {
                    return;
                }
                self.pending.borrow_mut().low_priority.push(weak);
            }
            ThumbPriority::Medium => {
                if self.low == 0 {
                    return;
                }
                // The ones added first aren't particularly useful, but if we have a bunch of bound
                // elements the ones added later are less likely to be useful.
                self.pending.borrow_mut().med_priority.push_back(weak);
            }
            ThumbPriority::High => {
                self.pending.borrow_mut().high_priority.push_back(weak);
            }
        }
        self.process();
    }

    fn done_with_ticket(factory: SendFactory) {
        gui_run(|g| {
            let t = &g.thumbnailer;
            t.pending.borrow_mut().factories.push(factory);
            t.process();
        });
    }

    fn finish_thumbnail(factory: SendFactory, tex: Texture, path: Arc<Path>) {
        gtk::glib::idle_add_once(move || {
            Self::done_with_ticket(factory);

            let Some(obj) = EntryObject::lookup(&path) else {
                return;
            };

            obj.imp().update_thumbnail(tex);
        });
    }

    fn fail_thumbnail(factory: SendFactory, path: Arc<Path>) {
        gtk::glib::idle_add_once(move || {
            Self::done_with_ticket(factory);

            let Some(obj) = EntryObject::lookup(&path) else {
                return;
            };
        });
    }

    fn find_next(&self) -> Option<(EntryObject, SendFactory)> {
        let mut pending = self.pending.borrow_mut();

        if pending.factories.is_empty() {
            return None;
        }

        while let Some(weak) = pending.high_priority.pop_front() {
            if let Some(strong) = weak.upgrade() {
                if strong.imp().mark_thumbnail_loading(ThumbPriority::High) {
                    pending.processed += 1;
                    return Some((strong, pending.factories.pop().unwrap()));
                }
            }
        }

        if self.high > self.low && self.high - pending.factories.len() as u16 > self.low {
            return None;
        }

        while let Some(weak) = pending.med_priority.pop_front() {
            if let Some(strong) = weak.upgrade() {
                if strong.imp().mark_thumbnail_loading(ThumbPriority::Medium) {
                    pending.processed += 1;
                    return Some((strong, pending.factories.pop().unwrap()));
                }
            }
        }

        while let Some(weak) = pending.low_priority.pop() {
            if let Some(strong) = weak.upgrade() {
                if strong.imp().mark_thumbnail_loading(ThumbPriority::Low) {
                    pending.processed += 1;
                    return Some((strong, pending.factories.pop().unwrap()));
                }
            }
        }

        if pending.factories.len() < self.high as usize {
            // Wait until all processing is done.
            return None;
        }

        if pending.processed > 32 {
            // Don't spam this if it's only for only a few updates.
            debug!("Finished loading all thumbnails (none pending).");
        }

        pending.processed = 0;
        pending.high_priority.shrink_to_fit();
        pending.med_priority.shrink_to_fit();
        pending.low_priority.shrink_to_fit();

        None
    }

    fn process(&self) {
        if closing::closed() {
            return;
        }

        let Some((obj, factory)) = self.find_next() else {
            return;
        };

        // Get what data we need and drop down to a weak ref.
        let entry = obj.get();
        let path = entry.abs_path.clone();
        let uri = gtk::gio::File::for_path(&entry.abs_path).uri();
        // It only cares about seconds
        let mtime_sec = obj.get().mtime.sec;
        let mime_type = entry.mime.clone();

        let start = Instant::now();

        self.pool.spawn(move || {
            let existing = factory.lookup(&uri, mtime_sec);

            if let Some(existing) = existing {
                let gfile = gtk::gio::File::for_path(existing);
                match Texture::from_file(&gfile) {
                    Ok(tex) => {
                        // This is just too spammy outside of debugging
                        // trace!("Loaded existing thumbnail for {uri:?} in {:?}",
                        // start.elapsed());
                        return Self::finish_thumbnail(factory, tex, path);
                    }
                    Err(e) => {
                        error!("Failed to load existing thumbnail: {e:?}");
                        return Self::fail_thumbnail(factory, path);
                    }
                }
            }

            if factory.has_failed(&uri, mtime_sec) {
                return Self::fail_thumbnail(factory, path);
            }

            if !factory.can_thumbnail(&uri, &mime_type, mtime_sec) {
                // trace!("Marking thumbnail as failed, though it wasn't attempted.");
                return Self::fail_thumbnail(factory, path);
            }

            match factory.generate_and_save_thumbnail(&uri, &mime_type, mtime_sec) {
                Some(tex) => {
                    trace!(
                        "Generated new thumbnail in {:?} for {:?}",
                        start.elapsed(),
                        path.file_name().unwrap_or(path.as_os_str())
                    );
                    Self::finish_thumbnail(factory, tex, path);
                }
                None => Self::fail_thumbnail(factory, path),
            }
        });
    }
}


mod send {
    use std::path::PathBuf;
    use std::ptr;
    use std::time::{Duration, Instant};

    use futures_executor::block_on;
    use gnome_desktop::traits::DesktopThumbnailFactoryExt;
    use gnome_desktop::{DesktopThumbnailFactory, DesktopThumbnailSize};
    use gtk::gdk::Texture;
    use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
    use gtk::gio::glib::GString;
    use gtk::gio::{Cancellable, Cancelled};
    use gtk::glib::ffi::{g_thread_self, GThread, G_SPAWN_ERROR_FAILED};
    use gtk::glib::{Bytes, Quark};

    use crate::closing;

    #[derive(Debug)]
    pub(super) struct SendFactory(DesktopThumbnailFactory, *mut GThread);

    // SAFETY: DesktopThumbnailFactory is, itself, thread safe. The problem is non-atomic
    // refcounting. By using drop() we ensure that the object is dropped from the main thread.
    // By not implementing Deref or allowing access to self.0, we ensure the factory cannot be
    // cloned.
    //
    // Since we have to always mark thumbnails as successes or failures, this aborts the process
    // rather than scheduling a separate callback main thread callback of its own for efficiency.
    //
    // An alternate implementation is just coercing a reference up to &'static, and assuming the
    // Gui object never moves, but we need to be a bit more careful in that case. This
    // implementation also prevents sloppy code.
    //
    // Alternatively could just make this abort on drop.
    unsafe impl Send for SendFactory {}

    impl Drop for SendFactory {
        fn drop(&mut self) {
            error!("Dropping a thumbnail factory, this shouldn't happen.");
            unsafe {
                if g_thread_self() != self.1 {
                    error!(
                        "Dropping DesktopThumbnailFactory from non-main thread. Aborting process."
                    );
                    // Deliberately kill everything, not just this thread.
                    std::process::abort();
                }
            }
        }
    }

    impl SendFactory {
        pub fn make(n: u16) -> Vec<Self> {
            let mut factories = Vec::with_capacity(n as usize);
            if n > 0 {
                let current_thread = unsafe { g_thread_self() };
                let f = DesktopThumbnailFactory::new(DesktopThumbnailSize::Normal);

                for _ in 0..n {
                    factories.push(Self(f.clone(), current_thread));
                }
            }

            factories
        }

        pub fn lookup(&self, uri: &str, mtime_sec: u64) -> Option<GString> {
            self.0.lookup(uri, mtime_sec as i64)
        }

        pub fn has_failed(&self, uri: &str, mtime_sec: u64) -> bool {
            self.0.has_valid_failed_thumbnail(uri, mtime_sec as i64)
        }

        pub fn can_thumbnail(&self, uri: &str, mime_type: &str, mtime_sec: u64) -> bool {
            self.0.can_thumbnail(uri, mime_type, mtime_sec as i64)
        }

        // It would be faster for the UI to set the thumbnail first and then go to save it.
        // Given how small and simple these files are, and the weird cases that could happen if we
        // fail to save a thumbnail after setting it, it's just not worth it.
        pub fn generate_and_save_thumbnail(
            &self,
            uri: &str,
            mime_type: &str,
            mtime_sec: u64,
        ) -> Option<Texture> {
            let generated = self.0.generate_thumbnail(uri, mime_type, None::<&Cancellable>);

            let pb = match generated {
                Ok(pb) => pb,
                Err(e) => {
                    if closing::closed() {
                        return None;
                    }

                    if e.domain() == Quark::from_str("g-exec-error-quark") {
                        // These represent errors with the thumbnail process itself, such as being
                        // killed. If the process exits on its own but fails the
                        // domain will be g-spawn-exit-error-quark.
                        error!("Thumbnailing failed abnormally for {uri:?} ({mime_type}): {e}");
                        return None;
                    }

                    error!("Failed to generate thumbnail for {uri:?} ({mime_type}): {e}");
                    if let Err(e) =
                        self.0.create_failed_thumbnail(uri, mtime_sec as i64, None::<&Cancellable>)
                    {
                        // Not a serious error for aw-fm, we will still skip trying multiple times,
                        // but it will be retried unnecessarily in the future.
                        error!("Failed to save failed thumbnail for {uri:?}: {e}");
                    }
                    return None;
                }
            };

            if let Err(e) = self.0.save_thumbnail(&pb, uri, mtime_sec as i64, None::<&Cancellable>)
            {
                error!("Failed to save thumbnail for {uri:?}: {e}");
                // Don't try to save a failed thumbnail here. We can retry in the future.
                return None;
            }

            Some(Texture::for_pixbuf(&pb))
        }
    }
}
