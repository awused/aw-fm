use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use gtk::prelude::{CastNone, ListModelExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::WidgetExt;
use gtk::{Orientation, Widget};
use MaybePane as MP;

use self::flat_dir::FlatDir;
use super::contents::Contents;
use super::element::TabElement;
use super::id::{TabId, TabUid};
use super::pane::Pane;
use super::search::Search;
use super::{HistoryEntry, NavTarget, PaneState, ScrollPosition};
use crate::com::{
    DirSettings, DirSnapshot, DisplayMode, EntryObject, EntryObjectSnapshot, SearchSnapshot,
    SearchUpdate, SnapshotId, SortSettings,
};
use crate::gui::tabs::PartiallyAppliedUpdate;
use crate::gui::{gui_run, Update};

/* This efficiently supports multiple tabs being open to the same directory with different
 * settings.
 *
 * It's a little more complicated than if they all were required to have the same settings, but
 * the only way to make this code actually simple is to completely abandon efficiency -
 * especially in my common case of opening a new tab.
 */


#[derive(Debug)]
enum MaybePane {
    Pane(Pane),
    Pending(Pane, PaneState),
    Closed(PaneState),
}

impl MaybePane {
    const fn has_pane(&self) -> bool {
        match &self {
            Self::Pane(_) | Self::Pending(..) => true,
            Self::Closed(_) => false,
        }
    }

    const fn get(&self) -> Option<&Pane> {
        match self {
            Self::Pane(p) | Self::Pending(p, _) => Some(p),
            Self::Closed(_) => None,
        }
    }

    fn getm(&mut self) -> Option<&mut Pane> {
        match self {
            Self::Pane(p) | Self::Pending(p, _) => Some(p),
            Self::Closed(_) => None,
        }
    }

    fn clone_state(&self, list: &Contents) -> PaneState {
        match self {
            Self::Pane(p) => p.get_state(list),
            Self::Pending(_, state) | Self::Closed(state) => state.clone(),
        }
    }

    fn overwrite_state(&mut self, state: PaneState) {
        match self {
            Self::Pane(_p) => {
                let temp = Self::Closed(PaneState::default());
                let old = std::mem::replace(self, temp);
                let Self::Pane(pane) = old else {
                    unreachable!()
                };
                *self = Self::Pending(pane, state);
            }
            Self::Pending(_, old_state) | Self::Closed(old_state) => *old_state = state,
        }
    }

    fn set_pane(&mut self, pane: Pane) {
        match self {
            Self::Pane(_) | Self::Pending(..) => unreachable!(),
            Self::Closed(state) => *self = Self::Pending(pane, std::mem::take(state)),
        }
    }

    fn take_pane(&mut self, list: &Contents) -> Pane {
        match self {
            Self::Pane(p) => {
                let new = Self::Closed(p.get_state(list));
                let old = std::mem::replace(self, new);
                let Self::Pane(pane) = old else {
                    unreachable!()
                };
                pane
            }
            Self::Pending(_p, state) => {
                let state = std::mem::take(state);
                let old = std::mem::replace(self, Self::Closed(state));
                let Self::Pending(p, _) = old else {
                    unreachable!()
                };
                p
            }
            Self::Closed(_) => unreachable!(),
        }
    }

    fn must_resolve_pending(&mut self) -> PaneState {
        match self {
            Self::Pane(_) | Self::Closed(_) => unreachable!(),
            Self::Pending(_p, _state) => {
                let temp = Self::Closed(PaneState::default());
                let old = std::mem::replace(self, temp);
                let Self::Pending(pane, state) = old else {
                    unreachable!()
                };
                *self = Self::Pane(pane);
                state
            }
        }
    }
}

// Current limitations:
// - All tabs open to the same directory are in the same TabState
// - Refreshing has a large splash radius.
// - Cannot refresh just one tab open to a directory.
//
// Invariants:
// - All tabs open to the same directory share the same watcher
// - All tabs open to the same directory share the same snapshots
// - All tabs open to the same directory are in the same TabState
#[derive(Debug)]
pub(super) struct Tab {
    id: TabUid,
    dir: FlatDir,
    pane: MaybePane,

    settings: DirSettings,
    contents: Contents,
    past: Vec<HistoryEntry>,
    future: Vec<HistoryEntry>,
    element: TabElement,

    // Each tab can only be open in one pane at once.
    // In theory we could do many-to-many but it's too niche.
    search: Option<Search>,
}

impl Tab {
    pub const fn id(&self) -> TabId {
        self.id.copy()
    }

    pub const fn visible(&self) -> bool {
        self.pane.has_pane()
    }

    fn loading(&self) -> bool {
        self.dir.state().loading() || self.search.as_ref().map_or(false, Search::loading)
    }

    fn loaded(&self) -> bool {
        self.dir.state().loaded() && self.search.as_ref().map_or(true, Search::loaded)
    }

    pub fn dir(&self) -> Arc<Path> {
        self.dir.path().clone()
    }

    pub fn matches_arc(&self, other: &Arc<Path>) -> bool {
        Arc::ptr_eq(self.dir.path(), other)
    }

    fn matching_mut<'a>(&'a self, other: &'a mut [Self]) -> impl Iterator<Item = &mut Self> {
        other.iter_mut().filter(|t| self.matches_arc(t.dir.path()))
    }

    fn matching<'a>(&'a self, other: &'a [Self]) -> impl Iterator<Item = &Self> {
        other.iter().filter(|t| self.matches_arc(t.dir.path()))
    }

    pub fn new(id: TabUid, target: NavTarget, existing_tabs: &[Self]) -> (Self, TabElement) {
        debug!("Opening tab {id:?} to {target:?}");
        // fetch metatada synchronously, even with a donor
        let settings = gui_run(|g| g.database.get(&target.dir));

        let element = TabElement::new(id.copy(), &target.dir);
        let dir = FlatDir::new(target.dir);
        let contents = Contents::new(settings.sort);
        let state = PaneState::for_jump(target.scroll);


        let mut t = Self {
            id,
            dir,
            pane: MP::Closed(state),

            settings,
            contents,
            past: Vec::new(),
            future: Vec::new(),
            element: element.clone(),
            search: None,
        };

        t.copy_from_donor(existing_tabs, &[]);

        (t, element)
    }

    pub fn cloned(id: TabUid, source: &Self) -> (Self, TabElement) {
        // Assumes inactive tabs cannot be cloned.
        let mut contents = Contents::new(source.settings.sort);
        let element = TabElement::new(id.copy(), Path::new(""));

        contents.clone_from(&source.contents, source.settings.sort);
        element.clone_from(&source.element);

        // Spinner will start spinning for search when pane is attached
        if !source.dir.state().loading() {
            element.imp().spinner.stop();
            element.imp().spinner.set_visible(false);
        }

        let search = source.search.as_ref().map(|s| s.clone_for(&contents));

        (
            Self {
                id,
                dir: source.dir.clone(),
                pane: MP::Closed(source.pane.clone_state(&source.contents)),

                settings: source.settings,
                contents,
                past: source.past.clone(),
                future: source.future.clone(),
                element: element.clone(),
                search,
            },
            element,
        )
    }

    fn copy_from_donor(&mut self, left: &[Self], right: &[Self]) -> bool {
        for t in left.iter().chain(right) {
            // Check for value equality, not reference equality
            if **self.dir.path() == **t.dir.path() {
                self.dir = t.dir.clone();
                self.contents.clone_from(&t.contents, self.settings.sort);
                self.element.clone_from(&t.element);

                if !self.dir.state().loading() {
                    self.element.imp().spinner.stop();
                    self.element.imp().spinner.set_visible(false);
                }

                return true;
            }
        }
        false
    }

    pub fn split(&mut self, orient: Orientation) -> Option<gtk::Paned> {
        self.pane.get().unwrap().split(orient)
    }

    fn open_search(&mut self, query: String) {
        trace!("Creating Search for {:?}", self.id);
        let mut search = Search::new(self.dir.path().clone(), &self.contents, query);

        let Some(pane) = self.pane.getm() else {
            self.search = Some(search);
            return;
        };

        self.element.search_title(self.dir.path());

        search.start_load();
        self.element.imp().spinner.start();
        self.element.imp().spinner.set_visible(true);

        pane.flat_to_search(
            search.query(),
            &search.contents().selection,
            search.filter.clone(),
            search.contents().filtered.clone().unwrap(),
        );

        self.search = Some(search);
    }

    fn close_search(&mut self) {
        trace!("Closing Search for {:?}", self.id);
        let Some(_search) = self.search.take() else {
            return;
        };

        if !self.loading() {
            self.element.imp().spinner.stop();
            self.element.imp().spinner.set_visible(false);
        }

        self.element.flat_title(self.dir.path());

        let Some(pane) = self.pane.getm() else {
            return;
        };

        pane.search_to_flat(self.dir.path(), &self.contents.selection);
    }

    // Starts a new search or updates an existing one with a new query.
    pub fn search(&mut self, query: String) {
        assert!(self.visible());

        if let Some(_search) = &mut self.search {
            let pane = self.pane.getm().unwrap();
            pane.update_search(&query);
            return;
        }

        self.past.push(self.current_history());
        self.future.clear();
        self.open_search(query);
    }

    // Only make this take &mut [Self] if truly necessary
    fn load(&mut self, left: &[Self], right: &[Self]) {
        let self_load = self.dir.start_load();
        let search_load = self.search.as_mut().map_or(false, Search::start_load);

        if self_load || search_load {
            self.element.imp().spinner.start();
            self.element.imp().spinner.set_visible(true);
        }

        if self_load {
            for t in self.matching(left).chain(self.matching(right)) {
                t.element.imp().spinner.start();
                t.element.imp().spinner.set_visible(true);
            }
        }
    }

    pub fn matches_snapshot(&self, snap: &SnapshotId) -> bool {
        if !Arc::ptr_eq(self.dir.path(), &snap.path) {
            return false;
        }


        self.dir.matches(snap)
    }

    fn apply_snapshot_inner(&mut self, snap: EntryObjectSnapshot) {
        if self.settings.display_mode == DisplayMode::Icons {
            if let Some(scroller) = self.pane.get().map(Pane::workaround_scroller) {
                if scroller.vscrollbar_policy() != gtk::PolicyType::Never {
                    error!("Locking scrolling to work around gtk crash");
                    scroller.set_vscrollbar_policy(gtk::PolicyType::Never);
                }
            }
        }

        if let Some(search) = &mut self.search {
            search.apply_flat_snapshot(snap.clone());
        }
        self.contents.apply_snapshot(snap);
    }

    pub fn apply_snapshot(&mut self, left: &mut [Self], right: &mut [Self], snap: DirSnapshot) {
        let start = Instant::now();
        assert!(self.matches_snapshot(&snap.id));
        let snap: EntryObjectSnapshot = snap.into();


        let force_search_sort = snap.had_search_updates;

        // The actual tab order doesn't matter here, so long as it's applied to every matching tab.
        // Avoid one clone by doing the other tabs first.
        self.matching_mut(right).for_each(|t| t.apply_snapshot_inner(snap.clone()));

        let s_id = snap.id.clone();
        let len = snap.entries.len();

        self.apply_snapshot_inner(snap);

        if force_search_sort {
            // We just sorted this tab and all exact matching tabs to the right.
            left.iter_mut().for_each(|t| t.search_sort_after_snapshot(&s_id.path));
            right
                .iter_mut()
                .filter(|t| !t.matches_arc(self.dir.path()))
                .for_each(|t| t.search_sort_after_snapshot(&s_id.path));
        }

        debug!(
            "Applied {:?} snapshot for {:?} with {len} items in {:?}",
            s_id.kind,
            self.dir.path(),
            start.elapsed()
        );

        if !s_id.kind.finished() {
            // Not completed, no changes to dir loading state.
            return;
        }

        // If we're finished, update all tabs to reflect this.
        let (updates, start) = self.dir.take_loaded();

        for u in updates {
            self.flat_update(left, right, u);
        }

        self.maybe_finish_load();
        // there will be no exact matches to the left.
        self.matching_mut(right).for_each(Self::maybe_finish_load);
        info!("Finished loading {:?} in {:?}", self.dir.path(), start.elapsed());
    }

    pub fn matches_search_snapshot(&self, snap: &SearchSnapshot) -> bool {
        if let Some(search) = &self.search {
            search.matches_snapshot(snap)
        } else {
            false
        }
    }

    pub fn apply_search_snapshot(&mut self, snap: SearchSnapshot) {
        let search = self.search.as_mut().unwrap();
        assert!(search.matches_snapshot(&snap));
        search.apply_search_snapshot(snap);
        self.maybe_finish_load();
    }

    pub fn matches_flat_update(&self, update: &Update) -> bool {
        // Not watching anything -> cannot match an update
        let Some(_watch) = self.dir.state().watched() else {
            return false;
        };

        Some(&**self.dir.path()) == update.path().parent()
    }

    pub fn matches_search_update(&self, update: &SearchUpdate) -> bool {
        // Not watching anything -> cannot match an update
        let Some(_watch) = self.dir.state().watched() else {
            return false;
        };

        let Some(search) = &self.search else {
            return false;
        };

        search.matches_update(update)
    }

    pub fn apply_search_update(&mut self, update: SearchUpdate) {
        self.search.as_mut().unwrap().apply_search_update(update);
    }

    fn search_sort_after_snapshot(&mut self, path: &Path) {
        let Some(search) = &mut self.search else {
            return;
        };

        if !path.starts_with(self.dir.path()) {
            return;
        }

        trace!("Re-sorting search for {:?} after flat snapshot", self.id);
        search.re_sort();
    }

    pub fn check_directory_deleted(&self, removed: &Path) -> bool {
        removed == &**self.dir.path()
    }

    // Need to handle this to avoid the case where a directory is "Loaded" but no watcher is
    // listening for events.
    // Even if the directory is recreated, the watcher won't work.
    pub fn handle_directory_deleted(&mut self, left: &[Self], right: &[Self]) -> bool {
        let cur_path = self.dir.path().clone();
        let mut path = &*cur_path;
        while !path.exists() || !path.is_dir() {
            path = path.parent().unwrap()
        }

        if path.exists() {
            // Honestly should just close the tab, but this means something is seriously broken.
            drop(self.change_location_flat(left, right, NavTarget::assume_dir(path)));
            true
        } else {
            false
        }
    }

    fn update_settings(&mut self) {
        if let Some(search) = &mut self.search {
            search.update_settings(self.settings);

            if let Some(p) = self.pane.getm() {
                p.update_settings(self.settings, search.contents())
            }
        } else if let Some(p) = self.pane.getm() {
            p.update_settings(self.settings, &self.contents)
        }
        self.save_settings();
    }

    pub fn update_sort(&mut self, sort: SortSettings) {
        self.settings.sort = sort;
        self.contents.sort(sort);
        self.update_settings();
    }

    pub fn update_display_mode(&mut self, mode: DisplayMode) {
        self.settings.display_mode = mode;
        self.update_settings();
    }

    // Changes location without managing history or view states.
    // Returns the navigation target if nothing happens
    // Will end a search.
    fn change_location_flat(
        &mut self,
        left: &[Self],
        right: &[Self],
        target: NavTarget,
    ) -> Option<NavTarget> {
        let was_search = self.search.is_some();

        if **self.dir.path() == *target.dir {
            if was_search {
                self.close_search();
                return None;
            }
            return Some(target);
        }

        if was_search {
            // Clear so we don't flicker
            self.contents.clear(self.settings.sort);
            self.close_search();
        }

        self.element.flat_title(&target.dir);

        self.pane.overwrite_state(PaneState::for_jump(target.scroll));
        self.dir = FlatDir::new(target.dir);

        if !self.copy_from_donor(left, right) {
            // We couldn't find any state to steal, so we know we're the only matching tab.

            let old_settings = self.settings;
            self.settings = gui_run(|g| g.database.get(self.dir.path()));

            // Deliberately do not clear or update self.contents here, not yet.
            // This allows us to keep something visible just a tiny bit longer.
            // Safe enough when the settings match.
            if was_search {
                // already cleared
            } else if self.pane.has_pane() && self.settings == old_settings {
                self.contents.mark_stale(self.settings.sort);
            } else {
                self.contents.clear(self.settings.sort);
            }
        }

        if let Some(pane) = self.pane.getm() {
            // Cannot be a search pane
            debug_assert!(self.search.is_none());
            pane.update_location(self.dir.path(), self.settings, &self.contents);
        }
        self.load(left, right);
        None
    }

    pub fn set_active(&mut self) {
        assert!(self.visible(), "Called set_active on a tab that isn't visible");
        self.pane.getm().unwrap().set_active(true);
        self.element.set_active(true);
    }

    pub fn set_inactive(&mut self) {
        if let Some(pane) = self.pane.getm() {
            pane.set_active(false);
        }
        self.element.set_active(false);
    }

    pub fn navigate(&mut self, left: &[Self], right: &[Self], target: NavTarget) {
        info!("Navigating {:?} from {:?} to {:?}", self.id, self.dir.path(), target);

        let history = self.current_history();

        let Some(unconsumed) = self.change_location_flat(left, right, target) else {
            self.past.push(history);
            self.future.clear();
            return;
        };

        let Some(path) = unconsumed.scroll else {
            return;
        };

        // Location didn't change, treat this as a jump
        let mut state = self.pane.clone_state(&self.contents);

        state.scroll_pos = Some(ScrollPosition { path, index: 0 });
        self.pane.overwrite_state(state);

        self.apply_pane_state();
    }

    pub fn parent(&mut self, left: &[Self], right: &[Self]) {
        let Some(parent) = self.dir.path().parent() else {
            warn!("No parent for {:?}", self.dir.path());
            return;
        };

        let target = NavTarget::assume_dir(parent);
        self.navigate(left, right, target);
    }

    pub fn forward(&mut self, left: &[Self], right: &[Self]) {
        let Some(next) = self.future.pop() else {
            warn!("No future for {:?} to go forward to", self.id);
            return;
        };

        info!("Forwards in tab {:?} to {next:?}", self.id);
        let history = self.current_history();

        self.past.push(history);

        // Shouldn't be a jump, could be a search starting/ending.
        if next.location == *self.dir.path() {
            if let Some(query) = next.search {
                self.open_search(query);
            } else {
                self.close_search();
            }
            self.pane.overwrite_state(next.state);
            self.apply_pane_state();
            return;
        }

        let target = NavTarget::assume_dir(next.location);

        if let Some(unconsumed) = self.change_location_flat(left, right, target) {
            error!(
                "Failed to change location {unconsumed:?} after already checking it was a change."
            );
            self.pane.overwrite_state(next.state);
            self.apply_pane_state();
            return;
        }

        if let Some(query) = next.search {
            self.open_search(query);
        } else {
            self.close_search();
        }

        self.pane.overwrite_state(next.state);
        self.apply_pane_state()
    }

    pub(super) fn back(&mut self, left: &[Self], right: &[Self]) {
        let Some(prev) = self.past.pop() else {
            warn!("No history for {:?} to go back to", self.id);
            return;
        };

        info!("Back in tab {:?} to {prev:?}", self.id);
        let history = self.current_history();

        self.future.push(history);

        // Shouldn't be a jump, could be a search starting/ending.
        if prev.location == *self.dir.path() {
            if let Some(query) = prev.search {
                self.open_search(query);
            } else {
                self.close_search();
            }
            self.pane.overwrite_state(prev.state);
            self.apply_pane_state();
            return;
        }

        let target = NavTarget::assume_dir(prev.location);

        if let Some(unconsumed) = self.change_location_flat(left, right, target) {
            error!(
                "Failed to change location {unconsumed:?} after already checking it was a change."
            );
            self.pane.overwrite_state(prev.state);
            self.apply_pane_state();
            return;
        }

        if let Some(query) = prev.search {
            self.open_search(query);
        } else {
            self.close_search();
        }

        self.pane.overwrite_state(prev.state);
        self.apply_pane_state()
    }

    // Goes to the child directory if one was previous open or if there's only one.
    pub(super) fn child(&mut self, left: &[Self], right: &[Self]) {
        if self.search.is_some() {
            warn!("Can't move to child in search tab {:?}", self.id);
            return;
        }

        // Try the past and future first.
        if Some(&**self.dir.path()) == self.past.last().and_then(|h| h.location.parent()) {
            info!("Found past entry for child dir while handling Child in {:?}", self.id);
            return self.back(left, right);
        }
        if Some(&**self.dir.path()) == self.future.last().and_then(|h| h.location.parent()) {
            info!("Found future entry for child dir while handling Child in {:?}", self.id);
            return self.forward(left, right);
        }

        if self.contents.selection.n_items() == 0 {
            warn!("No children in {:?} to navigate to.", self.id);
            return;
        }

        let first = self.contents.selection.item(0).and_downcast::<EntryObject>().unwrap();
        if !first.get().dir() {
            warn!("No subdirectories in {:?} to navigate to.", self.id);
            return;
        }

        if self.contents.selection.n_items() >= 2 {
            let second = self.contents.selection.item(0).and_downcast::<EntryObject>().unwrap();
            if second.get().dir() {
                warn!("More than one subdirectory found in {:?}, cannot use Child", self.id);
                return;
            }
        }

        let target = NavTarget::assume_dir(first.get().abs_path.clone());
        info!(
            "Navigating {:?} from {:?} to sole child directory {:?}",
            self.id,
            self.dir.path(),
            target
        );

        let history = self.current_history();

        self.past.push(history);
        self.pane.overwrite_state(PaneState::default());

        if let Some(unconsumed) = self.change_location_flat(left, right, target) {
            error!(
                "Failed to change location {unconsumed:?} after already checking it was a change."
            );
            self.apply_pane_state();
            return;
        }

        self.apply_pane_state()
    }

    fn current_history(&self) -> HistoryEntry {
        let state = if let Some(search) = &self.search {
            self.pane.clone_state(search.contents())
        } else {
            self.pane.clone_state(&self.contents)
        };
        let search = self.search.as_ref().map(|s| s.query().0.borrow().clone());
        HistoryEntry { location: self.dir(), search, state }
    }

    fn maybe_finish_load(&mut self) {
        if self.loaded() {
            let e = self.element.imp();
            e.spinner.stop();
            e.spinner.set_visible(false);
            self.apply_pane_state();
        }
    }

    fn apply_pane_state(&mut self) {
        if !self.loaded() {
            trace!("Deferring applying pane state to flat pane until loading is done.");
            return;
        }

        let MP::Pending(pane, state) = &mut self.pane else {
            debug!(
                "Ignoring apply_pane_state on tab {:?} without pending state and ready pane {:?}",
                self.id,
                self.element.imp().title.text()
            );
            return;
        };

        info!("Applying {:?} to tab {:?}", state, self.id);

        if self.settings.display_mode == DisplayMode::Icons {
            error!("Unsetting GTK crash workaround");
        }
        // Unconditionally unset it in case mode was swapped.
        pane.workaround_scroller().set_vscrollbar_policy(gtk::PolicyType::Automatic);

        let state = self.pane.must_resolve_pending();
        let pane = self.pane.getm().unwrap();

        if let Some(search) = &self.search {
            pane.apply_state(state, search.contents());
        } else {
            pane.apply_state(state, &self.contents);
        }
    }

    pub fn attach_pane<F: FnOnce(&Widget)>(&mut self, left: &[Self], right: &[Self], attach: F) {
        if self.visible() {
            error!("Pane {:?} already displayed", self.id);
            return;
        }

        if let Some(search) = &self.search {
            let pane = Pane::new_search(
                self.id(),
                search.query(),
                self.settings,
                &search.contents().selection,
                search.filter.clone(),
                search.contents().filtered.clone().unwrap(),
                attach,
            );
            self.pane.set_pane(pane);
        } else {
            let pane = Pane::new_flat(
                self.id(),
                self.dir.path(),
                self.settings,
                &self.contents.selection,
                attach,
            );
            self.pane.set_pane(pane);
        }

        self.element.set_pane_visible(true);

        self.load(left, right);
        self.apply_pane_state();
    }

    // Doesn't start loading
    pub fn replace_pane(&mut self, left: &[Self], right: &[Self], old: Pane) {
        info!("Replacing pane for {:?} with pane from {:?}", old.tab(), self.id());
        if self.visible() {
            error!("Pane already displayed, dropping instead.");
            return;
        }

        if let Some(search) = &self.search {
            let pane = Pane::new_search(
                self.id(),
                search.query(),
                self.settings,
                &search.contents().selection,
                search.filter.clone(),
                search.contents().filtered.clone().unwrap(),
                |new| old.replace_with_other_tab(new),
            );

            self.pane.set_pane(pane);
        } else {
            let pane = Pane::new_flat(
                self.id(),
                self.dir.path(),
                self.settings,
                &self.contents.selection,
                |new| old.replace_with_other_tab(new),
            );

            self.pane.set_pane(pane);
        }

        self.element.set_pane_visible(true);

        self.load(left, right);
        self.apply_pane_state();
    }

    pub fn close_pane(&mut self) {
        drop(self.take_pane());
        self.element.set_active(false);
    }

    pub fn next_of_kin_by_pane(&self) -> Option<TabId> {
        self.pane.get().and_then(Pane::next_of_kin)
    }

    pub fn take_pane(&mut self) -> Pane {
        assert!(self.pane.get().is_some(), "Called take_pane on {:?} with no pane", self.id);

        self.element.set_pane_visible(false);
        if let Some(search) = &self.search {
            self.pane.take_pane(search.contents())
        } else {
            self.pane.take_pane(&self.contents)
        }
    }

    fn save_settings(&self) {
        gui_run(|g| {
            g.database.store(self.dir.path(), self.settings);
        });
    }

    pub fn flat_update(&mut self, left: &mut [Self], right: &mut [Self], up: Update) {
        assert!(self.matches_flat_update(&up));

        let Some(update) = self.dir.consume(up) else {
            return;
        };

        // If it exists in any tab (search or flat)
        let existing_global = EntryObject::lookup(update.path());
        // If it exists in flat tabs (which all share the same state).
        let local_position = existing_global
            .clone()
            .and_then(|eg| self.contents.total_position_by_sorted(&eg.get()));


        // If something exists globally but not locally, it needs to be applied to search tabs.
        let mut search_mutate = None;

        let partial = match (update, local_position, existing_global) {
            // It simply can't exist locally but not globally
            (_, Some(_), None) => unreachable!(),
            (Update::Entry(entry), Some(pos), Some(obj)) => {
                let Some(old) = obj.update(entry) else {
                    // An unimportant update.
                    // No need to check other tabs.
                    return;
                };

                self.contents.reinsert_updated(pos, &obj, &old);

                trace!("Updated {:?} from event", old.abs_path);
                PartiallyAppliedUpdate::Mutate(old, obj)
            }
            (Update::Entry(entry), None, Some(obj)) => {
                // This means the element already existed in a search tab, somewhere else, and
                // we're updating it.
                if let Some(old) = obj.update(entry) {
                    // It's an update for (some) search tabs, but an insertion for flat tabs.
                    //
                    // This means that a different search tab happened to read a newly created file
                    // before it was inserted into this tab.
                    warn!(
                        "Search tab read item {:?} before it was inserted into a flat tab. This \
                         should be uncommon.",
                        old.abs_path
                    );
                    search_mutate = Some(PartiallyAppliedUpdate::Mutate(old, obj.clone()));
                }


                self.contents.insert(&obj);
                trace!("Inserted existing {:?} from event", obj.get().abs_path);
                PartiallyAppliedUpdate::Insert(obj)
            }
            (Update::Entry(entry), None, None) => {
                let new = EntryObject::new(entry);

                self.contents.insert(&new);
                trace!("Inserted new {:?} from event", new.get().abs_path);
                PartiallyAppliedUpdate::Insert(new)
            }
            (Update::Removed(path), Some(i), Some(obj)) => {
                self.contents.remove(i);
                trace!("Removed {:?} from event", path);
                PartiallyAppliedUpdate::Delete(obj)
            }
            (Update::Removed(path), None, Some(obj)) => {
                // So rare it's not worth optimizing
                warn!("Removed search-only {:?} from event. This should be uncommon.", path);
                PartiallyAppliedUpdate::Delete(obj)
            }
            (Update::Removed(path), None, None) => {
                // Unusual case, probably shouldn't happen often.
                // Maybe if something is removed while loading the directory before we read it.
                warn!("Got removal for {path:?} which wasn't present");
                return;
            }
        };


        if let Some(search) = &mut self.search {
            search.finish_flat_update(&partial)
        }

        // None of the tabs in `left` are an exact match.
        for t in self.matching_mut(right) {
            t.contents.finish_update(&partial);
            if let Some(search) = &mut t.search {
                search.finish_flat_update(&partial)
            }
        }

        // Apply mutations to any matching search tabs, even to the left.
        let mutate = search_mutate.unwrap_or(partial);
        if !matches!(mutate, PartiallyAppliedUpdate::Mutate(..)) {
            return;
        }

        for t in left
            .iter_mut()
            .chain(right.iter_mut())
            .filter(|t| !t.matches_arc(self.dir.path()))
        {
            if let Some(search) = &mut t.search {
                search.handle_subdir_flat_mutate(&mutate);
            }
        }
    }
}


