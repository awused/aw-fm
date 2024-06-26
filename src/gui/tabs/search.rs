use std::borrow::Cow;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk::CustomFilter;

use super::contents::Contents;
use super::PartiallyAppliedUpdate;
use crate::com::{
    DirSettings, EntryObject, EntryObjectSnapshot, GetEntry, ManagerAction, SearchSnapshot,
    SearchUpdate, SortSettings, Update,
};
use crate::gui::gui_run;
use crate::natsort::normalize_lowercase;

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
pub(super) struct Search {
    path: Arc<Path>,
    state: State,
    // This contains everything in tab.contents plus items from subdirectories.
    contents: Contents,
    original: Rc<RefCell<String>>,
    // Searching is case insensitive (for now at least, may do smart case later).
    normalized: Rc<RefCell<String>>,
    pub filter: CustomFilter,
}

impl Search {
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

    // TODO -- refresh -- sync unload for search
    pub fn unload(&mut self, sort: SortSettings) {
        self.state = State::Unloaded;
        self.contents.clear(sort);
    }

    pub fn query(&self) -> (Rc<RefCell<String>>, Rc<RefCell<String>>) {
        (self.original.clone(), self.normalized.clone())
    }

    pub fn new(path: Arc<Path>, flat_contents: &Contents, query: String) -> Self {
        let state = State::Unloaded;
        let (contents, filter) = Contents::search_from(flat_contents);


        let lowercase = query.to_lowercase();
        let normalized = match normalize_lowercase(&lowercase) {
            Cow::Borrowed(_) => lowercase,
            Cow::Owned(s) => s,
        };
        let normalized = Rc::new(RefCell::new(normalized));

        let original = Rc::new(RefCell::new(query));


        Self {
            path,
            state,
            contents,
            original,
            normalized,
            filter,
        }
    }

    pub fn clone_for(&self, new_contents: &Contents) -> Self {
        let state = State::Unloaded;
        let (contents, filter) = Contents::search_from(new_contents);

        let original = Rc::new(RefCell::new(self.original.borrow().clone()));
        let normalized = Rc::new(RefCell::new(self.normalized.borrow().clone()));

        Self {
            path: self.path.clone(),
            state,
            contents,
            original,
            normalized,
            filter,
        }
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

    pub fn overlaps_other_update(&self, update: &SearchUpdate) -> bool {
        match &self.state {
            State::Unloaded => false,
            State::Loading(..) | State::Done(_) => update.update.is_in_subdir(&self.path),
        }
    }

    // Special handling for flat events in other tabs that might overlap this search.
    // They might need to be immediately applied, even during loading, because they could
    // change sort order.
    pub fn handle_subdir_flat_update(&mut self, update: &PartiallyAppliedUpdate) {
        if !update.is_in_subdir(&self.path) {
            return;
        }

        match update {
            PartiallyAppliedUpdate::Delete(old) => {
                let entry = old.get();
                if let Some(pos) = self.contents.total_position_by_sorted(&entry) {
                    drop(entry);
                    self.contents.remove(pos);
                }
            }
            PartiallyAppliedUpdate::Mutate(old, new) => {
                // This needs to be applied immediately if the item is present to maintain sorting.
                if let Some(pos) = self.contents.total_position_by_sorted(old) {
                    self.contents.reinsert_updated(pos, new, old);
                }
            }
            PartiallyAppliedUpdate::Insert(_) => unreachable!(),
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
                self.apply_search_update_inner(u, false);
            }

            self.state = State::Done(id);
        }
    }

    fn apply_search_update_inner(&mut self, s_update: SearchUpdate, allow_mutation: bool) {
        let update = s_update.update;
        // For consistency reasons we only process mutations here when no other tabs might match
        // this mutation.
        //
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
            (Update::Entry(entry), None, None) => {
                trace!("Inserting new item from update in search tab.");
                let new = EntryObject::new(entry.get_entry(), true);
                self.contents.insert(&new);
            }
            (Update::Entry(_), None, Some(eo)) => {
                trace!("Inserting existing item from update in search tab.");
                self.contents.insert(&eo);
            }
            (Update::Entry(entry), Some(pos), Some(obj)) => {
                if !allow_mutation {
                    return debug!("Dropping mutation in search tab {:?}", self.path);
                }

                // It exists in this tab, but no other tabs, so mutating it is safe.
                let Some(old) = obj.update(entry) else {
                    return;
                };

                self.contents.reinsert_updated(pos, &obj, &old);
                debug!("Updated {:?} from event in search tab", old.abs_path);
            }
            (Update::Removed(_), None, _) => {}
            (Update::Removed(_), Some(pos), Some(_)) => {
                debug!("Removing item in search tab {:?}", self.path);
                self.contents.remove(pos);
            }
        }
    }

    pub fn apply_search_update(&mut self, s_update: SearchUpdate, allow_mutation: bool) {
        match &mut self.state {
            State::Unloaded => unreachable!(),
            State::Loading(_, pending) => {
                trace!(
                    "Deferring search update {:?} until loading is done.",
                    s_update.update.path()
                );
                pending.push(s_update);
            }
            State::Done(_) => self.apply_search_update_inner(s_update, allow_mutation),
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
        self.0.store(true, Ordering::Relaxed);
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
