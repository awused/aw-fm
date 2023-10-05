use std::borrow::Borrow;
use std::cell::{Ref, RefCell};
use std::cmp::Ordering;
use std::collections::{btree_map, hash_map, BTreeMap, BTreeSet};
use std::fmt::{self, Formatter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ahash::AHashMap;
use chrono::{Local, TimeZone};
use gtk::gdk::Texture;
use gtk::gio::ffi::G_FILE_TYPE_DIRECTORY;
use gtk::gio::{
    self, Cancellable, FileQueryInfoFlags, Icon, FILE_ATTRIBUTE_ACCESS_CAN_EXECUTE,
    FILE_ATTRIBUTE_STANDARD_ALLOCATED_SIZE, FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE,
    FILE_ATTRIBUTE_STANDARD_ICON, FILE_ATTRIBUTE_STANDARD_IS_SYMLINK, FILE_ATTRIBUTE_STANDARD_SIZE,
    FILE_ATTRIBUTE_STANDARD_SYMLINK_TARGET, FILE_ATTRIBUTE_STANDARD_TYPE,
    FILE_ATTRIBUTE_TIME_MODIFIED, FILE_ATTRIBUTE_TIME_MODIFIED_USEC,
    FILE_ATTRIBUTE_UNIX_IS_MOUNTPOINT,
};
use gtk::glib::ffi::GVariant;
use gtk::glib::{self, GStr, GString, Object, Variant, WeakRef};
use gtk::prelude::{FileExt, IconExt, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use once_cell::sync::Lazy;

use super::{SortDir, SortMode, SortSettings};
use crate::gui::{queue_thumb, ThumbPriority};
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
        FILE_ATTRIBUTE_STANDARD_SYMLINK_TARGET,
        FILE_ATTRIBUTE_STANDARD_SIZE,
        FILE_ATTRIBUTE_STANDARD_ALLOCATED_SIZE,
        FILE_ATTRIBUTE_TIME_MODIFIED,
        FILE_ATTRIBUTE_TIME_MODIFIED_USEC,
        FILE_ATTRIBUTE_UNIX_IS_MOUNTPOINT,
        FILE_ATTRIBUTE_ACCESS_CAN_EXECUTE,
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
    Uninitialized, // Broken {}
}


// Does NOT implement Clone so we can rely on GObject refcounting to minimize copies.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    pub allocated_size: u64,

    // This is an absolute but NOT canonicalized path.
    pub abs_path: Arc<Path>,
    // It's kind of expensive to do this but necessary as an mtime/ctime tiebreaker anyway.
    pub name: ParsedString,
    pub mtime: FileTime,

    // Doesn't work over NFS, could fall back to "changed" time but that's not what we really
    // want. Given how I use my NAS this just isn't useful right now.
    // pub btime: FileTime,
    pub mime: &'static str,
    pub symlink: Option<PathBuf>,
    pub icon: Variant,
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

            if (size % 512 == 0 && size <= 8192) || (mountpoint && size <= 2) {
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
            mime: self.mime.clone(),
            symlink: self.symlink.clone(),
            icon: self.icon.clone(),
        }
    }
}


#[derive(Debug)]
pub enum Thumbnail {
    Nothing,
    Unloaded,
    Loading,
    Loaded(Texture),
    Outdated(Texture, bool),
    Failed,
}

impl Thumbnail {
    const fn needs_load(&self) -> bool {
        match self {
            Self::Nothing | Self::Loaded(_) | Self::Failed | Self::Loading => false,
            Self::Unloaded => true,
            Self::Outdated(_, loading) => !*loading,
        }
    }
}


mod internal {
    use std::cell::{Ref, RefCell};
    use std::sync::Arc;

    use gtk::gdk::Texture;
    use gtk::glib;
    use gtk::glib::subclass::Signal;
    use gtk::prelude::ObjectExt;
    use gtk::subclass::prelude::{ObjectImpl, ObjectSubclass, ObjectSubclassExt};
    use once_cell::sync::Lazy;

    use super::{FileTime, ThumbPriority, Thumbnail, ALL_ENTRY_OBJECTS};
    use crate::com::EntryKind;