// These must share fate.
mod flat_dir {
    use std::cell::{Ref, RefCell};
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::Arc;
    use std::time::Instant;

    use crate::com::{ManagerAction, SnapshotId, SnapshotKind, Update};
    use crate::gui::gui_run;

    #[derive(Debug)]
    pub struct WatchedDir {
        path: Arc<Path>,
        cancel: Arc<AtomicBool>,
    }

    impl Drop for WatchedDir {
        fn drop(&mut self) {
            self.cancel.store(true, Relaxed);
            debug!("Stopped watching {:?}", self.path);
            gui_run(|g| {
                g.send_manager(ManagerAction::Unwatch(self.path.clone()));
            });
        }
    }

    impl WatchedDir {
        fn start(path: Arc<Path>) -> Self {
            let cancel: Arc<AtomicBool> = Arc::default();
            gui_run(|g| {
                g.send_manager(ManagerAction::Open(path.clone(), cancel.clone()));
            });

            Self { path, cancel }
        }
    }


    #[derive(Debug, Default)]
    pub enum DirState {
        #[default]
        Unloaded,
        Loading {
            watch: WatchedDir,
            pending_updates: Vec<Update>,
            start: Instant,
        },
        // ReOpening{} -- Transitions to Opening once a Start arrives, or Opened with Complete
        // UnloadedWhileOpening -- takes snapshots and drops them
        Loaded(WatchedDir),
    }

