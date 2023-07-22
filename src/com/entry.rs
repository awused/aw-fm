use std::borrow::BorrowMut;
use std::cell::{Ref, RefCell};
use std::cmp::Ordering;
use std::collections::hash_map;
use std::fmt::{self, Formatter};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ahash::AHashMap;
use gtk::gdk::Texture;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::gio::ffi::G_FILE_TYPE_DIRECTORY;
use gtk::gio::{
    self, Cancellable, FileInfo, FileQueryInfoFlags, Icon, FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE,
    FILE_ATTRIBUTE_STANDARD_FAST_CONTENT_TYPE, FILE_ATTRIBUTE_STANDARD_ICON,
    FILE_ATTRIBUTE_STANDARD_IS_SYMLINK, FILE_ATTRIBUTE_STANDARD_SIZE, FILE_ATTRIBUTE_STANDARD_TYPE,
    FILE_ATTRIBUTE_TIME_CREATED, FILE_ATTRIBUTE_TIME_CREATED_USEC, FILE_ATTRIBUTE_TIME_MODIFIED,
    FILE_ATTRIBUTE_TIME_MODIFIED_USEC,
};
use gtk::glib::subclass::Signal;
use gtk::glib::{self, GStr, Object, Variant, WeakRef};
use gtk::prelude::{FileExt, IconExt, ObjectExt};
use gtk::subclass::prelude::{ObjectImpl, ObjectSubclass, ObjectSubclassIsExt};
use once_cell::sync::Lazy;

use super::{DirSettings, SortDir, SortMode, SortSettings};
use crate::gui::{queue_high_priority_thumb, queue_low_priority_thumb};
use crate::natsort::{self, ParsedString};

// In theory could use standard::edit-name and standard::display-name instead of taking
// those from the path. In practice none of my own files are that broken.
static ATTRIBUTES: Lazy<String> = Lazy::new(|| {
    [
        FILE_ATTRIBUTE_STANDARD_TYPE,
        // FAST_CONTENT_TYPE doesn't sniff mimetypes, but even getting the icon involves getting
        // the slow content type.
        FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE,
        FILE_ATTRIBUTE_STANDARD_ICON,
        FILE_ATTRIBUTE_STANDARD_IS_SYMLINK,
        FILE_ATTRIBUTE_STANDARD_SIZE,
        FILE_ATTRIBUTE_TIME_MODIFIED,
        FILE_ATTRIBUTE_TIME_MODIFIED_USEC,
        // FILE_ATTRIBUTE_TIME_CREATED,
        // FILE_ATTRIBUTE_TIME_CREATED_USEC,
    ]
    .map(GStr::as_str)
    .join(",")
});

#[derive(Eq, PartialEq, Default, Clone, Copy)]
pub struct FileTime {
    pub sec: u64,
    pub usec: u32,
}

impl Ord for FileTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sec.cmp(&other.sec).then(self.usec.cmp(&other.usec))
    }
}

impl PartialOrd for FileTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for FileTime {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "time: {}, {}", self.sec, self.usec)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File { size: u64 },
    Directory { contents: u64 },
    Uninitialized, // Broken {}
}


// Does NOT implement Clone so we can rely on GObject refcounting to minimize copies.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    // This is an absolute but NOT canonicalized path.
    pub abs_path: Arc<Path>,
    // It's kind of expensive to do this but necessary as an mtime/ctime tiebreaker anyway.
    // TODO -- is this really name, or should it be rel_path for searching
    pub name: ParsedString,
    pub mtime: FileTime,

    // Doesn't work over NFS, could fall back to "changed" time but that's not what we really
    // want. Given how I use my NAS this just isn't useful right now.
    // pub btime: FileTime,

    // TODO -- Arc<> or some other mechanism for interning them, otherwise this is a large
    // number of wasted tiny allocations.
    // Could do String::into_boxed_str()::leak() to get &'static str
    pub mime: String,
    pub symlink: bool,
    // pub info: String,
    pub icon: Variant,
}

