use std::cmp::Ordering;
use std::env::current_dir;
use std::os::linux::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use flat_dir::CachedDir;
use gtk::gio::prelude::*;
use gtk::gio::{ListModel, ListStore};
use gtk::glib::object::{Cast, IsA};
use gtk::glib::{self, ControlFlow, Object, Priority};
use hashlink::LinkedHashMap;
use path_clean::PathClean;
use tab::Tab;

use self::contents::Contents;
use self::list::TabsList;
use crate::com::{DisplayMode, Entry, EntryObject, SortSettings, Update};
use crate::config::OPTIONS;
use crate::gui::show_warning;

mod contents;
mod element;
mod flat_dir;
pub mod list;
mod pane;
mod search;
mod tab;

// TODO -- Hardcoded for now, should be made configurable.
const MAX_CACHED_FLAT_DIRS: usize = 4;

// Context for a lot of tab operations.
// Tabs share a lot of state for efficiency, so often operations on one tab need to touch others.
struct TabContext<'a> {
    left: &'a [Tab],
    right: &'a [Tab],
    cached: &'a mut LinkedHashMap<Arc<Path>, CachedDir>,
}

fn cache_open_dir(cache: &mut LinkedHashMap<Arc<Path>, CachedDir>, new: CachedDir) {
    while cache.len() >= MAX_CACHED_FLAT_DIRS {
        cache.pop_front();
    }

    assert!(cache.insert(new.dir.clone(), new).is_none())
}


#[derive(Debug)]
enum PartiallyAppliedUpdate {
    // Updates without any potential sort change (rare) would be fully applied, but usually mtime
    // will change, so no sense worrying about it.
    Mutate(Entry, EntryObject),
    Insert(EntryObject),
    Delete(EntryObject),
}

impl PartiallyAppliedUpdate {
    fn is_in_subdir(&self, ancestor: &Path) -> bool {
        let entry = match self {
            Self::Mutate(_, eo) | Self::Insert(eo) | Self::Delete(eo) => eo.get(),
        };

        if entry.abs_path.parent() == Some(ancestor) {
            return false;
        }

        entry.abs_path.starts_with(ancestor)
    }
}


#[derive(Debug, Clone)]
struct HistoryEntry {
    location: Arc<Path>,
    search: Option<String>,
    state: PaneState,
}

#[derive(Debug, Clone)]
struct PrecisePosition {
    position: f64,
    // If the directory/mode has updated we ignore the precise positioning.
    view: DisplayMode,
    res: (i32, i32),
    count: u32,
}


#[derive(Debug, Clone)]
struct ScrollPosition {
    // If nothing meaningful has changed, we can restore to the exact same state.
    precise: Option<PrecisePosition>,
    // Usually just cloned from an existing Entry.
    path: Arc<Path>,
    // Used as a backup if path has been removed.
    index: u32,
}

#[derive(Debug, Clone)]
struct FocusState {
    path: Arc<Path>,
    // If true, update the selection state to just this item, otherwise do nothing.
    select: bool,
}

// Not kept up to date, maybe an enum?
#[derive(Debug, Clone, Default)]
struct PaneState {
    pub scroll: Option<ScrollPosition>,
    pub focus: Option<FocusState>,
}

