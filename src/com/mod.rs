// This file contains the structures references by both the gui and manager side of the
// application.


use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use derive_more::{Deref, DerefMut, From};
use gtk::glib::{Object, SignalHandlerId};
use gtk::prelude::{IsA, ObjectExt};

pub use self::entry::*;
pub use self::settings::*;
pub use self::snapshot::*;


mod entry;
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

#[derive(Debug)]
pub struct SearchUpdate {
    pub search_id: Arc<AtomicBool>,
    pub update: Update,
}


#[derive(Debug)]
pub enum ManagerAction {
    Open(Arc<Path>, Arc<AtomicBool>),
    Refresh(Arc<Path>, Arc<AtomicBool>),
    Unwatch(Arc<Path>),
    Search(Arc<Path>, Arc<AtomicBool>),
    EndSearch(Arc<AtomicBool>),
    Execute(String, Vec<(String, OsString)>),
    Script(String, Vec<(String, OsString)>),
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct WorkParams {
    pub park_before_scale: bool,
    pub jump_downscaling_queue: bool,
    pub extract_early: bool,
}


#[derive(Debug)]
pub enum GuiAction {
    Snapshot(DirSnapshot),
    Update(Update),
    SearchSnapshot(SearchSnapshot),
    SearchUpdate(SearchUpdate),

    DirectoryOpenError(Arc<Path>, String),
    // Directory errors that aren't as fatal. Could maybe flash the tab?
    DirectoryError(Arc<Path>, String),
    EntryReadError(Arc<Path>, Arc<Path>, String),
    // Any generic error we want to convey to the user.
    ConveyError(String),

    // SearchSubscription,
    // DirectoryContents

    //State(GuiState, GuiActionContext),
    Action(String),
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