    // (bound, mapped)
    #[derive(Debug, Default, Clone)]
    struct WidgetCounter(u16, u16);

    impl WidgetCounter {
        const fn priority(&self) -> ThumbPriority {
            match (self.0, self.1) {
                (0, 0) => ThumbPriority::Low,
                (_, 0) => ThumbPriority::Medium,
                (..) => ThumbPriority::High,
            }
        }
    }

    #[derive(Debug)]
    struct Entry {
        entry: super::Entry,
        widgets: WidgetCounter,
        thumbnail: Thumbnail,
        updated: bool,
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
            // We also purge contents during refresh.
            ALL_ENTRY_OBJECTS.with(|m| {
                // Remove only if the key is the same Arc<path>
                let mut mb = m.borrow_mut();
                let Some((k, _)) = mb.get_key_value(path) else {
                    return;
                };
                if Arc::ptr_eq(k, path) {
                    mb.remove(path);
                } else {
                    trace!("Not removing newer EntryObject for {path:?} after purge");
                }
            });
        }
    }

    // Might be worth redoing this and moving more logic down into EntryObject.
    // These should not be Entry methods since they don't make sense for a bare Entry.
    impl EntryWrapper {
        pub(super) fn init(&self, entry: super::Entry) -> Option<ThumbPriority> {
            let (thumbnail, p) = match entry.kind {
                EntryKind::File { .. } => (Thumbnail::Unloaded, Some(ThumbPriority::Low)),
                EntryKind::Directory { .. } => (Thumbnail::Nothing, None),
                EntryKind::Uninitialized => unreachable!(),
            };

            // TODO -- other mechanisms to set thumbnails as Nothing, like mimetype or somesuch

            let wrapped = Entry {
                entry,
                widgets: WidgetCounter(0, 0),
                thumbnail,
                updated: false,
            };
            assert!(self.0.replace(Some(wrapped)).is_none());
            p
        }

        pub(super) fn update_inner(
            &self,
            mut entry: super::Entry,
        ) -> (super::Entry, Option<ThumbPriority>) {
            // Every time there is an update, we have an opportunity to try the thumbnail again if
            // it failed.

            let widgets = { self.0.borrow().as_ref().unwrap().widgets.clone() };

            let (thumbnail, new_p) = match entry.kind {
                EntryKind::File { .. } => {
                    let wrapped = self.0.borrow();
                    let inner = wrapped.as_ref().unwrap();
                    let new_thumb = match &inner.thumbnail {
                        Thumbnail::Nothing
                        | Thumbnail::Unloaded
                        | Thumbnail::Loading
                        | Thumbnail::Failed => Thumbnail::Unloaded,
                        Thumbnail::Loaded(old) | Thumbnail::Outdated(old, _) => {
                            Thumbnail::Outdated(old.clone(), false)
                        }
                    };

                    (new_thumb, Some(widgets.priority()))
                }
                EntryKind::Directory { .. } => (Thumbnail::Nothing, None),
                EntryKind::Uninitialized => unreachable!(),
            };

            // Since this is used as the key in the hash map, if we use the new Arc<Path> we'll
            // double memory usage for no reason.
            // We also rely on Arc<Path> pointer equality after refreshes.
            {
                let old = self.0.borrow();
                let old_arc = old.as_ref().unwrap().entry.abs_path.clone();
                entry.abs_path = old_arc;
            }


            let wrapped = Entry { entry, widgets, thumbnail, updated: true };
            let old = self.0.replace(Some(wrapped)).unwrap();
            self.obj().emit_by_name::<()>("update", &[]);

            (old.entry, new_p)
        }

        pub(super) fn get(&self) -> Ref<super::Entry> {
            let b = self.0.borrow();
            Ref::map(b, |o| &o.as_ref().unwrap().entry)
        }

        // Marks the thumbnail as loading if it matches the given priority.
        pub fn mark_thumbnail_loading(&self, p: ThumbPriority) -> bool {
            let mut b = self.0.borrow_mut();
            let inner = &mut b.as_mut().unwrap();

            if !inner.thumbnail.needs_load() {
                return false;
            }

            if inner.widgets.priority() == p {
                match &mut inner.thumbnail {
                    Thumbnail::Nothing
                    | Thumbnail::Unloaded
                    | Thumbnail::Loading
                    | Thumbnail::Loaded(_)
                    | Thumbnail::Failed => inner.thumbnail = Thumbnail::Loading,
                    Thumbnail::Outdated(_, loading) => *loading = true,
                }
                true
            } else {
                false
            }
        }

        pub(super) fn change_widgets(&self, bound: i16, mapped: i16) -> Option<ThumbPriority> {
            let mut b = self.0.borrow_mut();
            let inner = &mut b.as_mut().unwrap();
            let w = &mut inner.widgets;
            let old_p = w.priority();

            // Should never fail, but explicitly check
            w.0 = w.0.checked_add_signed(bound).unwrap();
            w.1 = w.1.checked_add_signed(mapped).unwrap();

            let new_p = w.priority();
            if inner.thumbnail.needs_load() && new_p != old_p { Some(new_p) } else { None }
        }

        // There is a minute risk of a race where we're loading a thumbnail for a file twice at
        // once and the first one finishes second. The risk is so low and the outcome so minor it
        // just isn't worth addressing.
        pub fn update_thumbnail(&self, tex: Texture, mtime: FileTime) {
            let mut b = self.0.borrow_mut();
            let inner = &mut b.as_mut().unwrap();
            let thumb = &mut inner.thumbnail;
            if inner.entry.mtime != mtime {
                trace!("Not updating thumbnail for updated file {:?}", &*inner.entry.name);
                return;
            }

            inner.updated = false;

            match thumb {
                Thumbnail::Nothing | Thumbnail::Unloaded | Thumbnail::Failed => {}
                Thumbnail::Loading | Thumbnail::Loaded(_) | Thumbnail::Outdated(..) => {
                    *thumb = Thumbnail::Loaded(tex);
                    drop(b);
                    self.obj().emit_by_name::<()>("update", &[]);
                }
            }
        }

        pub fn fail_thumbnail(&self, mtime: FileTime) {
            let mut b = self.0.borrow_mut();
            let inner = &mut b.as_mut().unwrap();
            let thumb = &mut inner.thumbnail;
            if inner.entry.mtime != mtime {
                trace!("Not failing thumbnail for updated file {:?}", &*inner.entry.name);
                return;
            }

            inner.updated = false;

            match thumb {
                Thumbnail::Nothing | Thumbnail::Unloaded | Thumbnail::Failed => (),
                Thumbnail::Loading => *thumb = Thumbnail::Failed,
                Thumbnail::Outdated(..) | Thumbnail::Loaded(_) => {
                    *thumb = Thumbnail::Failed;
                    info!(
                        "Marking previously valid thumbnail as failed for: {:?}",
                        inner.entry.abs_path
                    );
                    drop(b);
                    self.obj().emit_by_name::<()>("update", &[]);
                }
            }
        }

        // Texture is refcounted, so this is cheap
        pub fn thumbnail(&self) -> Option<Texture> {
            let b = self.0.borrow();
            let thumb = &b.as_ref().unwrap().thumbnail;
            if let Thumbnail::Loaded(tex) = thumb {
                Some(tex.clone())
            } else if let Thumbnail::Outdated(tex, _) = thumb {
                Some(tex.clone())
            } else {
                None
            }
        }

        // Returns true if it's appropriate to synchronously thumbnail this file.
        pub fn can_sync_thumbnail(&self) -> bool {
            match self.0.borrow().as_ref().unwrap().thumbnail {
                Thumbnail::Nothing | Thumbnail::Loaded(_) | Thumbnail::Failed => false,
                Thumbnail::Unloaded | Thumbnail::Loading => true,
                Thumbnail::Outdated(..) => {
                    // This is a really niche edge case, but really it should be handled.
                    true
                }
            }
        }

        pub fn was_updated(&self) -> bool {
            self.0.borrow().as_ref().unwrap().updated
        }
    }
}