    impl DirState {
        pub const fn watched(&self) -> Option<&WatchedDir> {
            match self {
                Self::Unloaded => None,
                Self::Loading { watch, .. } | Self::Loaded(watch) => Some(watch),
            }
        }

        pub const fn loading(&self) -> bool {
            match self {
                Self::Unloaded | Self::Loaded(_) => false,
                Self::Loading { .. } => true,
            }
        }

        pub const fn loaded(&self) -> bool {
            match self {
                Self::Unloaded | Self::Loading { .. } => false,
                Self::Loaded(_) => true,
            }
        }
    }


    #[derive(Debug, Clone)]
    pub struct FlatDir {
        path: Arc<Path>,
        state: Rc<RefCell<DirState>>,
    }

    impl FlatDir {
        pub fn new(path: Arc<Path>) -> Self {
            Self { path, state: Rc::default() }
        }

        pub const fn path(&self) -> &Arc<Path> {
            &self.path
        }

        pub fn state(&self) -> Ref<'_, DirState> {
            self.state.borrow()
        }

        pub fn matches(&self, snap: &SnapshotId) -> bool {
            use SnapshotKind::*;

            let sb = self.state.borrow();

            match (&snap.kind, &*sb) {
                (_, DirState::Unloaded | DirState::Loaded(_)) => false,
                (Complete | Start | Middle | End, DirState::Loading { watch, .. }) => {
                    Arc::ptr_eq(&watch.cancel, &snap.id)
                }
            }
        }