// Put methods in here only when they are used alongside multiple other fields on the Entry object.
// Otherwise put them on Wrapper or EntryObject
impl Entry {
    // If this ever changes so that we don't use all other options as potential tiebreakers in all
    // cases, the "needs_resort" logic in EntryObject::update below will need updating.
    pub fn cmp(&self, other: &Self, settings: SortSettings) -> Ordering {
        use EntryKind::*;

        let size_order = match (&self.kind, &other.kind) {
            (Uninitialized, _) | (_, Uninitialized) => unreachable!(),
            (File { .. }, Directory { .. }) => return Ordering::Greater,
            (Directory { .. }, File { .. }) => return Ordering::Less,
            (File { size: self_size }, File { size: other_size })
            | (Directory { contents: self_size }, Directory { contents: other_size }) => {
                self_size.cmp(other_size)
            }
        };

        let m_order = self.mtime.cmp(&other.mtime);
        // let b_order = self.btime.cmp(&other.btime);
        let name_order = || self.name.cmp(&other.name);

        // Use the other options as tie breakers, with the abs_path as a final tiebreaker.
        let ordering = match settings.mode {
            SortMode::Name => name_order().then(m_order)/*.then(b_order)*/.then(size_order),
            SortMode::MTime => m_order/*.then(b_order)*/.then_with(name_order).then(size_order),
            SortMode::Size => size_order.then_with(name_order).then(m_order)/*.then(b_order)*/,
            // SortMode::BTime => b_order.then(m_order).then_with(name_order).then(size_order),
        }
        .then_with(|| self.abs_path.cmp(&other.abs_path));

        if settings.direction == SortDir::Ascending {
            ordering
        } else {
            ordering.reverse()
        }
    }

    pub fn new(abs_path: Arc<Path>) -> Result<Self, (Arc<Path>, gtk::glib::Error)> {
        debug_assert!(abs_path.is_absolute());

        let name = abs_path.file_name().unwrap_or(abs_path.as_os_str());
        let name = natsort::key(name);

        let info = match gio::File::for_path(&abs_path).query_info(
            ATTRIBUTES.as_str(),
            FileQueryInfoFlags::empty(),
            Option::<&Cancellable>::None,
        ) {
            Ok(info) => info,
            Err(e) => return Err((abs_path, e)),
        };

        let mtime = FileTime {
            sec: info.attribute_uint64(FILE_ATTRIBUTE_TIME_MODIFIED),
            usec: info.attribute_uint32(FILE_ATTRIBUTE_TIME_MODIFIED_USEC),
        };

        // let btime = FileTime {
        //     sec: info.attribute_uint64(FILE_ATTRIBUTE_TIME_CREATED),
        //     usec: info.attribute_uint32(FILE_ATTRIBUTE_TIME_CREATED_USEC),
        // };

        let mime = info.attribute_string(FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE).unwrap().to_string();

        let size = info.attribute_uint64(FILE_ATTRIBUTE_STANDARD_SIZE);

        let file_type = info.attribute_uint32(FILE_ATTRIBUTE_STANDARD_TYPE);
        let kind = if file_type == G_FILE_TYPE_DIRECTORY as u32 {
            // I think this is counting "." and ".." as members.
            EntryKind::Directory { contents: size.saturating_sub(2) }
        } else {
            EntryKind::File { size }
        };


        let icon = info.icon().unwrap().serialize().unwrap();
        let symlink = info.is_symlink();

        Ok(Self {
            kind,
            abs_path,
            name,
            mtime,
            // btime,
            mime,
            symlink,
            icon,
        })
    }

    // This could be a compact string/small string but possibly not worth the dependency on its own
    pub fn short_size_string(&self) -> String {
        match self.kind {
            EntryKind::File { size } => humansize::format_size(size, humansize::WINDOWS),
            EntryKind::Directory { contents } => format!("{contents}"),
            EntryKind::Uninitialized => unreachable!(),
        }
    }

