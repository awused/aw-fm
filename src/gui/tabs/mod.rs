use std::cell::{Cell, OnceCell, Ref, RefCell};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::env::current_dir;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut, Index, IndexMut};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::gio::ListStore;
use gtk::glib::Object;
use gtk::prelude::{Cast, ListModelExt, ListModelExtManual, StaticType};
use gtk::subclass::prelude::{ObjectSubclassExt, ObjectSubclassIsExt};
use gtk::traits::{AdjustmentExt, BoxExt, SelectionModelExt, WidgetExt};
use gtk::{glib, Box, MultiSelection, Orientation, ScrolledWindow};
use path_clean::PathClean;

use self::contents::Contents;
use self::list::TabsList;
use self::pane::{Pane, PaneExt};
use self::search::SearchPane;
use crate::com::{
    DirSettings, DirSnapshot, DisplayMode, Entry, EntryObject, EntryObjectSnapshot, FileTime,
    GuiAction, ManagerAction, SnapshotId, SnapshotKind, SortMode, SortSettings,
};
use crate::config::OPTIONS;
use crate::gui::Update;
use crate::natsort::ParsedString;

mod contents;
mod element;
pub mod list;
mod pane;
mod search;
mod tab;

use id::TabUid;

use super::{gui_run, tabs_run};


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

        if Some(ancestor) == entry.abs_path.parent() {
            return false;
        }

        entry.abs_path.starts_with(ancestor)
    }
}


#[derive(Debug, Clone)]
struct HistoryEntry {
    location: Arc<Path>,
    state: SavedViewState,
}

#[derive(Debug, Clone)]
struct ScrollPosition {
    // Usually just cloned from an existing Entry.
    path: Arc<Path>,
    // Used as a backup if path has been removed.
    index: u32,
}

// Not kept up to date, maybe an enum?
#[derive(Debug, Clone, Default)]
struct SavedViewState {
    // If the directory has updated we just don't care, it'll be wrong.
    pub scroll_pos: Option<ScrollPosition>,
    // Selected items?
    pub search: Option<String>,
}

impl SavedViewState {
    fn for_jump(jump: Option<Arc<Path>>) -> Self {
        Self {
            scroll_pos: jump.map(|path| ScrollPosition { path, index: 0 }),
            ..Self::default()
        }
    }
}

#[derive(Debug)]
struct NavTarget {
    // A clean and absolute but not canonical path.
    dir: Arc<Path>,
    scroll: Option<Arc<Path>>,
}

impl NavTarget {
    fn open_or_jump<P: AsRef<Path>>(path: P, list: &TabsList) -> Option<Self> {
        let p = path.as_ref();
        let target = Self::cleaned_abs(p, list)?;

        Some(if !target.exists() {
            gui_run(|g| g.warning(&format!("Could not locate {p:?}")));
            return None;
        } else if target.is_dir() {
            Self { dir: target.into(), scroll: None }
        } else if let Some(parent) = target.parent() {
            if !parent.is_dir() {
                gui_run(|g| g.warning(&format!("Could not open {p:?}")));
                return None;
            }

            let dir: Arc<Path> = parent.into();
            let scroll = if target.exists() { Some(target.into()) } else { None };

            Self { dir, scroll }
        } else {
            gui_run(|g| g.warning(&format!("Could not locate {p:?}")));
            return None;
        })
    }

    fn jump<P: AsRef<Path>>(path: P, list: &TabsList) -> Option<Self> {
        let p = path.as_ref();
        let target = Self::cleaned_abs(p, list)?;

        Some(if !target.exists() {
            gui_run(|g| g.warning(&format!("Could not locate {p:?}")));
            return None;
        } else if let Some(parent) = target.parent() {
            if !parent.is_dir() {
                gui_run(|g| g.warning(&format!("Could not open {p:?}")));
                return None;
            }

            let dir: Arc<Path> = parent.into();
            let scroll = if target.exists() { Some(target.into()) } else { None };

            Self { dir, scroll }
        } else {
            gui_run(|g| g.warning(&format!("Could not locate {p:?}")));
            return None;
        })
    }

    // Will cause an error later if this isn't a directory.
    fn assume_dir<P: AsRef<Path>>(path: P) -> Self {
        Self { dir: path.as_ref().into(), scroll: None }
    }

    fn initial(list: &TabsList) -> Option<Self> {
        let mut path = OPTIONS
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
            error!("Could not make {p:?} absolute");
            gui_run(|g| g.warning("Could not open {p:?}"));
            return None;
        })
    }
}

pub mod id {
    use std::cell::Cell;

    thread_local! {
        static NEXT_ID: Cell<u64> = Cell::new(0);
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

    #[derive(Debug, Eq, PartialEq, Clone, Copy)]
    pub struct TabId(u64);

    pub fn next_id() -> TabUid {
        TabUid(NEXT_ID.with(|n| {
            let o = n.get();
            n.set(o + 1);
            o
        }))
    }

    impl TabUid {
        pub const fn copy(&self) -> TabId {
            TabId(self.0)
        }
    }
}
