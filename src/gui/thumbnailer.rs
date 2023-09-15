use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use gnome_desktop::{DesktopThumbnailFactory, DesktopThumbnailFactoryExt};
use gtk::gdk::Texture;
use gtk::gio::{Cancellable, File};
use gtk::glib::{Quark, WeakRef};
use gtk::prelude::FileExt;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use rayon::{ThreadPool, ThreadPoolBuilder};

use self::send::SendFactory;
use super::{gui_run, ThumbPriority};
use crate::com::{EntryObject, FileTime};
use crate::config::CONFIG;
use crate::{closing, handle_panic};


#[derive(Debug, Default)]
struct PendingThumbs {
    // Currently visible.
    high_priority: VecDeque<(WeakRef<EntryObject>, bool)>,
    // Bound but not visible. Uses the same number of threads as low but runs earlier.
    med_priority: VecDeque<(WeakRef<EntryObject>, bool)>,
    low_priority: Vec<(WeakRef<EntryObject>, bool)>,

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

    sync_factory: Option<DesktopThumbnailFactory>,
}

impl Thumbnailer {
    pub fn new() -> Self {
        let high = CONFIG.max_thumbnailers as u16;
        let low = CONFIG.background_thumbnailers as u16;
        let (sync_factory, factories) = SendFactory::make(high);

        let pending = PendingThumbs { factories, ..PendingThumbs::default() };

        let pool = ThreadPoolBuilder::new()
            .thread_name(|n| format!("thumbnailer-{n}"))
            .panic_handler(handle_panic)
            .num_threads(high.into())
            .build()
            .unwrap();

        Self {
            pending: pending.into(),
            pool,
            high,
            low,
            sync_factory,
        }
    }

