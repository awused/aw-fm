// This file contains the structures references by both the gui and manager side of the
// application.


use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use derive_more::{Deref, DerefMut, From};
use gtk::glib::{Object, SignalHandlerId};
use gtk::prelude::{EventControllerExt, IsA, ObjectExt, WidgetExt};
use gtk::EventController;
use tokio::sync::oneshot;

pub use self::entry::*;
pub use self::settings::*;
pub use self::snapshot::*;
use crate::gui::TabId;


mod entry;
mod settings;
mod snapshot;


#[derive(Debug)]
pub enum Update {
    /// We don't really care about a creation vs update here, treat them all as a potential update.
    /// Races with reading the initial directory can cause us get a creation event for an entry we
    /// already have.
    Entry(Arc<Entry>),
    Removed(Arc<Path>),
}

impl Update {
    pub fn path(&self) -> &Path {
        match self {
            Self::Entry(e) => &e.abs_path,
            Self::Removed(path) => path,
        }
    }

    pub fn is_in_subdir(&self, ancestor: &Path) -> bool {
        let path = self.path();
        if path.parent() == Some(ancestor) {
            return false;
        }

        path.starts_with(ancestor)
    }
}

#[derive(Debug)]
pub struct SearchUpdate {
    pub search_id: Arc<AtomicBool>,
    pub update: Update,
}

#[derive(Debug, Clone, Copy)]
pub enum ActionTarget {
    /// The action was called with no associated tab
    NoTab,
    /// A specific tab, if this tab is closed the action will not run.
    Tab(TabId),
    /// The active tab, whatever it _currently_ is.
    /// Only useful for scripts when they call ClearTarget, otherwise it'll be
    /// Tab with a specific ID.
    Active,
}

#[derive(Debug)]
pub struct CompletionResult {
    pub tab: TabId,
    pub initial: String,
    pub candidates: Vec<PathBuf>,
    pub position: usize,
}

#[derive(Debug)]
pub enum ManagerAction {
    Open(Arc<Path>, SortSettings, Arc<AtomicBool>),
    Unwatch(Arc<Path>),
    Search(Arc<Path>, Arc<AtomicBool>),
    EndSearch(Arc<AtomicBool>),

    // For commands from configs/scripts
    Execute(Arc<Path>, Vec<(String, OsString)>),
    Script(Arc<Path>, ActionTarget, Vec<(String, OsString)>),
    // When launching an application or executable directly
    Launch(Arc<Path>, Vec<(String, OsString)>),

    GetChildren(Vec<Arc<Path>>, Arc<AtomicBool>),

    Flush(Vec<PathBuf>, oneshot::Sender<()>),

    Complete(PathBuf, String, TabId),
    CancelCompletion,
}


#[derive(Debug, Default, Clone, Copy)]
pub struct ChildInfo {
    pub size: u64,
    pub allocated: u64,
    pub files: usize,
    pub dirs: usize,
    pub done: bool,
}

#[derive(Debug)]
pub enum GuiAction {
    Watching(Arc<AtomicBool>),
    Snapshot(DirSnapshot),
    Update(Update),

    SearchSnapshot(SearchSnapshot),
    SearchUpdate(SearchUpdate),

    DirChildren(Arc<AtomicBool>, ChildInfo),

    DirectoryOpenError(Arc<Path>, String),
    // Directory errors that aren't as fatal. Could maybe flash the tab?
    DirectoryError(Arc<Path>, String),
    EntryReadError(Arc<Path>, Arc<Path>, String),
    // Any generic error we want to convey to the user.
    ConveyError(String),
    ConveyWarning(String),

    Action(String, ActionTarget),
    Completion(CompletionResult),

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
// This isn't necessary when connecting to widgets that will dispose of all their connectors when
// they are disposed of.
#[derive(Debug)]
pub struct SignalHolder<T: IsA<Object>>(T, Option<SignalHandlerId>);

impl<T: IsA<Object>> Drop for SignalHolder<T> {
    fn drop(&mut self) {
        self.0.disconnect(self.1.take().unwrap());
    }
}

impl<T: IsA<Object>> SignalHolder<T> {
    pub fn new(obj: &T, id: SignalHandlerId) -> Self {
        Self(obj.clone(), Some(id))
    }
}


#[derive(Debug)]
pub struct ControllerDisconnector(EventController);

impl Drop for ControllerDisconnector {
    fn drop(&mut self) {
        self.0.widget().remove_controller(&self.0);
    }
}

impl From<EventController> for ControllerDisconnector {
    fn from(controller: EventController) -> Self {
        Self(controller)
    }
}
