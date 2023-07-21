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
use self::pane::{Pane, PaneExt};
use self::search::SearchPane;
use crate::com::{
    DirSettings, DirSnapshot, DisplayMode, Entry, EntryObject, EntryObjectSnapshot, FileTime,
    GuiAction, ManagerAction, SnapshotId, SnapshotKind, SortMode, SortSettings,
};
use crate::gui::Update;
use crate::natsort::ParsedString;

mod contents;
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
    // This is intentionally not the same Arc<Path> we use for active tabs.
    // If there is a matching tab when we activate this history entry, steal that Arc and state.
    // If there is none, we need a new, fresh Arc<> that definitely has no pending snapshots.
    location: Rc<Path>,
    scroll_pos: SavedViewState,
}

// Not kept up to date, maybe an enum?
#[derive(Debug, Clone, Default)]
struct SavedViewState {
    // First visible element.
    // If the directory has updated we just don't care, it'll be wrong.
    pub scroll_pos: u32,
    // Selected items?
    pub search: Option<String>,
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
