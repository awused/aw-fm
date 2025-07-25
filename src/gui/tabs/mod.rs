use std::env::current_dir;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use path_clean::PathClean;

use self::contents::Contents;
use self::list::TabsList;
use crate::com::{DisplayMode, Entry, EntryObject};
use crate::config::OPTIONS;
use crate::gui::show_warning;

mod contents;
mod element;
pub mod list;
mod pane;
mod search;
mod tab;


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
struct NavTarget {
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
    fn assume_dir<P: AsRef<Path> + Into<Arc<Path>>>(path: P) -> Self {
        Self { dir: path.into(), scroll: None }
    }

    // Will cause an error later if path isn't a directory, jump missing causes no real problems.
    fn assume_jump<P: AsRef<Path> + Into<Arc<Path>>>(path: P, jump: Arc<Path>) -> Self {
        Self { dir: path.into(), scroll: Some(jump) }
    }

    fn initial(list: &TabsList) -> Option<Self> {
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