    pub fn queue(&self, weak: WeakRef<EntryObject>, p: ThumbPriority, from_event: bool) {
        if self.high == 0 {
            return;
        }

        match p {
            ThumbPriority::Low => {
                if self.low == 0 {
                    return;
                }
                self.pending.borrow_mut().low_priority.push((weak, from_event));
            }
            ThumbPriority::Medium => {
                if self.low == 0 {
                    return;
                }
                // The ones added first aren't particularly useful, but if we have a bunch of bound
                // elements the ones added later are less likely to be useful.
                self.pending.borrow_mut().med_priority.push_back((weak, from_event));
            }
            ThumbPriority::High => {
                self.pending.borrow_mut().high_priority.push_back((weak, from_event));
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

    fn finish_thumbnail(factory: SendFactory, tex: Texture, path: Arc<Path>, mtime: FileTime) {
        gtk::glib::idle_add_once(move || {
            Self::done_with_ticket(factory);

            let Some(obj) = EntryObject::lookup(&path) else {
                return;
            };

            obj.imp().update_thumbnail(tex, mtime);
        });
    }

    fn fail_thumbnail(factory: SendFactory, path: Arc<Path>, mtime: FileTime) {
        gtk::glib::idle_add_once(move || {
            Self::done_with_ticket(factory);

            let Some(obj) = EntryObject::lookup(&path) else {
                return;
            };

            obj.imp().fail_thumbnail(mtime);
        });
    }

    fn find_next(&self) -> Option<(EntryObject, bool, SendFactory)> {
        let mut pending = self.pending.borrow_mut();

        if pending.factories.is_empty() {
            return None;
        }

        while let Some((weak, from_event)) = pending.high_priority.pop_front() {
            if let Some(strong) = weak.upgrade() {
                if strong.imp().mark_thumbnail_loading(ThumbPriority::High) {
                    pending.processed += 1;
                    return Some((strong, from_event, pending.factories.pop().unwrap()));
                }
            }
        }

        if self.high > self.low && self.high - pending.factories.len() as u16 > self.low {
            return None;
        }

        while let Some((weak, from_event)) = pending.med_priority.pop_front() {
            if let Some(strong) = weak.upgrade() {
                if strong.imp().mark_thumbnail_loading(ThumbPriority::Medium) {
                    pending.processed += 1;
                    return Some((strong, from_event, pending.factories.pop().unwrap()));
                }
            }
        }

        while let Some((weak, from_event)) = pending.low_priority.pop() {
            if let Some(strong) = weak.upgrade() {
                if strong.imp().mark_thumbnail_loading(ThumbPriority::Low) {
                    pending.processed += 1;
                    return Some((strong, from_event, pending.factories.pop().unwrap()));
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

        let Some((obj, from_event, factory)) = self.find_next() else {
            return;
        };


        let entry = obj.get();
        let path = entry.abs_path.clone();
        // It only cares about seconds
        let mtime = obj.get().mtime;
        let mime_type = entry.mime;

        // let start = Instant::now();

        let gen_thumb = move || {
            let uri = match path.canonicalize() {
                Ok(canon) => File::for_path(canon).uri(),
                Err(_e) => File::for_path(&path).uri(),
            };

            // Exceedingly rare for any kind of event-based operation to already have a valid
            // thumbnail, so don't even check. For things like images that can have valid
            // thumbnails while being incomplete, this can be a stale partial thumbnail.
            //
            // Also can't trust mtime to make sense according to wall time or UTC time.
            if !from_event {
                if let Some(existing) = factory.lookup(&uri, mtime.sec) {
                    let gfile = gtk::gio::File::for_path(existing);
                    match Texture::from_file(&gfile) {
                        Ok(tex) => {
                            // This is just too spammy outside of debugging
                            // trace!("Loaded existing thumbnail for {uri:?}");
                            return Self::finish_thumbnail(factory, tex, path, mtime);
                        }
                        Err(e) => {
                            error!("Failed to load existing thumbnail: {e:?}");
                            return Self::fail_thumbnail(factory, path, mtime);
                        }
                    }
                }
            }
            // aw-fm doesn't write failed thumbnails for operations from events,
            // so this is most likely legitimate.
            if factory.has_failed(&uri, mtime.sec) {
                return Self::fail_thumbnail(factory, path, mtime);
            }

            if !factory.can_thumbnail(&uri, mime_type, mtime.sec) {
                // trace!("Marking thumbnail as failed, though it wasn't attempted.");
                return Self::fail_thumbnail(factory, path, mtime);
            }

            match factory.generate_and_save_thumbnail(&uri, mime_type, mtime.sec, from_event) {
                Some(tex) => {
                    // Spammy
                    // trace!(
                    //     "Generated new thumbnail in {:?} for {:?}",
                    //     start.elapsed(),
                    //     path.file_name().unwrap_or(path.as_os_str())
                    // );
                    Self::finish_thumbnail(factory, tex, path, mtime);
                }
                None => Self::fail_thumbnail(factory, path, mtime),
            }
        };

        self.pool.spawn(gen_thumb);
    }

    // This is for dialogs and other places that may grab a very small number of thumbnails.
    // In the worst case, this could be very slow and blocking the UI, but that should be rare.
    //
    // Worst-case we can make this async and parallel as well, but that shouldn't be necessary.
    pub(super) fn sync_thumbnail(&self, p: &Path, mime: &str, mtime: FileTime) -> Option<Texture> {
        let Some(factory) = &self.sync_factory else {
            return None;
        };

        info!("Synchronously loading a thumbnail for {p:?}");

        let uri = File::for_path(p).uri();

        if let Some(existing) = factory.lookup(&uri, mtime.sec as i64) {
            let thumb = gtk::gio::File::for_path(existing);
            match Texture::from_file(&thumb) {
                Ok(tex) => {
                    return Some(tex);
                }
                Err(e) => {
                    error!("Failed to load existing thumbnail: {e:?}");
                    return None;
                }
            }
        }

        if factory.has_valid_failed_thumbnail(&uri, mtime.sec as i64) {
            return None;
        }

        if !factory.can_thumbnail(&uri, mime, mtime.sec as i64) {
            return None;
        }

        generate_and_save_thumbnail(factory, &uri, mime, mtime.sec, false)
    }
}


mod send {

    use gnome_desktop::traits::DesktopThumbnailFactoryExt;
    use gnome_desktop::{DesktopThumbnailFactory, DesktopThumbnailSize};
    use gtk::gdk::Texture;
    use gtk::gio::glib::GString;
    use gtk::glib::ffi::{g_thread_self, GThread};


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
        pub fn make(n: u16) -> (Option<DesktopThumbnailFactory>, Vec<Self>) {
            if n > 0 {
                let f = DesktopThumbnailFactory::new(DesktopThumbnailSize::Normal);
                let mut factories = Vec::with_capacity(n as usize);

                let current_thread = unsafe { g_thread_self() };

                for _ in 0..n {
                    factories.push(Self(f.clone(), current_thread));
                }

                (Some(f), factories)
            } else {
                (None, Vec::new())
            }
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
            from_event: bool,
        ) -> Option<Texture> {
            super::generate_and_save_thumbnail(&self.0, uri, mime_type, mtime_sec, from_event)
        }
    }
}

pub fn generate_and_save_thumbnail(
    factory: &DesktopThumbnailFactory,
    uri: &str,
    mime_type: &str,
    mtime_sec: u64,
    from_event: bool,
) -> Option<Texture> {
    let generated = factory.generate_thumbnail(uri, mime_type, Cancellable::NONE);

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

            error!(
                "Failed to generate thumbnail for {uri:?} ({mime_type}) from_event: {from_event}: \
                 {e}"
            );
            // Don't store failed thumbnails for updates from events, as the second-level
            // precision causes problems. This means it will be retried later, but that's
            // fine.
            if !from_event {
                if let Err(e) =
                    factory.create_failed_thumbnail(uri, mtime_sec as i64, Cancellable::NONE)
                {
                    // Not a serious error for aw-fm, we will still skip trying multiple
                    // times, but it will be retried
                    // unnecessarily in the future.
                    error!("Failed to save failed thumbnail for {uri:?}: {e}");
                }
            }
            return None;
        }
    };

    if let Err(e) = factory.save_thumbnail(&pb, uri, mtime_sec as i64, Cancellable::NONE) {
        error!("Failed to save thumbnail for {uri:?}: {e}");
        // Don't try to save a failed thumbnail here. We can retry in the future.
        return None;
    }

    Some(Texture::for_pixbuf(&pb))
}
