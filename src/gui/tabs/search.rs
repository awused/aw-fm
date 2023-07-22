use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::contents::Contents;
use super::pane::{Pane, PaneExt};
use super::SavedViewState;
use crate::com::DirSettings;

// Search is handled as, effectively, an overlay on top of a flat tab.
//
// It gets items in current_dir from the tab, but gets everything in subdirs on its own.


#[derive(Debug)]
enum State {
    Loading(Arc<Path>, SearchId),
    Done,
}

#[derive(Debug)]
pub(super) struct SearchPane {
    state: State,
    pane: Pane,
    // This contains everything in tab.contents plus items from subdirectories.
    contents: Contents,
    // This is used to store a view state until search is done.
    pending_view_state: Option<SavedViewState>,
}

impl SearchPane {
    fn contents(&mut self) -> &mut Contents {
        &mut self.contents
    }
}

impl PaneExt for SearchPane {
    fn update_settings(&mut self, settings: DirSettings, _ignored: &Contents) {
        todo!()
    }

    fn get_view_state(&self, _list: &super::Contents) -> super::SavedViewState {
        todo!()
    }

    fn apply_view_state(&mut self, state: super::SavedViewState) {
        match self.state {
            State::Loading(..) => {
                self.pending_view_state = Some(state);
                todo!()
            }
            State::Done => todo!(),
        }
    }

    fn workaround_scroller(&self) -> &gtk::ScrolledWindow {
        todo!()
    }

    fn activate(&self) {
        todo!()
    }
}

// The pointer is used for uniqueness, the boolean is used to signal cancellation on drop.
#[derive(Debug)]
struct SearchId(Arc<AtomicBool>);

impl Drop for SearchId {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed)
    }
}
