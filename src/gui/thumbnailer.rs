use std::cell::{Cell, RefCell};
use std::collections::{btree_map, BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gnome_desktop::{DesktopThumbnailFactory, DesktopThumbnailFactoryExt, DesktopThumbnailSize};
use gstreamer::glib::{ControlFlow, Priority};
use gtk::gdk::Texture;
use gtk::gio::{Cancellable, File};
use gtk::glib::{Quark, WeakRef};
use gtk::prelude::FileExt;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use rayon::{ThreadPool, ThreadPoolBuilder};

use self::send::{Factories, SendFactory};
use super::{gui_run, ThumbPriority};
use crate::com::{EntryObject, FileTime};
use crate::config::CONFIG;
use crate::{closing, handle_panic};


type Ongoing = (Arc<AtomicBool>, Arc<Mutex<()>>);

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
    // This is responsible for cancelling saving thumbnails from earlier events.
    // Also ensures we never try to write the same thumbnail twice at once.
    ongoing: BTreeMap<Arc<Path>, Ongoing>,
    processed: usize,
}

#[derive(Debug)]
pub struct Thumbnailer {
    pending: RefCell<PendingThumbs>,
    // No real advantage to rayon over other pools here, but we already have it as a dependency.
    // Did test a fully glib-async version that uses GTasks under the hood, but the performance
    // wasn't any better and was sometimes much worse.
    pool: ThreadPool,
    // The current thumbnail size, normally calculated from the scale of the main window's surface.
    // It's fine if there's a properties window on a higher DPI monitor that is a bit blurry.
    pub size: Cell<DesktopThumbnailSize>,

    high: u16,
    low: u16,

    sync_factory: Option<Factories>,
}

#[derive(Debug)]
struct Job {
    path: Arc<Path>,
    priority: ThumbPriority,
    from_event: bool,
    mime: &'static str,
    mtime: FileTime,
    size: DesktopThumbnailSize,
    factory: SendFactory,
    // Ensure last-event-wins semantics without needing to take a lock in the main thread
    cancel: Arc<AtomicBool>,
    // We shouldn't have two jobs working on the same thumbnail at once
    lock: Arc<Mutex<()>>,
}


impl PendingThumbs {
    fn prep_job(
        &mut self,
        obj: EntryObject,
        priority: ThumbPriority,
        from_event: bool,
        factory: SendFactory,
        size: DesktopThumbnailSize,
    ) -> Job {
        let entry = obj.get();
        let path = entry.abs_path.clone();
        let mtime = entry.mtime;
        let mime = entry.mime;

        let (cancel, lock) = match self.ongoing.entry(path.clone()) {
            btree_map::Entry::Vacant(v) => v.insert((Arc::default(), Arc::default())),
            btree_map::Entry::Occupied(mut o) => {
                let old = std::mem::take(&mut o.get_mut().0);
                old.store(true, Ordering::Relaxed);
                o.into_mut()
            }
        }
        .clone();

        Job {
            path,
            priority,
            from_event,
            mime,
            mtime,
            size,
            factory,
            cancel,
            lock,
        }
    }
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
            // TODO[thumbsize] read from surface size
            size: Cell::new(DesktopThumbnailSize::Normal),
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

    fn done_with_ticket(factory: SendFactory, path: Arc<Path>) {
        gui_run(|g| {
            let t = &g.thumbnailer;
            {
                let mut pending = t.pending.borrow_mut();
                pending.factories.push(factory);

                // Remove the old value if this was the last event
                if let btree_map::Entry::Occupied(o) = pending.ongoing.entry(path) {
                    if Arc::strong_count(&o.get().1) == 1 {
                        o.remove();
                    } else {
                        warn!(
                            "Did not remove ongoing thumbnail tracker for {:?} as it is still in \
                             use. This should be rare.",
                            o.key()
                        );
                    }
                }
            }

            t.process();
        });
    }

    fn finish_thumbnail(job: Job, tex: Texture) {
        let priority = match job.priority {
            ThumbPriority::Low | ThumbPriority::Medium => Priority::LOW,
            ThumbPriority::High => Priority::DEFAULT_IDLE,
        };

        let mut factory = Some(job.factory);
        let mut path = Some(job.path);
        let mut tex = Some(tex);
        drop(job.lock);

        gtk::glib::idle_add_full(priority, move || {
            let path = path.take().unwrap();
            if let Some(obj) = EntryObject::lookup(&path) {
                obj.imp().update_thumbnail(tex.take().unwrap(), job.mtime, job.size);
            }

            Self::done_with_ticket(factory.take().unwrap(), path);

            ControlFlow::Break
        });
    }