// This does burn a bit of memory, but it avoids any costly searches on updates and insertions.
thread_local! {
    static ALL_ENTRY_OBJECTS: RefCell<AHashMap<Arc<Path>, WeakRef<EntryObject>>> =
        AHashMap::new().into();

    static ICON_MAP: RefCell<BTreeMap<*mut GVariant, Icon>> = RefCell::default();
}

glib::wrapper! {
    pub struct EntryObject(ObjectSubclass<internal::EntryWrapper>);
}

impl EntryObject {
    // This can ONLY be called after all tabs have been cleared of their content.
    // If any tabs still have live references to entry objects in this map we can end up with
    // duplicate enties or stale values.
    pub unsafe fn purge() {
        ALL_ENTRY_OBJECTS.with(RefCell::take);
    }

    fn create(entry: Entry, from_event: bool) -> Self {
        let obj: Self = Object::new();
        let p = obj.imp().init(entry);
        obj.queue_thumb(p, from_event);

        obj
    }

    pub fn new(entry: Entry, from_event: bool) -> Self {
        let obj = Self::create(entry, from_event);

        ALL_ENTRY_OBJECTS.with(|m| {
            let old = m.borrow_mut().insert(obj.get().abs_path.clone(), obj.downgrade());
            assert!(old.is_none());
        });

        obj
    }

