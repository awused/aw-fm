use super::contents::Contents;
use super::pane::{Pane, PaneExt};
use crate::com::DirSettings;

// Search is handled as, effectively, an overlay on top of a flat tab.
//
// It gets items in current_dir from the tab, but gets everything in subdirs on its own.

#[derive(Debug)]
pub(super) struct SearchPane {
    // Search panes do not get evicted as they're expensive to reopen.
    pane: Pane,
    // This contains everything in tab.contents
    contents: Contents,
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
        todo!()
    }

    fn workaround_scroller(&self) -> &gtk::ScrolledWindow {
        todo!()
    }

    fn activate(&self) {
        todo!()
    }
}
