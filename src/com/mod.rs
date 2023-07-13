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
use gtk::glib::Object;
use gtk::prelude::{Cast, FileExt};
use path_clean::PathClean;
use rusqlite::ToSql;
use strum_macros::{AsRefStr, EnumString};
use tokio::sync::oneshot;

pub use self::entry::*;
pub use self::res::*;
use crate::natsort::{self, ParsedString};

mod entry;
mod res;


#[derive(Debug, Default, Clone, Copy, EnumString, AsRefStr)]
#[strum(serialize_all = "lowercase")]
pub enum DisplayMode {
    Icons,
    #[default]
    List,
}

#[derive(Debug, Default, Clone, Copy, EnumString, AsRefStr)]
#[strum(serialize_all = "lowercase")]
pub enum SortMode {
    #[default]
    Name,
    MTime,
    Size,
    BTime,
}

#[derive(Debug, PartialEq, Eq, Default, Clone, Copy, EnumString, AsRefStr)]
#[strum(serialize_all = "lowercase")]
pub enum SortDir {
    #[default]
    Ascending,
    Descending,
}

#[derive(Debug, Default, Clone, Copy, EnumString, AsRefStr)]
#[strum(serialize_all = "lowercase")]
pub enum DisplayHidden {
    #[default]
    // Default, -- would be the global setting, if/when we have one
    False,
    True,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SortSettings {
    pub mode: SortMode,
    pub direction: SortDir,
}

impl SortSettings {
    pub fn comparator(self) -> impl Fn(&Object, &Object) -> Ordering + 'static {
        move |a, b| {
            let a = a.downcast_ref::<EntryObject>().unwrap();
            let b = b.downcast_ref::<EntryObject>().unwrap();
            a.cmp(b, self)
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DirSettings {
    pub mode: DisplayMode,
    pub sort: SortSettings,
    pub display_hidden: DisplayHidden,
}


pub type CommandResponder = oneshot::Sender<serde_json::Value>;

pub type MAWithResponse = (ManagerAction, GuiActionContext, Option<CommandResponder>);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Toggle {
    Change,
    On,
    Off,
}

impl TryFrom<&str> for Toggle {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.eq_ignore_ascii_case("toggle") {
            Ok(Self::Change)
        } else if value.eq_ignore_ascii_case("on") {
            Ok(Self::On)
        } else if value.eq_ignore_ascii_case("off") {
            Ok(Self::Off)
        } else {
            Err(())
        }
    }
}

impl Toggle {
    // Returns true if something happened.
    #[must_use]
    pub fn apply(self, v: &mut bool) -> bool {
        match (self, *v) {
            (Self::Change, _) | (Self::On, false) | (Self::Off, true) => {
                *v = !*v;
                true
            }
            _ => false,
        }
    }

    // Returns true if something happened.
    #[must_use]
    pub fn apply_cell(self, v: &Cell<bool>) -> bool {
        let val = v.get();
        match (self, val) {
            (Self::Change, _) | (Self::On, false) | (Self::Off, true) => {
                v.set(!val);
                true
            }
            _ => false,
        }
    }

    pub fn run_if_change(self, v: bool, became_true: impl FnOnce(), became_false: impl FnOnce()) {
        match (self, v) {
            (Self::Change | Self::On, false) => became_true(),
            (Self::Change | Self::Off, true) => became_false(),
            _ => {}
        }
    }
}

#[derive(Debug)]
pub enum ManagerAction {
    Open(Arc<Path>),
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


#[derive(Debug, Clone)]
pub enum SnapshotKind {
    Complete,
    Start,
    Middle,
    End,
}

#[derive(Debug, Clone)]
pub struct DirSnapshot {
    pub kind: SnapshotKind,
    pub path: Arc<Path>,
    pub entries: Vec<Entry>,
}


#[derive(Debug)]
pub enum GuiAction {
    // Subscription(Arc<WatchedDir>),
    // Metadata,
    Snapshot(DirSnapshot),
    // FullSnapshot(Arc<Path>, Vec<Entry>),
    // PartialSnapshot(Start/Middle/End, Files)

    // DirectoryError()
    // EntryError(Arc<Path>, )

    // SearchSubscription,
    // DirectoryContents

    //State(GuiState, GuiActionContext),
    //Action(String, CommandResponder),
    IdleUnload,
    Quit,
}

// #[derive(Deref, Default, DerefMut, From)]
// pub struct DebugIgnore<T>(pub T);
//
// impl<T> fmt::Debug for DebugIgnore<T> {
//     fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
//         fmt::Result::Ok(())
//     }
// }
//
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