    // If the old entry is present, we have existing search tabs we'll need to update.
    //
    // This cannot cause updates to existing non-search lists.
    pub fn create_or_update(entry: Entry, from_event: bool) -> (Self, Option<Entry>) {
        ALL_ENTRY_OBJECTS.with(|m| {
            let mut map = m.borrow_mut();

            match map.entry(entry.abs_path.clone()) {
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
        ALL_ENTRY_OBJECTS.with(|m| m.borrow().get(path).and_then(WeakRef::upgrade))
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
            //     "Queuing thumbnail for {:?} {:?}: from_event {from_event}",
            //     self.get().abs_path,
            //     p
            // );
            queue_thumb(self.downgrade(), p, from_event)
        }
    }

    // mapped is whether the widget was mapped at the time of binding.
    pub fn mark_bound(&self, mapped: bool) {
        self.queue_thumb(self.imp().change_widgets(1, mapped.into()), self.imp().was_updated());
    }

    pub fn mark_unbound(&self, mapped: bool) {
        self.queue_thumb(
            self.imp().change_widgets(-1, -i16::from(mapped)),
            self.imp().was_updated(),
        );
    }

    pub fn mark_mapped_changed(&self, mapped: bool) {
        self.queue_thumb(
            self.imp().change_widgets(0, if mapped { 1 } else { -1 }),
            self.imp().was_updated(),
        );
    }

    pub fn icon(&self) -> Icon {
        ICON_MAP.with(|im| {
            let mut map = im.borrow_mut();

            let key = self.get().icon.as_ptr();

            match map.entry(key) {
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
    let ir = INTERNED_MIMETYPES.read().unwrap();

    let Some(existing) = ir.get(mime.as_str()) else {
        drop(ir);
        let mut iw = INTERNED_MIMETYPES.write().unwrap();

        if let Some(existing) = iw.get(mime.as_str()) {
            return existing;
        }

        let leaked: &'static str = Box::leak(mime.to_string().into_boxed_str());
        trace!("Interned mimetype {leaked}");
        iw.insert(leaked);
        return leaked;
    };

    existing
}

// Same for icons, but variants are larger and have their own refcounting
static INTERNED_ICONS: RwLock<BTreeMap<Box<str>, Variant>> = RwLock::new(BTreeMap::new());

#[allow(
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]
fn intern_icon(icon: Icon) -> Variant {
    let key = IconExt::to_string(&icon).unwrap().to_string().into_boxed_str();
    let ir = INTERNED_ICONS.read().unwrap();

    let Some(existing) = ir.get(&key) else {
        drop(ir);
        let mut iw = INTERNED_ICONS.write().unwrap();


        match iw.entry(key) {
            btree_map::Entry::Occupied(o) => return o.get().clone(),
            btree_map::Entry::Vacant(v) => {
                let variant = icon.serialize().unwrap();
                return v.insert(variant).clone();
            }
        }
    };

    existing.clone()
}
