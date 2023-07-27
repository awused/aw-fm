use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk::traits::FilterExt;
use gtk::{CustomFilter, Orientation, Widget};

use super::contents::Contents;
use super::id::TabId;
use super::pane::{Pane, PaneExt};
use super::{PaneState, PartiallyAppliedUpdate};
use crate::com::{
    DirSettings, EntryObject, EntryObjectSnapshot, ManagerAction, SearchSnapshot, SearchUpdate,
    SortSettings, Update,
};
use crate::gui::gui_run;

// Search is handled as, effectively, an overlay on top of a flat tab.
//
// It gets items in current_dir from the tab, but gets everything in subdirs on its own.
#[derive(Debug, Default)]
enum State {
    #[default]
    Unloaded,
    Loading(SearchId, Vec<SearchUpdate>),
    Done(SearchId),
}

#[derive(Debug)]
pub(super) struct SearchPane {
    tab: TabId,
    path: Arc<Path>,
    state: State,
    // This contains everything in tab.contents plus items from subdirectories.
    contents: Contents,
    query: Rc<RefCell<String>>,
    pub filter: CustomFilter,
    // This is used to store a view state until search is done loading.
}

impl SearchPane {
    pub const fn contents(&self) -> &Contents {
        &self.contents
    }

    pub const fn loaded(&self) -> bool {
        matches!(self.state, State::Done(..))
    }

    pub const fn loading(&self) -> bool {
        matches!(self.state, State::Loading(..))
    }

    pub fn start_load(&mut self) -> bool {
        match self.state {
            State::Unloaded => {
                self.state = State::Loading(SearchId::new(self.path.clone()), Vec::new());
                true
            }
            State::Loading(..) | State::Done(_) => false,
        }
    }

    pub fn query(&self) -> Rc<RefCell<String>> {
        self.query.clone()
    }

    pub fn new(
        tab: TabId,
        path: Arc<Path>,
        settings: DirSettings,
        flat_contents: &Contents,
        query: String,
    ) -> Self {
        let state = State::Unloaded;
        let (contents, filter) = Contents::search_from(flat_contents);

        let query = Rc::new(RefCell::new(query.to_lowercase()));

        Self {
            tab,
            path,
            state,
            contents,
            query,
            filter,
        }
    }

    pub fn clone_for(&self, tab: TabId, new_contents: &Contents) -> Self {
        let state = State::Unloaded;
        let (contents, filter) = Contents::search_from(new_contents);

        let query = Rc::new(RefCell::new(self.query.borrow().clone()));

        Self {
            tab,
            path: self.path.clone(),
            state,
            contents,
            query,
            filter,
        }
    }

    pub fn set_query(&mut self, query: String) {
        trace!("Updated search to: {query}");
        self.query.replace(query);
        self.filter.changed(gtk::FilterChange::Different);
    }

    pub fn re_sort(&mut self) {
        self.contents.re_sort_for_search();
    }

    pub fn matches_snapshot(&self, snap: &SearchSnapshot) -> bool {
        match &self.state {
            State::Unloaded | State::Done(_) => false,
            State::Loading(id, _) => Arc::ptr_eq(&id.0, &snap.id),
        }
    }

    pub fn matches_update(&self, update: &SearchUpdate) -> bool {
        match &self.state {
            State::Unloaded => false,
            State::Loading(id, _) | State::Done(id) => Arc::ptr_eq(&id.0, &update.search_id),
        }
    }

    pub fn apply_flat_snapshot(&mut self, snap: EntryObjectSnapshot) {
        self.contents.add_flat_elements_for_search(snap);
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

            for u in pending {
                self.apply_search_update_inner(u);
            }

            self.state = State::Done(id);
        }
    }

    fn apply_search_update_inner(&mut self, s_update: SearchUpdate) {
        let update = s_update.update;
        // For consistency reasons we only process local inserts or deletions here.
        // Do not process global or local mutations as they could overlap with another search or
        // with flat directories and cause sort issues.
        //
        // This is probably good enough.
        //
        // Flat updates will always take priority over anything done in here.
        let existing_global = EntryObject::lookup(update.path());
        let local_position = existing_global
            .clone()
            .and_then(|eg| self.contents.total_position_by_sorted(&eg.get()));

        match (update, local_position, existing_global) {
            (_, Some(_), None) => unreachable!(),
            (Update::Entry(_), None, None) => todo!(),
            (Update::Entry(_), None, Some(eo)) => {
                trace!("Inserting existing item from update in search tab.");
                self.contents.insert(&eo);
            }
            (Update::Entry(_), Some(_), Some(_)) => {
                debug!("Dropping mutation in search tab {:?}", self.path);
            }
            (Update::Removed(_), None, _) => {}
            (Update::Removed(_), Some(pos), Some(_)) => {
                debug!("Removing item in search tab {:?}", self.path);
                self.contents.remove(pos);
            }
        }
    }

    pub fn apply_search_update(&mut self, s_update: SearchUpdate) {
        match &mut self.state {
            State::Unloaded => unreachable!(),
            State::Loading(_, pending) => pending.push(s_update),
            State::Done(_) => self.apply_search_update_inner(s_update),
        }
    }

    pub fn update_settings(&mut self, settings: DirSettings) {
        self.contents.sort(settings.sort);
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