    pub fn long_size_string(&self) -> String {
        match self.kind {
            EntryKind::File { size } => humansize::format_size(size, humansize::WINDOWS),
            EntryKind::Directory { contents } => format!("{contents} items"),
            EntryKind::Uninitialized => unreachable!(),
        }
    }

    pub const fn dir(&self) -> bool {
        matches!(self.kind, EntryKind::Directory { .. })
    }

    pub const fn raw_size(&self) -> u64 {
        match self.kind {
            EntryKind::File { size } => size,
            EntryKind::Directory { contents } => contents,
            EntryKind::Uninitialized => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub enum Thumbnail {
    Nothing,
    LowPriority,
    HighPriority(u8),
    // Only set by the thumbnailer when it starts to load the thumbnail for a file.
    // This is to avoid race conditions between an update being handled and a second update to the
    // file.
    Loading,
    // Loaded(Pixbuf),
    Loaded(Texture),
    // Outdated(Pixbuf),
    // Can do two-pass unloading for all images.
    // Only marginally useful compared to completely unloading tabs.
    // Unloaded(WeakRef<Pixbuf>),
    Failed,
}


// All the ugly GTK wrapper code below this

mod internal {
    use std::cell::{OnceCell, Ref, RefCell};
    use std::ops::Deref;
    use std::time::Instant;

    use gtk::gdk::Texture;
    use gtk::gdk_pixbuf::Pixbuf;
    use gtk::gio::Icon;
    use gtk::glib;
    use gtk::glib::subclass::Signal;
    use gtk::prelude::ObjectExt;
    use gtk::subclass::prelude::{ObjectImpl, ObjectSubclass, ObjectSubclassExt};
    use once_cell::sync::Lazy;

    use super::{Thumbnail, ALL_ENTRY_OBJECTS};
    use crate::com::EntryKind;

    #[derive(Debug)]
    struct Entry {
        entry: super::Entry,
        thumbnail: Thumbnail,
    }

    #[derive(Debug, Default)]
    pub struct EntryWrapper(RefCell<Option<Entry>>);


    #[glib::object_subclass]
    impl ObjectSubclass for EntryWrapper {
        type Type = super::EntryObject;

        const NAME: &'static str = "aw-fm-Entry";
    }

    impl ObjectImpl for EntryWrapper {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: Lazy<Vec<Signal>> =
                Lazy::new(|| vec![Signal::builder("update").build()]);
            SIGNALS.as_ref()
        }

        fn dispose(&self) {
            // Too spammy
            // trace!("EntryObject disposed");
            let path = &self.get().abs_path;
            // Could check the load factor and shrink the map.
            // dispose can, technically, be called multiple times, so unsafe to assert here.
            ALL_ENTRY_OBJECTS.with(|m| m.borrow_mut().remove(path));
        }
    }

    // Might be worth redoing this and moving more logic down into EntryObject.
    // These should not be Entry methods since they don't make sense for a bare Entry.
    impl EntryWrapper {
        pub(super) fn init(&self, entry: super::Entry) {
            let thumbnail = match entry.kind {
                EntryKind::File { .. } => Thumbnail::LowPriority,
                EntryKind::Directory { .. } => Thumbnail::Nothing,
                EntryKind::Uninitialized => unreachable!(),
            };

            // TODO -- other mechanisms to set thumbnails as Nothing, like mimetype or somesuch

            let wrapped = Entry { entry, thumbnail };
            assert!(self.0.replace(Some(wrapped)).is_none());
        }

        pub(super) fn update_inner(&self, mut entry: super::Entry) -> super::Entry {
            // TODO -- this is where we'd be able to use "Outdated"
            // Every time there is an update, we have an opportunity to try the thumbnail again if
            // it failed.

            let thumbnail = match entry.kind {
                EntryKind::File { .. } => Thumbnail::LowPriority,
                EntryKind::Directory { .. } => Thumbnail::Nothing,
                EntryKind::Uninitialized => unreachable!(),
            };

            // Since this is used as the key in the hash map, if we use the new Arc<Path> we'll
            // double memory usage for no reason.
            {
                let old = self.0.borrow();
                let old_arc = old.as_ref().unwrap().entry.abs_path.clone();
                entry.abs_path = old_arc;
            }

            let wrapped = Entry { entry, thumbnail };
            let old = self.0.replace(Some(wrapped)).unwrap();
            self.obj().emit_by_name::<()>("update", &[]);
            old.entry
        }

        pub(super) fn get(&self) -> Ref<super::Entry> {
            let b = self.0.borrow();
            Ref::map(b, |o| &o.as_ref().unwrap().entry)
        }

        pub(super) fn thumbnail(&self) -> Ref<Thumbnail> {
            let b = self.0.borrow();
            Ref::map(b, |o| &o.as_ref().unwrap().thumbnail)
        }

        pub(super) fn mark_thumbnail_loading_high(&self) -> bool {
            let mut b = self.0.borrow_mut();
            let mut thumb = &mut b.as_mut().unwrap().thumbnail;

            if matches!(thumb, Thumbnail::HighPriority(_)) {
                *thumb = Thumbnail::Loading;
                return true;
            }
            false
        }

        pub(super) fn mark_thumbnail_loading_low(&self) -> bool {
            let mut b = self.0.borrow_mut();
            let mut thumb = &mut b.as_mut().unwrap().thumbnail;

            if matches!(thumb, Thumbnail::LowPriority) {
                *thumb = Thumbnail::Loading;
                return true;
            }
            false
        }

        pub(super) fn deprioritize_thumb(&self) -> bool {
            let mut b = self.0.borrow_mut();
            let mut thumb = &mut b.as_mut().unwrap().thumbnail;
            match &mut thumb {
                Thumbnail::HighPriority(1) => {
                    *thumb = Thumbnail::LowPriority;
                    true
                }
                Thumbnail::HighPriority(n) => {
                    *n -= 1;
                    false
                }
                Thumbnail::Nothing
                | Thumbnail::Loading
                | Thumbnail::Loaded(_)
                | Thumbnail::Failed
                | Thumbnail::LowPriority => false,
            }
        }

        // There is a minute risk of a race where we're loading a thumbnail for a file twice at
        // once and the first one finishes second. The risk is so low and the outcome so minor it
        // just isn't worth addressing.
        pub(super) fn update_thumbnail(&self, tex: Texture) {
            let mut b = self.0.borrow_mut();
            let thumb = &mut b.as_mut().unwrap().thumbnail;

            match thumb {
                Thumbnail::Nothing
                | Thumbnail::LowPriority
                | Thumbnail::HighPriority(_)
                | Thumbnail::Failed => return,
                Thumbnail::Loading | Thumbnail::Loaded(_) => {
                    *thumb = Thumbnail::Loaded(tex);
                }
            }
            drop(b);
            let start = Instant::now();
            self.obj().emit_by_name::<()>("update", &[]);
        }

        pub(super) fn should_request_low_priority_thumb(&self) -> bool {
            let mut b = self.0.borrow();
            let thumb = &b.as_ref().unwrap().thumbnail;

            matches!(thumb, Thumbnail::LowPriority)
        }

        pub(super) fn high_priority_thumb(&self) -> (bool, Option<Texture>) {
            let mut b = self.0.borrow_mut();
            let thumb = &mut b.as_mut().unwrap().thumbnail;
            match thumb {
                Thumbnail::LowPriority => {
                    *thumb = Thumbnail::HighPriority(1);
                    (true, None)
                }
                Thumbnail::HighPriority(n) => {
                    // This can actually increment it multiple times from update() calls, but
                    // the worst case there is that we keep it in the high priority queue.
                    *n += 1;
                    (false, None)
                }
                Thumbnail::Loaded(pb) => (false, Some(pb.clone())),
                Thumbnail::Nothing | Thumbnail::Failed | Thumbnail::Loading => (false, None),
            }
        }
    }
}

// This does burn a bit of memory, but it avoids any costly searches on updates and insertions.
thread_local! {
    static ALL_ENTRY_OBJECTS: RefCell<AHashMap<Arc<Path>, WeakRef<EntryObject>>> =
        AHashMap::new().into();
}

glib::wrapper! {
    pub struct EntryObject(ObjectSubclass<internal::EntryWrapper>);
}

impl EntryObject {
    fn create(entry: Entry) -> Self {
        let obj: Self = Object::new();
        obj.imp().init(entry);

        if obj.imp().should_request_low_priority_thumb() {
            queue_low_priority_thumb(obj.downgrade())
        }

        obj
    }

    pub fn new(entry: Entry) -> Self {
        let obj = Self::create(entry);

        ALL_ENTRY_OBJECTS.with(|m| {
            let old = m.borrow_mut().insert(obj.get().abs_path.clone(), obj.downgrade());
            assert!(old.is_none());
        });

        obj
    }

    // If the old entry is present, we have existing search tabs we'll need to update.
    //
    // This cannot cause updates to existing non-search lists.
    pub fn create_or_update(entry: Entry) -> (Self, Option<Entry>) {
        ALL_ENTRY_OBJECTS.with(|m| {
            let mut map = m.borrow_mut();

            match map.entry(entry.abs_path.clone()) {
                // We update it here if it's different to avoid the case where this is a stale
                // entry from a search tab keeping a stale reference up to date, or a user is
                // refreshing a remote directory with a search open.
                hash_map::Entry::Occupied(o) => {
                    let value = o.into_mut();
                    if let Some(existing) = value.upgrade() {
                        return (existing.clone(), existing.update(entry));
                    }

                    warn!("Got dangling WeakRef in EntryObject::create_or_update");
                    let new = Self::create(entry);
                    *value = new.downgrade();
                    (new, None)
                }
                hash_map::Entry::Vacant(v) => {
                    let new = Self::create(entry);
                    v.insert(new.downgrade());
                    (new, None)
                }
            }
        })
    }

    pub fn lookup(path: &Path) -> Option<Self> {
        ALL_ENTRY_OBJECTS.with(|m| m.borrow().get(path).and_then(WeakRef::upgrade))
    }

    // Returns the old value only if an update happened.
    pub fn update(&self, entry: Entry) -> Option<Entry> {
        let old = self.imp().get();
        assert!(old.abs_path == entry.abs_path);
        if *old == entry {
            // No change we care about
            trace!("Update for {:?} was unimportant", entry.abs_path);
            return None;
        }

        drop(old);

        trace!("Update for {:?}", entry.abs_path);

        let old = self.imp().update_inner(entry);

        if self.imp().should_request_low_priority_thumb() {
            queue_low_priority_thumb(self.downgrade())
        }

        Some(old)
    }

    pub(super) fn cmp(&self, other: &Self, settings: SortSettings) -> Ordering {
        self.imp().get().cmp(&other.imp().get(), settings)
    }

    pub fn get(&self) -> Ref<'_, Entry> {
        self.imp().get()
    }

    // Gets the thumbnail. If the thumbnail has been requested at a low priority, bumps it to a
    // high priority.
    pub fn thumbnail_for_display(&self) -> Option<Texture> {
        let (was_low, tex) = self.imp().high_priority_thumb();
        if was_low {
            queue_high_priority_thumb(self.downgrade());
        }
        tex
    }

    pub fn deprioritize_thumb(&self) {
        let became_low = self.imp().deprioritize_thumb();
        if became_low {
            // This is probably unnecessary.
            queue_low_priority_thumb(self.downgrade());
        }
    }

    pub fn mark_thumbnail_loading_high(&self) -> bool {
        self.imp().mark_thumbnail_loading_high()
    }

    pub fn mark_thumbnail_loading_low(&self) -> bool {
        self.imp().mark_thumbnail_loading_low()
    }

    pub fn update_thumbnail(&self, tex: Texture) {
        self.imp().update_thumbnail(tex)
    }

    pub fn icon(&self) -> Icon {
        Icon::deserialize(&self.get().icon).unwrap()
    }
}