    fn fail_thumbnail(job: Job) {
        let mut factory = Some(job.factory);
        let mut path = Some(job.path);
        drop(job.lock);

        // Must be DEFAULT_IDLE so that these never lose a race with finish_thumbnail calls.
        // While that should be nearly impossible, failed thumbnails should be rare.
        gtk::glib::idle_add_full(Priority::DEFAULT_IDLE, move || {
            let path = path.take().unwrap();
            if let Some(obj) = EntryObject::lookup(&path) {
                obj.imp().fail_thumbnail(job.mtime);
            }

            Self::done_with_ticket(factory.take().unwrap(), path);

            ControlFlow::Break
        });
    }

    // Only happens when a later queued thumbnail has superseded this one
    fn abandon_thumbnail(job: Job) {
        let mut factory = Some(job.factory);
        let mut path = Some(job.path);
        drop(job.lock);

        gtk::glib::idle_add_full(Priority::LOW, move || {
            Self::done_with_ticket(factory.take().unwrap(), path.take().unwrap());

            ControlFlow::Break
        });
    }

    fn find_next(&self) -> Option<Job> {
        let mut pending = self.pending.borrow_mut();

        if pending.factories.is_empty() {
            return None;
        }
        let size = self.size.get();

        let job = 'find_job: {
            while let Some((weak, from_event)) = pending.high_priority.pop_front() {
                if let Some(strong) = weak.upgrade() {
                    if strong.imp().mark_thumbnail_loading(ThumbPriority::High, size) {
                        break 'find_job Some((strong, ThumbPriority::High, from_event));
                    }
                }
            }

            if self.high > self.low && self.high - pending.factories.len() as u16 > self.low {
                break 'find_job None;
            }

            while let Some((weak, from_event)) = pending.med_priority.pop_front() {
                if let Some(strong) = weak.upgrade() {
                    if strong.imp().mark_thumbnail_loading(ThumbPriority::Medium, size) {
                        break 'find_job Some((strong, ThumbPriority::Medium, from_event));
                    }
                }
            }

            while let Some((weak, from_event)) = pending.low_priority.pop() {
                if let Some(strong) = weak.upgrade() {
                    if strong.imp().mark_thumbnail_loading(ThumbPriority::Low, size) {
                        break 'find_job Some((strong, ThumbPriority::Low, from_event));
                    }
                }
            }

            None
        };

        if let Some((strong, priority, from_event)) = job {
            pending.processed += 1;
            let factory = pending.factories.pop().unwrap();
            return Some(pending.prep_job(strong, priority, from_event, factory, size));
        }

        if pending.factories.len() < self.high as usize {
            // Wait until all processing is done.
            return None;
        }

        if pending.processed > 32 {
            // Don't spam this if it's only for only a few updates.
            debug!("Finished loading all thumbnails (none pending).");
        }

        assert!(pending.ongoing.is_empty());
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

        let Some(job) = self.find_next() else {
            return;
        };


        self.pool.spawn(move || {
            let guard = job.lock.lock().unwrap();
            if job.cancel.load(Ordering::Relaxed) {
                debug!("Cancelled thumbnail job for {:?} immediately after queueing it", job.path);
                drop(guard);
                return Self::abandon_thumbnail(job);
            }

            let uri = match job.path.canonicalize() {
                Ok(canon) => File::for_path(canon).uri(),
                Err(_e) => File::for_path(&job.path).uri(),
            };

            // Exceedingly rare for any kind of event-based operation to already have a valid
            // thumbnail, so don't even check. For things like images that can have valid
            // thumbnails while being incomplete, this can be a stale partial thumbnail.
            //
            // Also can't trust mtime to make sense according to wall time or UTC time.
            if !job.from_event {
                if let Some(existing) = job.lookup(&uri) {
                    let gfile = gtk::gio::File::for_path(existing);
                    match Texture::from_file(&gfile) {
                        Ok(tex) => {
                            // This is just too spammy outside of debugging
                            // trace!("Loaded existing thumbnail for {uri:?}");
                            drop(guard);
                            return Self::finish_thumbnail(job, tex);
                        }
                        Err(e) => {
                            error!("Failed to load existing thumbnail: {e:?}");
                            drop(guard);
                            return Self::fail_thumbnail(job);
                        }
                    }
                }
            }

            // aw-fm doesn't write failed thumbnails for operations from events,
            // so this is most likely legitimate.
            if job.has_failed(&uri) {
                drop(guard);
                return Self::fail_thumbnail(job);
            }

            if !job.can_thumbnail(&uri) {
                // trace!("Marking thumbnail as failed, though it wasn't attempted.");
                drop(guard);
                return Self::fail_thumbnail(job);
            }

            #[allow(clippy::branches_sharing_code)]
            if let Some(tex) = job.generate_and_save_thumbnail(&uri) {
                // Spammy
                // trace!(
                //     "Generated new thumbnail in {:?} for {:?}",
                //     start.elapsed(),
                //     path.file_name().unwrap_or(path.as_os_str())
                // );
                drop(guard);
                Self::finish_thumbnail(job, tex);
            } else {
                drop(guard);
                Self::fail_thumbnail(job);
            }
        });
    }

    // This is for dialogs and other places that may grab a very small number of thumbnails.
    // In the worst case, this could be very slow and blocking the UI, but that should be rare.
    //
    // Worst-case we can make this async and parallel as well, but that shouldn't be necessary.
    pub(super) fn sync_thumbnail(&self, p: &Path, mime: &str, mtime: FileTime) -> Option<Texture> {
        let factory = self.sync_factory.as_ref()?;

        let factory = factory.get(self.size.get());

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

        // Treat this as a cancelled background job and don't write it.
        // Not worth bothering to check pending.ongoing here.
        generate_and_save_thumbnail(factory, &uri, mime, mtime.sec, false, &AtomicBool::new(true))
    }
}


