// This file contains the structures references by both the gui and manager side of the
// application.

use std::cell::Cell;
use std::cmp::Ordering;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use derive_more::{Deref, DerefMut, From};
use gtk::gio::{self, Cancellable, FileQueryInfoFlags};
use gtk::glib::{Object, SignalHandlerId};
use gtk::prelude::{Cast, FileExt, IsA, ObjectExt};
use gtk::SortType;
use path_clean::PathClean;
use rusqlite::ToSql;
use strum_macros::{AsRefStr, EnumString};
use tokio::sync::oneshot;

pub use self::entry::*;
pub use self::res::*;
pub use self::settings::*;
pub use self::snapshot::*;
use crate::natsort::{self, ParsedString};

mod entry;
mod res;
mod settings;
mod snapshot;


#[derive(Debug)]
pub enum Update {
    // We don't really care about a creation vs update here, treat them all as a potential update.
    // Races with reading the initial directory can cause us get a creation event for an entry we
    // already have.
    Entry(Entry),
    Removed(Arc<Path>),
}

impl Update {
    pub fn path(&self) -> &Path {
        match self {
            Self::Entry(e) => &e.abs_path,
            Self::Removed(path) => path,
        }
    }
}


pub type CommandResponder = oneshot::Sender<serde_json::Value>;

pub type MAWithResponse = (ManagerAction, GuiActionContext, Option<CommandResponder>);


#[derive(Debug)]
pub enum ManagerAction {
    Open(Arc<Path>),
    // Watch(Arc<Path>),
    Close(Arc<Path>),
    // Refresh(Arc<Path>),
    // Close(PathBuf),
    // StartSearch(),
    // RefreshSearch(),
    // EndSearch(),
    // Resolution,
    // MovePages(usize),
    // NextArchive,
    // PreviousArchive,
    // Status(Vec<(String, OsString)>),
    // Execute(String, Vec<(String, OsString)>),
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct WorkParams {
    pub park_before_scale: bool,
    pub jump_downscaling_queue: bool,
    pub extract_early: bool,
}


// Any additional data the Gui sends along. This is not used or persisted by the manager, and is
// echoed back as context for the Gui to prevent concurrent actions from confusing the Gui.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GuiActionContext {}


#[derive(Debug)]
pub enum GuiAction {
    // Subscription(Arc<WatchedDir>),
    // Metadata,
    Snapshot(DirSnapshot),
    Update(Update),
    // FullSnapshot(Arc<Path>, Vec<Entry>),
    // PartialSnapshot(Start/Middle/End, Files)

    // DirectoryError()
    // EntryError(Arc<Path>, )

    // SearchSubscription,
    // DirectoryContents

    //State(GuiState, GuiActionContext),
    //Action(String, CommandResponder),
    // IdleUnload,
    Quit,
}

#[derive(Deref, Default, DerefMut, From)]
pub struct DebugIgnore<T>(pub T);

impl<T> fmt::Debug for DebugIgnore<T> {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Result::Ok(())
    }
}

// Makes sure to disconnect a signal handler when the rust object drops.
#[derive(Debug)]
pub struct Disconnector<T: IsA<Object>>(T, Option<SignalHandlerId>);

impl<T: IsA<Object>> Drop for Disconnector<T> {
    fn drop(&mut self) {
        self.0.disconnect(self.1.take().unwrap());
    }
}

// We CAN make something super safe that connects in here so that the signal ID is always correct
// for this object, but just not worth it.
impl<T: IsA<Object>> Disconnector<T> {
    pub fn new(obj: &T, id: SignalHandlerId) -> Self {
        Self(obj.clone(), Some(id))
    }
}

// #[derive(Debug)]
// pub struct DedupedVec<T> {
//     deduped: Vec<T>,
//     indices: Vec<usize>,
// }
//
// impl<T> Index<usize> for DedupedVec<T> {
//     type Output = T;
//
//     fn index(&self, index: usize) -> &Self::Output {
//         &self.deduped[self.indices[index]]
//     }
// }
//
// impl<T> IndexMut<usize> for DedupedVec<T> {
//     fn index_mut(&mut self, index: usize) -> &mut Self::Output {
//         &mut self.deduped[self.indices[index]]
//     }
// }
//
// impl<T> DedupedVec<T> {
//     pub fn len(&self) -> usize {
//         self.indices.len()
//     }
//
//     pub fn iter_deduped_mut(&mut self) -> std::slice::IterMut<T> {
//         self.deduped.iter_mut()
//     }
//
//     pub fn map<U, F>(&self, f: F) -> DedupedVec<U>
//     where
//         F: FnMut(&T) -> U,
//     {
//         DedupedVec {
//             deduped: self.deduped.iter().map(f).collect(),
//             indices: self.indices.clone(),
//         }
//     }
// }

// #[derive(Debug, PartialEq, Eq, Clone, Copy)]
// pub enum Toggle {
//     Change,
//     On,
//     Off,
// }
//
// impl TryFrom<&str> for Toggle {
//     type Error = ();
//
//     fn try_from(value: &str) -> Result<Self, Self::Error> {
//         if value.eq_ignore_ascii_case("toggle") {
//             Ok(Self::Change)
//         } else if value.eq_ignore_ascii_case("on") {
//             Ok(Self::On)
//         } else if value.eq_ignore_ascii_case("off") {
//             Ok(Self::Off)
//         } else {
//             Err(())
//         }
//     }
// }
//
// impl Toggle {
//     // Returns true if something happened.
//     #[must_use]
//     pub fn apply(self, v: &mut bool) -> bool {
//         match (self, *v) {
//             (Self::Change, _) | (Self::On, false) | (Self::Off, true) => {
//                 *v = !*v;
//                 true
//             }
//             _ => false,
//         }
//     }
//
//     // Returns true if something happened.
//     #[must_use]
//     pub fn apply_cell(self, v: &Cell<bool>) -> bool {
//         let val = v.get();
//         match (self, val) {
//             (Self::Change, _) | (Self::On, false) | (Self::Off, true) => {
//                 v.set(!val);
//                 true
//             }
//             _ => false,
//         }
//     }
//
//     pub fn run_if_change(self, v: bool, became_true: impl FnOnce(), became_false: impl FnOnce())
// {         match (self, v) {
//             (Self::Change | Self::On, false) => became_true(),
//             (Self::Change | Self::Off, true) => became_false(),
//             _ => {}
//         }
//     }
// }
