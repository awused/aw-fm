use std::borrow::Borrow;
use std::cell::{Ref, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, btree_map, hash_map};
use std::fmt::{self, Formatter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use ahash::AHashMap;
use chrono::{Local, TimeZone};
use gnome_desktop::DesktopThumbnailSize;
use gtk::gdk::Texture;
use gtk::gio::ffi::G_FILE_TYPE_DIRECTORY;
use gtk::gio::{
    self, Cancellable, FILE_ATTRIBUTE_ACCESS_CAN_EXECUTE, FILE_ATTRIBUTE_STANDARD_ALLOCATED_SIZE,
    FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE, FILE_ATTRIBUTE_STANDARD_ICON,
    FILE_ATTRIBUTE_STANDARD_IS_SYMLINK, FILE_ATTRIBUTE_STANDARD_SIZE,
    FILE_ATTRIBUTE_STANDARD_SYMLINK_TARGET, FILE_ATTRIBUTE_STANDARD_TYPE,
    FILE_ATTRIBUTE_TIME_MODIFIED, FILE_ATTRIBUTE_TIME_MODIFIED_USEC, FILE_ATTRIBUTE_UNIX_INODE,
    FILE_ATTRIBUTE_UNIX_IS_MOUNTPOINT, FileQueryInfoFlags, Icon,
};
use gtk::glib::ffi::GVariant;
use gtk::glib::{self, GStr, GString, Object, Variant, WeakRef};
use gtk::prelude::{FileExt, IconExt, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;

use super::{SortDir, SortMode, SortSettings};
use crate::gui::{ThumbPriority, queue_thumb};
use crate::natsort::{self, NatKey};


// In theory could use standard::edit-name and standard::display-name instead of taking
// those from the path. In practice none of my own files are that broken.
static ATTRIBUTES: LazyLock<String> = LazyLock::new(|| {
    [
        FILE_ATTRIBUTE_STANDARD_TYPE,
        // FAST_CONTENT_TYPE doesn't sniff mimetypes, but even getting the icon involves getting
        // the slow content type.
        FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE,
        FILE_ATTRIBUTE_STANDARD_ICON,
        FILE_ATTRIBUTE_STANDARD_IS_SYMLINK,
        FILE_ATTRIBUTE_STANDARD_SYMLINK_TARGET,
        FILE_ATTRIBUTE_STANDARD_SIZE,
        FILE_ATTRIBUTE_STANDARD_ALLOCATED_SIZE,
        FILE_ATTRIBUTE_TIME_MODIFIED,
        FILE_ATTRIBUTE_TIME_MODIFIED_USEC,
        FILE_ATTRIBUTE_UNIX_IS_MOUNTPOINT,
        FILE_ATTRIBUTE_ACCESS_CAN_EXECUTE,
        FILE_ATTRIBUTE_UNIX_INODE,
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

impl FileTime {
    pub fn seconds_string(self) -> String {
        let localtime = Local.timestamp_opt(self.sec as i64, 0).unwrap();
        let text = localtime.format("%Y-%m-%d %H:%M:%S");
        format!("{text}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File { size: u64, executable: bool },
    Directory { contents: Option<u64> },
    Uninitialized,
}


// Does NOT implement Clone so we can rely on GObject refcounting to minimize copies.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    pub allocated_size: u64,

    // This is an absolute but NOT canonicalized path.
    pub abs_path: Arc<Path>,
    // It's kind of expensive to do this but necessary as an mtime/ctime tiebreaker anyway.
    pub name: NatKey,
    pub mtime: FileTime,

    // Doesn't work over NFS, could fall back to "changed" time but that's not what we really
    // want. Given how I use my NAS this just isn't useful right now.
    // pub btime: FileTime,
    pub mime: &'static str,
    pub symlink: Option<PathBuf>,
    pub icon: Variant,

    // Used for detecting silly renames
    pub inode: u64,
}

pub trait GetEntry {
    fn get_entry(self) -> Entry;
}

impl GetEntry for Entry {
    fn get_entry(self) -> Self {
        self
    }
}

impl GetEntry for Arc<Entry> {
    // In the vast majority of cases we'll only have one instance.
    // This is to avoid needing to read the file twice for overlapping updates.
    fn get_entry(self) -> Entry {
        Self::try_unwrap(self).unwrap_or_else(|e| e.clone_inner())
    }
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
            (File { size: self_size, .. }, File { size: other_size, .. })
            | (Directory { contents: Some(self_size) }, Directory { contents: Some(other_size) }) => {
                self_size.cmp(other_size)
            }
            (Directory { contents: None }, Directory { contents: Some(_) }) => Ordering::Less,
            (Directory { contents: Some(_) }, Directory { contents: None }) => Ordering::Greater,
            (Directory { contents: None }, Directory { contents: None }) => Ordering::Equal,
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

    pub fn new(abs_path: Arc<Path>) -> Result<(Self, bool), (Arc<Path>, gtk::glib::Error)> {
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

        let mime = info.attribute_string(FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE).unwrap();

        let mime = intern_mimetype(mime);

        let size = info.attribute_uint64(FILE_ATTRIBUTE_STANDARD_SIZE);
        let allocated_size = info.attribute_uint64(FILE_ATTRIBUTE_STANDARD_ALLOCATED_SIZE);

        let file_type = info.attribute_uint32(FILE_ATTRIBUTE_STANDARD_TYPE);
        let mut needs_full_count = false;

        let kind = if file_type == G_FILE_TYPE_DIRECTORY as u32 {
            let mountpoint = info.boolean(FILE_ATTRIBUTE_UNIX_IS_MOUNTPOINT);

            if allocated_size == 0 || size == allocated_size || (mountpoint && size <= 2) {
                info!(
                    "Got suspicious directory size {size} for {abs_path:?}, counting files \
                     directly"
                );
                // This is suspicious, and we should count the contents directly.
                needs_full_count = true;
                EntryKind::Directory { contents: None }
            } else {
                // I think this is counting "." and ".." as members.
                EntryKind::Directory { contents: Some(size.saturating_sub(2)) }
            }
        } else {
            EntryKind::File {
                size,
                executable: info.boolean(FILE_ATTRIBUTE_ACCESS_CAN_EXECUTE),
            }
        };


        let symlink = if info.is_symlink() { info.symlink_target() } else { None };
        let icon = intern_icon(info.icon().unwrap());

        let inode = info.attribute_uint64(FILE_ATTRIBUTE_UNIX_INODE);

        Ok((
            Self {
                kind,
                allocated_size,
                abs_path,
                name,
                mtime,
                // btime,
                mime,
                symlink,
                icon,
                inode,
            },
            needs_full_count,
        ))
    }

    pub fn new_assume_dir_size(abs_path: Arc<Path>, size: u64) -> Option<Self> {
        let res = Self::new(abs_path);

        let mut s = match res {
            Ok((entry, _)) => entry,
            Err((abs_path, e)) => {
                error!("Failed to recreate directory Entry for {abs_path:?}: {e}");
                return None;
            }
        };

        let EntryKind::Directory { contents } = &mut s.kind else {
            warn!(
                "Directory {:?} stopped being a directory after we counted its contents",
                s.abs_path
            );
            return None;
        };

        *contents = Some(size);
        Some(s)
    }

    // This could be a compact string/small string but possibly not worth the dependency on its own
    pub fn short_size_string(&self) -> String {
        match self.kind {
            EntryKind::File { size, .. } => humansize::format_size(size, humansize::WINDOWS),
            EntryKind::Directory { contents: Some(contents) } => format!("{contents}"),
            EntryKind::Directory { contents: None } => "...".into(),
            EntryKind::Uninitialized => unreachable!(),
        }
    }

    pub fn long_size_string(&self) -> String {
        match self.kind {
            EntryKind::File { size, .. } => humansize::format_size(size, humansize::WINDOWS),
            EntryKind::Directory { contents: Some(contents) } => format!("{contents} items"),
            EntryKind::Directory { contents: None } => "... items".into(),
            EntryKind::Uninitialized => unreachable!(),
        }
    }

    pub const fn dir(&self) -> bool {
        matches!(self.kind, EntryKind::Directory { .. })
    }

    pub fn raw_size(&self) -> u64 {
        match self.kind {
            EntryKind::File { size, .. } => size,
            EntryKind::Directory { contents } => contents.unwrap_or_default(),
            EntryKind::Uninitialized => unreachable!(),
        }
    }

    fn clone_inner(&self) -> Self {
        warn!(
            "Cloning an Entry, this should only ever happen if there are search tabs overlapping \
             with flat tabs."
        );
        Self {
            kind: self.kind,
            allocated_size: self.allocated_size,
            abs_path: self.abs_path.clone(),
            name: self.name.clone(),
            mtime: self.mtime,
            mime: self.mime,
            symlink: self.symlink.clone(),
            icon: self.icon.clone(),
            inode: self.inode,
        }
    }
}

#[derive(Debug)]
pub enum Thumbnail {
    Texture(Texture),
    Pending,
    None,
}

impl From<Thumbnail> for Option<Texture> {
    fn from(value: Thumbnail) -> Self {
        match value {
            Thumbnail::Texture(texture) => Some(texture),
            Thumbnail::Pending | Thumbnail::None => None,
        }
    }
}


mod internal {
    use std::cell::{Ref, RefCell};
    use std::path::Path;
    use std::sync::{Arc, LazyLock};

    use gnome_desktop::DesktopThumbnailSize;
    use gtk::gdk::Texture;
    use gtk::glib::subclass::Signal;
    use gtk::glib::{self, ControlFlow, Priority};
    use gtk::prelude::ObjectExt;
    use gtk::subclass::prelude::{
        ObjectImpl, ObjectSubclass, ObjectSubclassExt, ObjectSubclassIsExt,
    };
    use hashlink::LinkedHashSet;

    use super::{ALL_ENTRY_OBJECTS, Entry, FileTime, ThumbPriority, Thumbnail};
    use crate::com::EntryKind;
    use crate::config::CONFIG;
    use crate::gui::thumb_size;

    // (bound, mapped)
    #[derive(Debug, Default, Clone)]
    struct WidgetCounter {
        bound: u16,
        mapped: u16,
    }

    impl WidgetCounter {
        const fn priority(&self) -> ThumbPriority {
            match self {
                Self { bound: 0, mapped: 0 } => ThumbPriority::Low,
                Self { bound: _, mapped: 0 } => ThumbPriority::Medium,
                Self { .. } => ThumbPriority::High,
            }
        }
    }

    #[derive(Debug)]
    enum Thumb {
        Never,
        Unloaded,
        // TODO Loading(DesktopThumbnailSize, bool), if bool is true, show the icon
        Loading(DesktopThumbnailSize),
        Loaded(Texture, DesktopThumbnailSize),
        Outdated(Texture, Option<DesktopThumbnailSize>),
        Failed,
    }

    impl Thumb {
        fn needs_load(&self, size: DesktopThumbnailSize) -> bool {
            match self {
                Self::Loading(sz) | Self::Loaded(_, sz) | Self::Outdated(_, Some(sz)) => {
                    *sz != size
                }
                Self::Never | Self::Failed => false,
                Self::Unloaded | Self::Outdated(_, None) => true,
            }
        }

        fn renderable(&self, from_event: bool) -> Thumbnail {
            match self {
                Self::Never | Self::Failed => Thumbnail::None,
                // TODO -- Thumbnail::None when entry was from an event?
                Self::Unloaded | Self::Loading(_) if from_event => Thumbnail::None,
                Self::Unloaded | Self::Loading(_) => Thumbnail::Pending,
                Self::Loaded(tex, _) | Self::Outdated(tex, _) => Thumbnail::Texture(tex.clone()),
            }
        }
    }


    #[derive(Debug)]
    struct Wrapper {
        entry: Entry,
        widgets: WidgetCounter,
        thumbnail: Thumb,
        updated_from_event: bool,
    }

    #[derive(Debug, Default)]
    pub struct EntryWrapper(RefCell<Option<Wrapper>>);


    #[glib::object_subclass]
    impl ObjectSubclass for EntryWrapper {
        type Type = super::EntryObject;

        const NAME: &'static str = "aw-fm-Entry";
    }

    impl ObjectImpl for EntryWrapper {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: LazyLock<Vec<Signal>> =
                LazyLock::new(|| vec![Signal::builder("update").build()]);
            SIGNALS.as_ref()
        }

        fn dispose(&self) {
            let path = &self.get().abs_path;
            // Could check the load factor and shrink the map.
            // dispose can, technically, be called multiple times, so unsafe to assert here.
            // We also purge contents during refresh.
            let removed = ALL_ENTRY_OBJECTS.with_borrow_mut(|m| {
                let Some((k, v)) = m.remove_entry(path) else {
                    return false;
                };

                // Remove only if the key is the same Arc<path>.
                // This should be fairly rare, even during refreshes.
                if !Arc::ptr_eq(&k, path) {
                    trace!("Not removing newer EntryObject for {path:?} after purge");
                    m.insert(k, v);
                    false
                } else {
                    true
                }
            });

            if removed && Self::UNLOAD_LOW.with(|u| *u) {
                Self::UNLOAD_QUEUE.with_borrow_mut(|q| q.remove(path));
            }
        }
    }


    // Might be worth redoing this and moving more logic down into EntryObject.
    // These should not be Entry methods since they don't make sense for a bare Entry.
    impl EntryWrapper {
        // To avoid flapping reloads due to GTK weirdness
        const LRU_THUMBNAIL_LIMIT: usize = 512;

        thread_local! {
            static ENABLE_THUMBS: bool = CONFIG.max_thumbnailers > 0;
            static UNLOAD_LOW: bool = CONFIG.background_thumbnailers < 0;
            static UNLOAD_QUEUE: RefCell<LinkedHashSet<Arc<Path>>> = RefCell::default();
            static INITIAL_PRIORITY: ThumbPriority =
                if CONFIG.background_thumbnailers >= 0 {
                    ThumbPriority::Low
                } else {
                    ThumbPriority::Generate
                };
        }

        pub(super) fn init(&self, entry: Entry, from_event: bool) -> Option<ThumbPriority> {
            let (thumbnail, p) = if Self::ENABLE_THUMBS.with(|p| *p) {
                match entry.kind {
                    EntryKind::File { .. } => {
                        let p = if from_event {
                            // Use Low if it came from an event to avoid slowing down everything.
                            ThumbPriority::Low
                        } else {
                            Self::INITIAL_PRIORITY.with(|p| *p)
                        };
                        (Thumb::Unloaded, Some(p))
                    }
                    EntryKind::Directory { .. } => (Thumb::Never, None),
                    EntryKind::Uninitialized => unreachable!(),
                }
            } else {
                (Thumb::Never, None)
            };

            // TODO -- other mechanisms to set thumbnails as Never, like mimetype or somesuch
            // Though it'd need an audit of the rest of the code

            let wrapped = Wrapper {
                entry,
                widgets: WidgetCounter::default(),
                thumbnail,
                updated_from_event: from_event,
            };
            assert!(self.0.replace(Some(wrapped)).is_none());
            p
        }

        pub(super) fn update_inner(&self, mut entry: Entry) -> (Entry, Option<ThumbPriority>) {
            // Every time there is an update, we have an opportunity to try the thumbnail again if
            // it failed.

            let widgets = { self.0.borrow().as_ref().unwrap().widgets.clone() };

            let (thumbnail, new_p) = if Self::ENABLE_THUMBS.with(|p| *p) {
                match entry.kind {
                    EntryKind::File { .. } => {
                        let wrapped = self.0.borrow();
                        let inner = wrapped.as_ref().unwrap();
                        let new_thumb = match &inner.thumbnail {
                            Thumb::Never | Thumb::Unloaded | Thumb::Loading(_) | Thumb::Failed => {
                                Thumb::Unloaded
                            }
                            Thumb::Loaded(old, _) | Thumb::Outdated(old, _) => {
                                Thumb::Outdated(old.clone(), None)
                            }
                        };

                        (new_thumb, Some(widgets.priority()))
                    }
                    EntryKind::Directory { .. } => (Thumb::Never, None),
                    EntryKind::Uninitialized => unreachable!(),
                }
            } else {
                (Thumb::Never, None)
            };

            // Since this is used as the key in the hash map, if we use the new Arc<Path> we'll
            // double memory usage for no reason.
            // We also rely on Arc<Path> pointer equality after refreshes.
            {
                entry.abs_path = self.get().abs_path.clone();
            }


            let wrapped = Wrapper {
                entry,
                widgets,
                thumbnail,
                updated_from_event: true,
            };
            let old = self.0.replace(Some(wrapped)).unwrap();
            self.obj().emit_by_name::<()>("update", &[]);

            (old.entry, new_p)
        }

        pub(super) fn get(&self) -> Ref<Entry> {
            let b = self.0.borrow();
            Ref::map(b, |o| &o.as_ref().unwrap().entry)
        }

        // Marks the thumbnail as loading if it matches the given priority and size.
        pub fn mark_thumbnail_loading(&self, p: ThumbPriority, size: DesktopThumbnailSize) -> bool {
            let mut b = self.0.borrow_mut();
            let inner = &mut b.as_mut().unwrap();

            if !inner.thumbnail.needs_load(size) {
                return false;
            }

            if p == ThumbPriority::Generate {
                // Only generate a thumbnail if nothing has changed since this object was created,
                // and don't actually move this thumbnail into the Loading state.
                // The generated thumbnail can still be used to satisfy a later Loading state if
                // that does change.
                inner.widgets.priority() == ThumbPriority::Low
                    && matches!(&inner.thumbnail, Thumb::Unloaded)
            } else if inner.widgets.priority() == p {
                match &mut inner.thumbnail {
                    Thumb::Never | Thumb::Unloaded | Thumb::Loading(..) | Thumb::Failed => {
                        inner.thumbnail = Thumb::Loading(size)
                    }
                    Thumb::Loaded(tex, _) => {
                        inner.thumbnail = Thumb::Outdated(tex.clone(), Some(size))
                    }
                    Thumb::Outdated(_, loading) => *loading = Some(size),
                }
                true
            } else {
                false
            }
        }

        fn unload_thumbnail(path: Arc<Path>) {
            if !super::EntryObject::lookup(&path).is_some_and(|eo| {
                eo.imp().0.borrow().as_ref().unwrap().widgets.priority() == ThumbPriority::Low
            }) {
                return;
            }

            Self::UNLOAD_QUEUE.with_borrow_mut(|q| {
                q.insert(path);
                Self::unload_above_limit(Self::LRU_THUMBNAIL_LIMIT, q);
            });
        }

        pub(super) fn purge_unload_queue() {
            if !Self::UNLOAD_LOW.with(|u| *u) {
                return;
            }

            Self::UNLOAD_QUEUE.with_borrow_mut(|q| {
                Self::unload_above_limit(0, q);
            });
        }

        fn unload_above_limit(limit: usize, queue: &mut LinkedHashSet<Arc<Path>>) {
            while queue.len() > limit {
                let unload = queue.pop_front().unwrap();

                let Some(s) = super::EntryObject::lookup(&unload) else {
                    continue;
                };

                let mut b = s.imp().0.borrow_mut();
                let inner = &mut b.as_mut().unwrap();
                if inner.widgets.priority() == ThumbPriority::Low {
                    // This is spammy because gtk changes the bound widgets a lot.
                    // That also means burning a lot of CPU time sometimes, but eh.
                    // trace!("Unloaded thumbnail for {:?}", inner.entry.abs_path);
                    inner.thumbnail = Thumb::Unloaded;
                }
            }
        }

        fn start_unload_thumbnail(path: Arc<Path>) {
            // This, annoyingly, needs to happen after gtk has a chance to correct the
            // widgets count, hence the idle unload.
            Self::UNLOAD_QUEUE.with_borrow_mut(|q| q.remove(&path));

            let mut path = Some(path);
            glib::idle_add_local_full(Priority::LOW, move || {
                Self::unload_thumbnail(path.take().unwrap());
                ControlFlow::Break
            });
        }

        pub(super) fn change_widgets(&self, bound: i16, mapped: i16) -> Option<ThumbPriority> {
            let mut b = self.0.borrow_mut();
            let inner = &mut b.as_mut().unwrap();
            let w = &mut inner.widgets;
            let old_p = w.priority();

            // Should never fail, but explicitly check
            w.bound = w.bound.checked_add_signed(bound).unwrap();
            w.mapped = w.mapped.checked_add_signed(mapped).unwrap();

            let new_p = w.priority();

            if Self::UNLOAD_LOW.with(|u| *u) {
                if new_p == ThumbPriority::Low {
                    match &inner.thumbnail {
                        Thumb::Loading(_) | Thumb::Loaded(..) | Thumb::Outdated(..) => {
                            Self::start_unload_thumbnail(inner.entry.abs_path.clone())
                        }
                        Thumb::Never | Thumb::Failed | Thumb::Unloaded => {}
                    }
                    return None;
                }

                if old_p == ThumbPriority::Low {
                    Self::UNLOAD_QUEUE.with_borrow_mut(|q| q.remove(&inner.entry.abs_path));
                }
            }

            if new_p != old_p && inner.thumbnail.needs_load(thumb_size()) {
                Some(new_p)
            } else {
                None
            }
        }

        pub(super) fn needs_reload_for_size(
            &self,
            size: DesktopThumbnailSize,
        ) -> Option<ThumbPriority> {
            let b = self.0.borrow();
            let inner = &b.as_ref().unwrap();

            if inner.thumbnail.needs_load(size) {
                Some(inner.widgets.priority())
            } else {
                None
            }
        }

        // There is a minute risk of a race where we're loading a thumbnail for a file twice at
        // once and the first one finishes second. The risk is so low and the outcome so minor it
        // just isn't worth addressing.
        pub fn update_thumbnail(&self, tex: Texture, mtime: FileTime, size: DesktopThumbnailSize) {
            let mut b = self.0.borrow_mut();
            let inner = &mut b.as_mut().unwrap();
            let thumb = &mut inner.thumbnail;
            if inner.entry.mtime != mtime {
                trace!("Not updating thumbnail for updated file {:?}", &*inner.entry.name);
                return;
            }

            inner.updated_from_event = false;

            match thumb {
                Thumb::Loaded(_, sz) if *sz != size => {}
                Thumb::Loading(sz) | Thumb::Outdated(_, Some(sz)) if *sz == size => {}
                Thumb::Never
                | Thumb::Unloaded
                | Thumb::Failed
                | Thumb::Loading(..)
                | Thumb::Loaded(..)
                | Thumb::Outdated(..) => return,
            }

            *thumb = Thumb::Loaded(tex, size);
            drop(b);
            self.obj().emit_by_name::<()>("update", &[]);
        }

        pub fn fail_thumbnail(&self, mtime: FileTime) {
            let mut b = self.0.borrow_mut();
            let inner = &mut b.as_mut().unwrap();
            let thumb = &mut inner.thumbnail;
            if inner.entry.mtime != mtime {
                debug!("Not failing thumbnail for updated file {:?}", &*inner.entry.name);
                return;
            }

            inner.updated_from_event = false;

            match thumb {
                Thumb::Never | Thumb::Failed => return,
                Thumb::Loading(_) | Thumb::Unloaded => (),
                Thumb::Outdated(..) | Thumb::Loaded(..) => {
                    info!(
                        "Marking previously valid thumbnail as failed for: {:?}",
                        inner.entry.abs_path
                    );
                }
            }
            *thumb = Thumb::Failed;
            drop(b);
            self.obj().emit_by_name::<()>("update", &[]);
        }

        pub(super) fn thumbnail(&self) -> Thumbnail {
            let b = self.0.borrow();
            let inner = b.as_ref().unwrap();
            inner.thumbnail.renderable(inner.updated_from_event)
        }

        // Returns true if it's appropriate to synchronously thumbnail this file.
        // This does not consider thumbnail sizes right now as it's unlikely to matter.
        // The only case when it would really matter is when we have a Loaded thumbnail of the
        // wrong size.
        pub fn can_sync_thumbnail(&self) -> bool {
            match self.0.borrow().as_ref().unwrap().thumbnail {
                Thumb::Never | Thumb::Loaded(..) | Thumb::Failed => false,
                Thumb::Unloaded | Thumb::Loading(_) => true,
                Thumb::Outdated(..) => {
                    // This is a really niche edge case, but really it should be handled.
                    true
                }
            }
        }

        pub fn was_updated_from_event(&self) -> bool {
            self.0.borrow().as_ref().unwrap().updated_from_event
        }
    }
}

thread_local! {
    // This does burn a bit of memory, but it avoids any costly searches on updates and insertions.
    static ALL_ENTRY_OBJECTS: RefCell<AHashMap<Arc<Path>, WeakRef<EntryObject>>> =
        AHashMap::new().into();

    static ICON_MAP: RefCell<BTreeMap<*mut GVariant, Icon>> = RefCell::default();
}

glib::wrapper! {
    // This is a terrible name, everything in this file is named poorly.
    pub struct EntryObject(ObjectSubclass<internal::EntryWrapper>);
}

impl EntryObject {
    // This can ONLY be called after all tabs have been cleared of their content.
    // If any tabs still have live references to entry objects in this map we can end up with
    // duplicate enties or stale values.
    pub unsafe fn purge() {
        ALL_ENTRY_OBJECTS.take();
    }

    pub fn idle_trim() {
        ALL_ENTRY_OBJECTS.with_borrow_mut(|m| m.shrink_to_fit());
        internal::EntryWrapper::purge_unload_queue();
    }

    fn create(entry: Entry, from_event: bool) -> Self {
        let obj: Self = Object::new();
        let p = obj.imp().init(entry, from_event);
        obj.queue_thumb(p, from_event);

        obj
    }

    pub fn new(entry: Entry, from_event: bool) -> Self {
        let obj = Self::create(entry, from_event);

        ALL_ENTRY_OBJECTS.with_borrow_mut(|m| {
            let old = m.insert(obj.get().abs_path.clone(), obj.downgrade());
            assert!(old.is_none());
        });

        obj
    }

    // If the old entry is present, we have existing search tabs we'll need to update.
    //
    // This cannot cause updates to existing non-search lists.
    pub fn create_or_update(entry: Entry, from_event: bool) -> (Self, Option<Entry>) {
        ALL_ENTRY_OBJECTS.with_borrow_mut(|m| {
            match m.entry(entry.abs_path.clone()) {
                // We update it here if it's different to avoid the case where this is a stale
                // entry from a search tab keeping a stale reference up to date, or a user is
                // refreshing a remote directory with a search open.
                hash_map::Entry::Occupied(o) => {
                    let value = o.into_mut();
                    if let Some(existing) = value.upgrade() {
                        // If we got a meaningful update here, treat it as if it's from an event
                        // regardless.
                        return (existing.clone(), existing.update(entry));
                    }

                    warn!("Got dangling WeakRef in EntryObject::create_or_update");
                    let new = Self::create(entry, from_event);
                    *value = new.downgrade();
                    (new, None)
                }
                hash_map::Entry::Vacant(v) => {
                    let new = Self::create(entry, from_event);
                    v.insert(new.downgrade());
                    (new, None)
                }
            }
        })
    }

    pub fn lookup(path: &Path) -> Option<Self> {
        ALL_ENTRY_OBJECTS.with_borrow(|m| m.get(path).and_then(WeakRef::upgrade))
    }

    // Returns the old value only if an update happened.
    pub fn update(&self, entry: impl GetEntry + Borrow<Entry>) -> Option<Entry> {
        let old = self.imp().get();
        assert!(old.abs_path == entry.borrow().abs_path);
        if *old == *entry.borrow() {
            // No change we care about (spammy)
            // trace!("Update for {:?} was unimportant", entry.abs_path);
            return None;
        }

        drop(old);

        trace!("Update for {:?}", entry.borrow().abs_path);

        let (old, p) = self.imp().update_inner(entry.get_entry());
        self.queue_thumb(p, true);


        Some(old)
    }

    pub(super) fn cmp(&self, other: &Self, settings: SortSettings) -> Ordering {
        self.imp().get().cmp(&other.imp().get(), settings)
    }

    pub fn get(&self) -> Ref<'_, Entry> {
        self.imp().get()
    }

    fn queue_thumb(&self, p: Option<ThumbPriority>, from_event: bool) {
        if let Some(p) = p {
            // Very spammy
            // trace!(
            //     "Queuing thumbnail for {:?} {p:?}: from_event {from_event}",
            //     self.get().abs_path,
            // );
            queue_thumb(self.downgrade(), p, from_event)
        }
    }

    // mapped is whether the widget was mapped at the time of binding.
    pub fn mark_bound(&self, mapped: bool) {
        self.queue_thumb(
            self.imp().change_widgets(1, mapped.into()),
            self.imp().was_updated_from_event(),
        );
    }

    pub fn mark_unbound(&self, mapped: bool) {
        self.queue_thumb(
            self.imp().change_widgets(-1, -i16::from(mapped)),
            self.imp().was_updated_from_event(),
        );
    }

    pub fn mark_mapped_changed(&self, mapped: bool) {
        self.queue_thumb(
            self.imp().change_widgets(0, if mapped { 1 } else { -1 }),
            self.imp().was_updated_from_event(),
        );
    }

    pub fn thumbnail_no_defer(&self) -> Option<Texture> {
        self.imp().thumbnail().into()
    }

    // TODO -- the idea here is to show the icon if the thumbnail is still loading after a
    // non-trivial amount of time. If that's not necessary, remove these two methods and go back to
    // imp().thumbnail().
    pub fn thumbnail_or_defer(&self) -> Thumbnail {
        // if let Thumbnail::Pending = thumb {
        // let c = self.clone();
        //     glib::timeout_add_local_once(Duration::from_millis(5), move || {
        //
        //     });
        // }
        self.imp().thumbnail()
    }

    pub fn change_thumb_size(size: DesktopThumbnailSize) {
        ALL_ENTRY_OBJECTS.with_borrow(|m| {
            // WeakRef::upgrade should always succeed here
            m.values().filter_map(WeakRef::upgrade).for_each(|eo| {
                if eo.imp().needs_reload_for_size(size).is_some() {
                    info!(
                        "Invalidating thumbnail for {:?} after thumbnail size change",
                        eo.get().abs_path
                    );
                }
                eo.queue_thumb(
                    eo.imp().needs_reload_for_size(size),
                    eo.imp().was_updated_from_event(),
                );
            })
        });
    }

    pub fn icon(&self) -> Icon {
        ICON_MAP.with_borrow_mut(|im| {
            let key = self.get().icon.as_ptr();

            match im.entry(key) {
                btree_map::Entry::Occupied(o) => o.get().clone(),
                btree_map::Entry::Vacant(v) => {
                    let icon = Icon::deserialize(&self.get().icon).unwrap();
                    v.insert(icon).clone()
                }
            }
        })
    }

    pub fn matches_seek(&self, lowercase: &str) -> bool {
        self.get().name.normalized().contains(lowercase)
    }
}


// Mimetypes are small but very often shared between many files.
// There might be some slight write contention early on but RwLock should pay for itself fairly
// quickly.
static INTERNED_MIMETYPES: RwLock<BTreeSet<&'static str>> = RwLock::new(BTreeSet::new());

#[allow(clippy::significant_drop_tightening)]
fn intern_mimetype(mime: GString) -> &'static str {
    if let Some(existing) = INTERNED_MIMETYPES.read().unwrap().get(mime.as_str()) {
        return existing;
    }

    let mut iw = INTERNED_MIMETYPES.write().unwrap();

    if let Some(existing) = iw.get(mime.as_str()) {
        return existing;
    }

    let leaked: &'static str = Box::leak(mime.to_string().into_boxed_str());
    trace!("Interned mimetype {leaked}");
    iw.insert(leaked);
    leaked
}

// Same for icons, but variants are larger and have their own refcounting
static INTERNED_ICONS: RwLock<BTreeMap<Box<str>, Variant>> = RwLock::new(BTreeMap::new());

#[allow(
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]
fn intern_icon(icon: Icon) -> Variant {
    let key = IconExt::to_string(&icon).unwrap().to_string().into_boxed_str();

    if let Some(existing) = INTERNED_ICONS.read().unwrap().get(&key) {
        return existing.clone();
    }

    let mut iw = INTERNED_ICONS.write().unwrap();


    match iw.entry(key) {
        btree_map::Entry::Occupied(o) => o.get().clone(),
        btree_map::Entry::Vacant(v) => {
            let variant = icon.serialize().unwrap();
            v.insert(variant).clone()
        }
    }
}