        // Returns the update if it wasn't consumed
        pub fn consume(&self, up: Update) -> Option<Update> {
            let mut sb = self.state.borrow_mut();
            match &mut *sb {
                DirState::Unloaded => {
                    warn!("Dropping update {up:?} for unloaded tab.");
                    None
                }
                DirState::Loading { pending_updates, .. } => {
                    pending_updates.push(up);
                    None
                }
                DirState::Loaded(_) => Some(up),
            }
        }

        pub fn start_load(&self) -> bool {
            let mut sb = self.state.borrow_mut();
            let dstate = &mut *sb;
            match dstate {
                DirState::Loading { .. } | DirState::Loaded(_) => false,
                DirState::Unloaded => {
                    let start = Instant::now();
                    debug!("Opening directory for {:?}", self.path);
                    let watch = WatchedDir::start(self.path.clone());

                    *dstate = DirState::Loading {
                        watch,
                        pending_updates: Vec::new(),
                        start,
                    };
                    true
                }
            }
        }

        pub fn take_loaded(&self) -> (Vec<Update>, Instant) {
            let mut sb = self.state.borrow_mut();
            match std::mem::take(&mut *sb) {
                DirState::Unloaded | DirState::Loaded(_) => unreachable!(),
                DirState::Loading { watch, pending_updates, start } => {
                    *sb = DirState::Loaded(watch);
                    (pending_updates, start)
                }
            }
        }
    }
}
