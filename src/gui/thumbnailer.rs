use std::cell::RefCell;
use std::collections::VecDeque;

use ahash::AHashSet;
use gnome_desktop::{DesktopThumbnailFactory, DesktopThumbnailSize};
use gtk::gdk_pixbuf::Pixbuf;
use gtk::glib::WeakRef;

use crate::com::EntryObject;


#[derive(Debug, Default)]
struct PendingThumbs {
    high_priority: VecDeque<WeakRef<EntryObject>>,
    low_priority: Vec<WeakRef<EntryObject>>,
}

#[derive(Debug)]
pub struct Thumbnailer {
    pending: RefCell<PendingThumbs>,
    factory: DesktopThumbnailFactory,
}

impl Thumbnailer {
    pub fn new() -> Self {
        let pending = RefCell::default();
        let factory = DesktopThumbnailFactory::new(DesktopThumbnailSize::Normal);


        Self { pending, factory }
    }
}
