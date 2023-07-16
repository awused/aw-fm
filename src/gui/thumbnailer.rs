use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs::File;
use std::time::Instant;

use ahash::AHashSet;
use gnome_desktop::traits::DesktopThumbnailFactoryExt;
use gnome_desktop::{DesktopThumbnailFactory, DesktopThumbnailSize};
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::gio::{Cancellable, ReadInputStream};
use gtk::glib::{Bytes, WeakRef};
use gtk::prelude::{FileExt, IconExt, ObjectExt};
use old_gio::glib::translate::{IntoGlibPtr, ToGlibPtr};

use super::GUI;
use crate::com::EntryObject;

// How many concurrent thumbnail processes we allow.
static MAX_TICKETS: usize = 4;

#[derive(Debug, Default)]
struct PendingThumbs {
    high_priority: VecDeque<WeakRef<EntryObject>>,
    // For low priority, order doesn't matter.
    low_priority: Vec<WeakRef<EntryObject>>,
}

#[derive(Debug)]
pub struct Thumbnailer {
    pending: RefCell<PendingThumbs>,
    factory: DesktopThumbnailFactory,
    tickets: Cell<usize>,
}

impl Thumbnailer {
    // Would really prefer to do all this in a rayon threadpool entirely under my control.
    // The gnome-desktop maintainer generated the wrapper methods but didn't expose them since
    // there isn't a "v42" feature.
    pub fn new() -> Self {
        let pending = RefCell::default();
        let mut factory = DesktopThumbnailFactory::new(DesktopThumbnailSize::Normal);


        // let a = factory.lookup(f.uri().as_str(), 1682479363);
        // let a = factory.lookup(f.uri().as_str(), 1689497620);
        // let b = factory.can_thumbnail(f.uri().as_str(), "video/mp4", 1689497620);
        // let fact = factory.clone();
        // factory.generate_thumbnail_async(
        //     f.uri().as_str(),
        //     "video/mp4",
        //     None::<&old_gio::Cancellable>,
        //     move |r| {
        //         let p: old_pixbuf::Pixbuf = r.unwrap();
        //
        //         // Makes a copy, annoying but safe
        //         let bytes = Bytes::from(p.pixel_bytes().unwrap().as_ref());
        //
        //         let new_p = Pixbuf::from_bytes(
        //             &bytes,
        //             // There is only one real supported Colourspace,
        //             Colorspace::Rgb,
        //             p.has_alpha(),
        //             p.bits_per_sample(),
        //             p.width(),
        //             p.height(),
        //             p.rowstride(),
        //         );
        //
        //         // let new_p = Pixbuf::from_bytes(
        //         // )
        //         println!("Thumbnail {p:?}");
        //         fact.save_thumbnail_async(
        //             &p,
        //             f.uri().as_str(),
        //             1_689_497_620,
        //             None::<&old_gio::Cancellable>,
        //             |x| {
        //                 println!("{x:?}");
        //             },
        //         )
        //     },
        // );


        Self {
            pending,
            factory,
            tickets: Cell::new(MAX_TICKETS),
        }
    }

    pub fn low_priority(&self, weak: WeakRef<EntryObject>) {
        self.pending.borrow_mut().low_priority.push(weak);
        self.process();
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

    fn finish_thumbnail(pixbuf: Pixbuf, weak: WeakRef<EntryObject>) {
        Self::done_with_ticket();

        let Some(obj) = weak.upgrade() else {
            return;
        };

        obj.update_thumbnail(pixbuf);
    }

    fn fail_thumbnail(weak: WeakRef<EntryObject>) {
        Self::done_with_ticket();
    }

    fn find_next(&self) -> Option<EntryObject> {
        let mut pending = self.pending.borrow_mut();

        while let Some(weak) = pending.high_priority.pop_front() {
            if let Some(strong) = weak.upgrade() {
                if strong.mark_thumbnail_loading() {
                    return Some(strong);
                }
            }
        }

        while let Some(weak) = pending.low_priority.pop() {
            if let Some(strong) = weak.upgrade() {
                if strong.mark_thumbnail_loading() {
                    return Some(strong);
                }
            }
        }

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
        let uri = gtk::gio::File::for_path(&entry.abs_path).uri();
        // It only cares about seconds
        let mtime = obj.get().mtime.sec;
        let mime = entry.mime.clone();
        drop(entry);

        let weak = obj.downgrade();
        drop(obj);

        // TODO -- could move this I/O to another thread.
        // Async moves the actual file reading and decoding off the main thread already.
        // First check, quickly, if there is a valid thumbnail already.
        let start = Instant::now();
        let existing = self.factory.lookup(&uri, mtime as i64);

        if let Some(existing) = existing {
            if let Ok(file) = File::open(&existing) {
                let stream = ReadInputStream::new(file);

                Pixbuf::from_stream_async(&stream, None::<&gtk::gio::Cancellable>, move |pixbuf| {
                    match pixbuf {
                        Ok(pixbuf) => {
                            trace!("Loaded existing thumbnail in {:?}", start.elapsed());
                            Self::finish_thumbnail(pixbuf, weak)
                        }
                        Err(e) => {
                            error!("Loading existing thumbnail failed {e}");
                            Self::fail_thumbnail(weak);
                        }
                    }
                });

                return;
            }
        }


        error!("TODO -- handle missing thumbnail");
    }
}