impl PaneState {
    fn for_jump(jump: Option<Arc<Path>>) -> Self {
        Self {
            scroll: jump.clone().map(|path| ScrollPosition { precise: None, path, index: 0 }),
            focus: jump.map(|path| FocusState { path, select: true }),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct NavTarget {
    // A clean and absolute but not canonical path.
    dir: Arc<Path>,
    scroll: Option<Arc<Path>>,
}

impl NavTarget {
    fn open_or_jump<P: AsRef<Path>>(path: P, list: &TabsList) -> Option<Self> {
        let p = path.as_ref();
        let target = Self::cleaned_abs(p, list)?;

        Self::open_or_jump_abs(target.into())
    }

    fn open_or_jump_abs(target: Arc<Path>) -> Option<Self> {
        assert!(target.has_root());
        debug_assert!(*target == *target.clean());

        if !target.exists() {
            show_warning(format!("Could not locate {target:?}"));
            None
        } else if target.is_dir() {
            Some(Self { dir: target, scroll: None })
        } else if let Some(parent) = target.parent() {
            if !parent.is_dir() {
                show_warning(format!("Could not open {target:?}"));
                return None;
            }

            let dir: Arc<Path> = parent.into();
            let scroll = if target.exists() { Some(target) } else { None };

            Some(Self { dir, scroll })
        } else {
            show_warning(format!("Could not locate {target:?}"));
            None
        }
    }

    fn jump<P: AsRef<Path>>(path: P, list: &TabsList) -> Option<Self> {
        let p = path.as_ref();
        let target = Self::cleaned_abs(p, list)?;

        if !target.exists() {
            show_warning(format!("Could not locate {p:?}"));
            None
        } else if let Some(parent) = target.parent() {
            if !parent.is_dir() {
                show_warning(format!("Could not open {p:?}"));
                return None;
            }

            let dir: Arc<Path> = parent.into();
            let scroll = if target.exists() { Some(target.into()) } else { None };

            Some(Self { dir, scroll })
        } else {
            show_warning(format!("Could not locate {p:?}"));
            None
        }
    }

    // Will cause an error later if this isn't a directory.
    pub(super) fn assume_dir<P: AsRef<Path> + Into<Arc<Path>>>(path: P) -> Self {
        Self { dir: path.into(), scroll: None }
    }

    // Will cause an error later if path isn't a directory, jump missing causes no real problems.
    pub(super) fn assume_jump<P: AsRef<Path> + Into<Arc<Path>>>(path: P, jump: Arc<Path>) -> Self {
        Self { dir: path.into(), scroll: Some(jump) }
    }

    fn initial(list: &TabsList) -> Option<Self> {
        if OPTIONS.empty && OPTIONS.file_name.is_none() {
            return None;
        }

        let path = OPTIONS
            .file_name
            .clone()
            .unwrap_or_else(|| current_dir().unwrap_or_else(|_| "/".into()));

        Self::open_or_jump(path, list)
    }

    fn cleaned_abs(p: &Path, list: &TabsList) -> Option<PathBuf> {
        Some(if p.has_root() {
            p.clean()
        } else if let Some(cur) = list.get_active_dir() {
            warn!("Got relative path {p:?}, trying inside current active directory");
            let mut current = cur.to_path_buf();
            current.push(p);
            current.clean()
        } else if let Ok(mut cur) = current_dir() {
            warn!("Got relative path {p:?}, trying inside current working directory");
            cur.push(p);
            cur.clean()
        } else {
            show_warning(format!("Could not make {p:?} absolute"));
            return None;
        })
    }
}


pub mod id {
    use std::cell::Cell;

    thread_local! {
        static NEXT_ID: Cell<u64> = const { Cell::new(0) };
    }

    // A unique identifier for tabs.
    // Options considered:
    //   Incrementing u64:
    //      + Easy implementation
    //      + Fast, no allocations
    //      - Can theoretically overflow
    //      - Uniqueness isn't trivially statically guaranteed
    //      - Linear searching for tabs
    //   Rc<()>:
    //      + Easy implementation
    //      + Rc::ptr_eq is as fast as comparing u64
    //      + Tabs can create their own
    //      + Uniqueness is guaranteed provided tabs always construct their own
    //      - Wasted heap allocations
    //      - Linear searching for tabs
    //  Rc<Cell<index>>:
    //      + No need for linear searching to find tabs
    //      + Rc::ptr_eq is as fast as comparing u64
    //      + Uniqueness is guaranteed
    //      - Most complicated implementation. Must be manually kept up-to-date.
    //      - If the index is ever wrong, weird bugs can happen
    //      - Heap allocation
    //  UUIDs:
    //      - Not really better than a bare u64
    #[derive(Debug, Eq, PartialEq)]
    pub struct TabUid(u64);

    #[derive(Debug, Eq, PartialEq, Clone, Copy, Hash)]
    pub struct TabId(u64);

    pub fn next_id() -> TabUid {
        let n = NEXT_ID.get();
        NEXT_ID.set(n + 1);
        TabUid(n)
    }

    impl TabUid {
        pub const fn copy(&self) -> TabId {
            TabId(self.0)
        }
    }
}

// ListStore isn't even a flat array, so binary searching isn't much worse even at small sizes.
fn listmodel_bsearch<L: IsA<ListModel>>(
    list: &L,
    sort: SortSettings,
    entry: &Entry,
) -> Option<u32> {
    let mut start = 0;
    let mut end = list.n_items();

    if end == 0 {
        return None;
    }

    while start < end {
        let mid = start + (end - start) / 2;

        let obj = list.item(mid).unwrap().downcast::<EntryObject>().unwrap();

        let inner = obj.get();
        if inner.abs_path == entry.abs_path {
            // The equality check below may fail even with abs_path being equal due to updates.
            return Some(mid);
        }

        match entry.cmp(&inner, sort) {
            Ordering::Equal => unreachable!(),
            Ordering::Less => end = mid,
            Ordering::Greater => start = mid + 1,
        }
    }

    // All list stores must always be sorted modulo individual updates by the time updates are
    // being handled.
    //
    // The item is not present.
    None
}

struct TotalPos(u32);

enum ExistingEntry {
    Present(EntryObject, TotalPos),
    NotLocal(EntryObject),
    Missing,
}

fn liststore_entry_for_update(
    list: &ListStore,
    sort: SortSettings,
    update: &Update,
) -> ExistingEntry {
    let path = update.path();

    // If it exists in any tab (search or flat)
    let mut global = EntryObject::lookup(path);

    if global.is_none()
        && matches!(update, Update::Removed(_))
        && path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".nfs"))
    {
        let start = Instant::now();
        if let Ok(inode) = path.metadata().as_ref().map(MetadataExt::st_ino) {
            // We got a "deletion" for a silly renamed file. In practice this is fairly
            // rare, so handle by scanning for the inode number in the current
            // directory. Only consider the current flat contents, not search contents.
            //
            // If performance becomes a concern we'll need a map of inodes somewhere.
            global = list.iter::<EntryObject>().flatten().find(|eo| eo.get().inode == inode);
            if let Some(eo) = &global {
                warn!(
                    "Got NFS silly rename for {:?}. Scan took {:?}",
                    eo.get().abs_path,
                    start.elapsed()
                );
            } else {
                warn!("Got NFS silly rename for missing file. Scan took {:?}", start.elapsed());
            }
        }
    }

    // If it exists in flat tabs (which all share the same state).
    if let Some(global) = global {
        let local = listmodel_bsearch(list, sort, &global.get());
        if let Some(local) = local {
            ExistingEntry::Present(global, TotalPos(local))
        } else {
            ExistingEntry::NotLocal(global)
        }
    } else {
        ExistingEntry::Missing
    }
}


fn liststore_needs_reinsert(
    list: &ListStore,
    sort: SortSettings,
    pos: TotalPos,
    new: &EntryObject,
) -> bool {
    let pos = pos.0;
    let comp = sort.comparator();
    let greater_than_left =
        pos == 0 || comp(&list.item(pos - 1).unwrap(), new.upcast_ref::<Object>()).is_lt();
    let less_than_right = pos + 1 == list.n_items()
        || comp(new.upcast_ref::<Object>(), &list.item(pos + 1).unwrap()).is_lt();
    !greater_than_left || !less_than_right
}

fn liststore_drop_batched(list: ListStore) {
    // Dropping in one go can be glacially slow due to callbacks and notifications.
    // Especially if we're cleaning up the final references to a lot of items with thumbnails.
    // 130ms for ~40k items
    // 160ms for 50k
    // More with thumbnails
    let start = Instant::now();
    let total = list.n_items();
    glib::idle_add_local_full(Priority::LOW, move || {
        if list.n_items() <= 1000 {
            trace!("Finished dropping {total} items in {:?}", start.elapsed());
            return ControlFlow::Break;
        }
        list.splice(0, 1000, &[] as &[EntryObject]);
        ControlFlow::Continue
    });
}