mod send {
    use gnome_desktop::traits::DesktopThumbnailFactoryExt;
    use gnome_desktop::{DesktopThumbnailFactory, DesktopThumbnailSize};
    use gtk::gdk::Texture;
    use gtk::gio::glib::GString;
    use gtk::glib::ffi::{g_thread_self, GThread};

    use super::Job;

    #[derive(Debug, Clone)]
    pub(super) struct Factories(DesktopThumbnailFactory, DesktopThumbnailFactory);

    impl Factories {
        pub(super) const fn get(&self, size: DesktopThumbnailSize) -> &DesktopThumbnailFactory {
            match size {
                DesktopThumbnailSize::Normal => &self.0,
                DesktopThumbnailSize::Large
                | DesktopThumbnailSize::Xlarge
                | DesktopThumbnailSize::Xxlarge => &self.1,
                _ => unreachable!(),
            }
        }
    }

    #[derive(Debug)]
    pub(super) struct SendFactory(Factories, *mut GThread);

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
        pub fn make(n: u16) -> (Option<Factories>, Vec<Self>) {
            if n > 0 {
                let f = Factories(
                    DesktopThumbnailFactory::new(DesktopThumbnailSize::Normal),
                    DesktopThumbnailFactory::new(DesktopThumbnailSize::Large),
                );

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
    }

    impl Job {
        pub fn lookup(&self, uri: &str) -> Option<GString> {
            self.factory.0.get(self.size).lookup(uri, self.mtime.sec as i64)
        }

        pub fn has_failed(&self, uri: &str) -> bool {
            self.factory
                .0
                .get(self.size)
                .has_valid_failed_thumbnail(uri, self.mtime.sec as i64)
        }

        pub fn can_thumbnail(&self, uri: &str) -> bool {
            self.factory
                .0
                .get(self.size)
                .can_thumbnail(uri, self.mime, self.mtime.sec as i64)
        }

        // It would be faster for the UI to set the thumbnail first and then go to save it.
        // Given how small and simple these files are, and the weird cases that could happen if we
        // fail to save a thumbnail after setting it, it's just not worth it.
        pub fn generate_and_save_thumbnail(&self, uri: &str) -> Option<Texture> {
            super::generate_and_save_thumbnail(
                self.factory.0.get(self.size),
                uri,
                self.mime,
                self.mtime.sec,
                self.from_event,
                &self.cancel,
            )
        }
    }
}

pub fn generate_and_save_thumbnail(
    factory: &DesktopThumbnailFactory,
    uri: &str,
    mime_type: &str,
    mtime_sec: u64,
    from_event: bool,
    cancel_save: &AtomicBool,
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
            if !from_event && !cancel_save.load(Ordering::Relaxed) {
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

    // Avoid saving successful thumbnails if a newer event has come in.
    // This prevents the case where mtime_sec is the same between two events but the earlier one
    // finishes a successful but incomplete thumbnail after the later one.
    if !cancel_save.load(Ordering::Relaxed) {
        if let Err(e) = factory.save_thumbnail(&pb, uri, mtime_sec as i64, Cancellable::NONE) {
            error!("Failed to save thumbnail for {uri:?}: {e}");
            // Don't try to save a failed thumbnail here. We can retry in the future.
            return None;
        }
    } else {
        warn!("Cancelled saving thumbnail for {uri:?} as a new update has arrived");
    }

    Some(Texture::for_pixbuf(&pb))
}
