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
use rayon::{ThreadBuilder, ThreadPool, ThreadPoolBuilder};

use self::send::SendFactory;
use super::GUI;
use crate::com::EntryObject;
use crate::handle_panic;

// How many concurrent thumbnail processes we allow.
// There are tradeoffs between how fast we want to generate the thumbnails the user is looking at
// and how much we can accept in terms of choppiness.
static MAX_TICKETS: usize = 4;
// Low priority runs with this many threads. No effect if higher than MAX_TICKETS.
// If 0, no thumbnails will be generated in the background.
static LOW_PRIORITY: usize = 1;

#[derive(Debug, Default)]
struct PendingThumbs {
    // Neither FIFO or FILO here is really perfect.
    // FIFO is probably better more of the time than FILO.
    // The files the user is actually looking at right now are often somewhere in the middle.
    // FIFO can't really help with that but it at least tends to generate thumbnails from the top
    // down.
    high_priority: VecDeque<WeakRef<EntryObject>>,
    // For low priority, order doesn't matter much, since the objects can be created in any
    // order. More trouble than it's worth to get a useful order out of these.
    low_priority: Vec<WeakRef<EntryObject>>,
}


#[derive(Debug)]
pub struct Thumbnailer {
    pending: RefCell<PendingThumbs>,
    factory: SendFactory,
    // No real advantage to rayon over other pools here, but we already have it as a dependency.
    pool: ThreadPool,
    // More so we can drive prioritization.
    // In theory we could just dump all the tasks directly into rayon with a combination of spawn
    // and spawn_fifo.
    tickets: Cell<usize>,
}

impl Thumbnailer {
    // Would really prefer to do all this in a rayon threadpool entirely under my control.
    pub fn new() -> Self {
        println!("{:?}", data_dir());
        println!("{:?}", data_local_dir());
        unsafe {
            println!("{:?}", g_get_user_data_dir());
        }

        let pending = RefCell::default();
        let factory = SendFactory::new();
        let pool = ThreadPoolBuilder::new()
            .thread_name(|n| format!("thumbnailer-{n}"))
            .panic_handler(handle_panic)
            .num_threads(MAX_TICKETS)
            .build()
            .unwrap();

        Self {
            pending,
            factory,
            pool,
            tickets: Cell::new(MAX_TICKETS),
        }
    }

    pub fn low_priority(&self, weak: WeakRef<EntryObject>) {
        if LOW_PRIORITY > 0 {
            self.pending.borrow_mut().low_priority.push(weak);
            self.process();
        }
    }

    pub fn high_priority(&self, weak: WeakRef<EntryObject>) {
        self.pending.borrow_mut().high_priority.push_back(weak);
        self.process();
    }

    fn done_with_ticket() {
        GUI.with(|g| {
            let t = &g.get().unwrap().thumbnailer;
            t.tickets.set(t.tickets.get() + 1);
            t.process();
        });
    }

    fn finish_thumbnail(factory: SendFactory, pixbuf: Pixbuf, path: Arc<Path>) {
        let tex = Texture::for_pixbuf(&pixbuf);
        gtk::glib::idle_add_once(move || {
            drop(factory);
            // If this isn't on the main thread, this will crash. No chance of UB.
            Self::done_with_ticket();

            let Some(obj) = EntryObject::lookup(&path) else {
                return;
            };

            obj.update_thumbnail(tex);
        });
    }

    fn fail_thumbnail(factory: SendFactory, path: Arc<Path>) {
        gtk::glib::idle_add_once(move || {
            drop(factory);
            Self::done_with_ticket();

            let Some(obj) = EntryObject::lookup(&path) else {
                return;
            };
        });
    }

    fn find_next(&self) -> Option<EntryObject> {
        let mut pending = self.pending.borrow_mut();

        while let Some(weak) = pending.high_priority.pop_front() {
            if let Some(strong) = weak.upgrade() {
                if strong.mark_thumbnail_loading_high() {
                    return Some(strong);
                }
            }
        }

        if MAX_TICKETS > LOW_PRIORITY && MAX_TICKETS - self.tickets.get() > LOW_PRIORITY {
            return None;
        }

        while let Some(weak) = pending.low_priority.pop() {
            if let Some(strong) = weak.upgrade() {
                if strong.mark_thumbnail_loading_low() {
                    return Some(strong);
                }
            }
        }

        pending.high_priority.shrink_to_fit();
        pending.low_priority.shrink_to_fit();

        None
    }

    fn process(&self) {
        if self.tickets.get() == 0 {
            return;
        }

        let Some(obj) = self.find_next() else { return };

        self.tickets.set(self.tickets.get() - 1);

        // Get what data we need and drop down to a weak ref.
        let entry = obj.get();
        let path = entry.abs_path.clone();
        let uri = gtk::gio::File::for_path(&entry.abs_path).uri();
        // It only cares about seconds
        let mtime_sec = obj.get().mtime.sec;
        let mime_type = entry.mime.clone();

        let start = Instant::now();
        let factory = self.factory.clone();
        self.pool.spawn(move || {
            let existing = factory.lookup(&uri, mtime_sec);

            if let Some(existing) = existing {
                match Pixbuf::from_file(existing) {
                    Ok(pixbuf) => {
                        // This is just too spammy outside of debugging
                        // trace!("Loaded existing thumbnail for {uri:?} in {:?}", start.elapsed());
                        return Self::finish_thumbnail(factory, pixbuf, path);
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
                warn!("Marking thumbnail as failed, though it wasn't attempted.");
                return Self::fail_thumbnail(factory, path);
            }

            match factory.generate_and_save_thumbnail(&uri, &mime_type, mtime_sec) {
                Some(pixbuf) => {
                    trace!("Generated new thumbnail in {:?} for {uri:?}", start.elapsed());
                    Self::finish_thumbnail(factory, pixbuf, path);
                }
                None => Self::fail_thumbnail(factory, path),
            }
        });
    }
}


mod send {
    use std::path::PathBuf;
    use std::ptr;
    use std::time::Instant;

    use futures_executor::block_on;
    use gnome_desktop::traits::DesktopThumbnailFactoryExt;
    use gnome_desktop::{DesktopThumbnailFactory, DesktopThumbnailSize};
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
    // rather than scheduling a callback to run on the main thread for efficiencyi.
    //
    // An alternate implementation is just coercing a reference up to &'static, and assuming the
    // Gui object never moves, but we need to be a bit more careful in that case. This
    // implementation also prevents sloppy code.
    unsafe impl Send for SendFactory {}

    impl Drop for SendFactory {
        fn drop(&mut self) {
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
        pub fn new() -> Self {
            let current_thread = unsafe { g_thread_self() };
            Self(DesktopThumbnailFactory::new(DesktopThumbnailSize::Normal), current_thread)
        }

        pub fn clone(&self) -> Self {
            let current_thread = unsafe { g_thread_self() };
            assert_eq!(self.1, current_thread);
            Self(self.0.clone(), self.1)
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
        ) -> Option<Pixbuf> {
            let generated = self.0.generate_thumbnail(uri, mime_type, None::<&Cancellable>);

            let pb = match generated {
                Ok(pb) => pb,
                Err(e) => {
                    // if closing::closed() {
                    //     return None;
                    // }

                    if e.domain() == Quark::from_str("g-exec-error-quark") {
                        // These represent errors with the thumbnail process itself, such as being
                        // killed. If the process exits normally but fails it should be in the
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

            Some(pb)
        }
    }
}
