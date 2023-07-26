use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk::{CustomFilter, Orientation};

use super::contents::Contents;
use super::id::TabId;
use super::pane::{Pane, PaneExt};
use super::{PartiallyAppliedUpdate, SavedPaneState};
use crate::com::{
    DirSettings, EntryObjectSnapshot, ManagerAction, SearchSnapshot, SearchUpdate, SortSettings,
};
use crate::gui::gui_run;

// Search is handled as, effectively, an overlay on top of a flat tab.
//
// It gets items in current_dir from the tab, but gets everything in subdirs on its own.
#[derive(Debug, Default)]
enum State {
    #[default]
    Unloaded,
    Loading(SearchId, Vec<PartiallyAppliedUpdate>),
    AwaitingFlat(SearchId, Vec<PartiallyAppliedUpdate>),
    Done(SearchId),
}

#[derive(Debug)]
pub(super) struct SearchPane {
    path: Arc<Path>,
    state: State,
    flat_loading: bool,
    pane: Option<Pane>,
    // This contains everything in tab.contents plus items from subdirectories.
    contents: Contents,
    // This is used to store a view state until search is done loading.
    pending_view_state: Option<SavedPaneState>,
}

impl SearchPane {
    pub fn new(
        tab: TabId,
        path: Arc<Path>,
        settings: DirSettings,
        flat_loading: bool,
        flat_contents: &Contents,
        query: String,
        flat_pane: Pane,
    ) -> Self {
        let state = State::Loading(SearchId::new(path.clone()), Vec::new());
        let (contents, filter) = Contents::search_from(flat_contents);

        let pane = flat_pane.flat_to_search(query, settings, &contents.selection, filter);

        Self {
            path,
            state,
            flat_loading,
            pane: Some(pane),
            contents,
            pending_view_state: None,
        }
    }

    pub fn new_unattached(
        parent_loading: bool,
        flat: &Contents,
        pane_state: SavedPaneState,
    ) -> Self {
        todo!()
    }

    pub fn set_query(&mut self, query: String) {
        if let Some(pane) = &self.pane {
            pane.update_search(&query);
        } else {
            // Search query blows away previous pane state.
            self.pending_view_state = Some(SavedPaneState { scroll_pos: None, search: Some(query) })
        }
    }

    pub fn re_sort(&mut self) {
        self.contents.re_sort_for_search();
    }

    pub const fn is_loading(&self) -> bool {
        matches!(self.state, State::Done(..))
    }

    pub fn matches_snapshot(&self, snap: &SearchSnapshot) -> bool {
        match &self.state {
            State::Unloaded | State::Done(_) | State::AwaitingFlat(..) => false,
            State::Loading(id, _) => Arc::ptr_eq(&id.0, &snap.id),
        }
    }

    pub fn matches_update(&self, update: &SearchUpdate) -> bool {
        match &self.state {
            State::Unloaded => false,
            State::Loading(id, _) | State::AwaitingFlat(id, _) | State::Done(id) => {
                Arc::ptr_eq(&id.0, &update.search_id)
            }
        }
    }

    pub fn apply_flat_snapshot(&mut self, snap: EntryObjectSnapshot) {
        self.flat_loading = !snap.id.kind.finished();

        self.contents.add_flat_elements_for_search(snap);

        self.check_done();
    }

    // These need to go through to the contents immediately
    pub fn finish_flat_update(&mut self, update: &PartiallyAppliedUpdate) {
        self.contents.finish_update(update);
    }

    // Special handling for flat events in other tabs that might overlap this search.
    // They might need to be immediately applied, even during loading, because they could
    // change sort order.
    pub fn handle_subdir_flat_mutate(&mut self, update: &PartiallyAppliedUpdate) {
        if !update.is_in_subdir(&self.path) {
            return;
        }

        if let PartiallyAppliedUpdate::Mutate(old, new) = update {
            // This needs to be applied immediately if the item is present to maintain sorting.
            if let Some(pos) = self.contents.total_position_by_sorted(old) {
                self.contents.reinsert_updated(pos, new, old);
            }
        } else {
            unreachable!()
        }
    }

    pub fn apply_search_snapshot(&mut self, snap: SearchSnapshot) {
        let done = snap.finished;
        self.contents.apply_search_snapshot(snap);

        if done {
            let State::Loading(id, pending) = std::mem::take(&mut self.state) else {
                unreachable!();
            };

            self.state = State::AwaitingFlat(id, pending);
        }

        self.check_done()
    }

    pub fn apply_search_update(&mut self, update: SearchUpdate) {
        todo!()
    }

    fn check_done(&mut self) {
        error!("TODO -- finish loading search")
    }
}

impl PaneExt for SearchPane {
    fn set_active(&mut self, active: bool) {
        assert!(
            !active || self.pane.is_some(),
            "Called set_active on a pane that wasn't visible"
        );
        if let Some(pane) = &mut self.pane {
            pane.set_active(active);
        }
    }

    fn visible(&self) -> bool {
        self.pane.as_ref().map_or(false, PaneExt::visible)
    }

    fn update_settings(&mut self, settings: DirSettings, _ignored: &Contents) {
        todo!()
    }

    fn get_state(&self, _ignored: &super::Contents) -> super::SavedPaneState {
        if let Some(pane) = &self.pane {
            let query = pane.text_contents();
            let mut state = pane.get_state(&self.contents);

            state.search = Some(query);
            state
        } else {
            todo!()
        }
    }

    fn apply_state(&mut self, state: super::SavedPaneState, _ignored: &super::Contents) {
        match self.state {
            State::Unloaded | State::Loading(..) | State::AwaitingFlat(..) => {
                self.pending_view_state = Some(state);
                todo!()
            }
            State::Done(..) => todo!(),
        }
    }

    fn workaround_scroller(&self) -> Option<&gtk::ScrolledWindow> {
        self.pane.as_ref().and_then(PaneExt::workaround_scroller)
    }

    fn activate(&self) {
        todo!()
    }

    fn split(&self, orient: Orientation) -> Option<gtk::Paned> {
        self.pane.as_ref().unwrap().split(orient)
    }

    fn next_of_kin(&self) -> Option<TabId> {
        todo!()
    }
}

// The pointer is used for uniqueness, the boolean is used to signal cancellation on drop.
#[derive(Debug)]
struct SearchId(Arc<AtomicBool>);

impl Drop for SearchId {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
        gui_run(|g| g.send_manager(ManagerAction::EndSearch(self.0.clone())))
    }
}

impl SearchId {
    fn new(path: Arc<Path>) -> Self {
        let id: Arc<AtomicBool> = Arc::default();

        gui_run(|g| g.send_manager(ManagerAction::Search(path, id.clone())));

        Self(id)
    }
}
