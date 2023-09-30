use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use ahash::{AHashMap, AHashSet};
use gtk::gio::Cancellable;
use gtk::prelude::{CastNone, ListModelExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{SelectionModelExt, WidgetExt};
use gtk::{glib, AlertDialog, MultiSelection, Orientation, PopoverMenu, Widget};
use TabPane as TP;

use self::flat_dir::FlatDir;
use super::contents::Contents;
use super::element::TabElement;
use super::id::{TabId, TabUid};
use super::list::Group;
use super::pane::Pane;
use super::search::Search;
use super::{HistoryEntry, NavTarget, PaneState, ScrollPosition};
use crate::com::{
    DirSettings, DirSnapshot, DisplayMode, EntryObject, EntryObjectSnapshot, GetEntry,
    ManagerAction, SearchSnapshot, SearchUpdate, SortDir, SortMode, SortSettings,
};
use crate::config::CONFIG;
use crate::database::SavedGroup;
use crate::gui::clipboard::{handle_clipboard, handle_drop, ClipboardOp, SelectionProvider};
use crate::gui::operations::{self, Kind, Outcome};
use crate::gui::tabs::PartiallyAppliedUpdate;
use crate::gui::{applications, gui_run, show_error, show_warning, tabs_run, Selected, Update};

/* This efficiently supports multiple tabs being open to the same directory with different
 * settings.
 *
 * It's a little more complicated than if they all were required to have the same settings, but
 * the only way to make this code actually simple is to completely abandon efficiency -
 * especially in my common case of opening a new tab.
 */


#[derive(Debug)]
enum TabPane {
    Pane(Pane),
    Pending(Pane, PaneState),
    Detached {
        pane: Pane,
        state: PaneState,
        // If res is present, it was in the "Pane" state.
        // If res is present and matches the resolution on reattach, we don't need to apply the
        // state.
        // res: Option<(u32, u32)>,
        pending: bool,
    },
    Empty,
}

impl Deref for TabPane {
    type Target = Pane;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Pane(p) | Self::Pending(p, _) | Self::Detached { pane: p, .. } => p,
            Self::Empty => unreachable!(),
        }
    }
}

impl DerefMut for TabPane {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Pane(p) | Self::Pending(p, _) | Self::Detached { pane: p, .. } => p,
            Self::Empty => unreachable!(),
        }
    }
}

impl TabPane {
    const fn visible(&self) -> bool {
        match &self {
            Self::Pane(_) | Self::Pending(..) => true,
            Self::Detached { .. } => false,
            Self::Empty => unreachable!(),
        }
    }

    const fn get_visible(&self) -> Option<&Pane> {
        match self {
            Self::Pane(p) | Self::Pending(p, _) => Some(p),
            Self::Detached { .. } => None,
            Self::Empty => unreachable!(),
        }
    }

    fn get_visible_mut(&mut self) -> Option<&mut Pane> {
        match self {
            Self::Pane(p) | Self::Pending(p, _) => Some(p),
            Self::Detached { .. } => None,
            Self::Empty => unreachable!(),
        }
    }

    fn clone_state(&self, list: &Contents) -> PaneState {
        match self {
            Self::Pane(p) => p.get_state(list),
            Self::Detached { state, .. } | Self::Pending(_, state) => state.clone(),
            Self::Empty => unreachable!(),
        }
    }

    fn overwrite_state(&mut self, new_state: PaneState) {
        match self {
            Self::Pane(_p) => {
                let temp = Self::Empty;
                let old = std::mem::replace(self, temp);
                let Self::Pane(pane) = old else {
                    unreachable!()
                };
                *self = Self::Pending(pane, new_state);
            }
            Self::Detached { state, .. } | Self::Pending(_, state) => *state = new_state,
            Self::Empty => unreachable!(),
        }
    }

    fn make_visible(&mut self) {
        match self {
            Self::Detached { .. } => {
                let old = std::mem::replace(self, Self::Empty);
                let Self::Detached { pane, state, pending } = old else {
                    unreachable!()
                };
                pane.set_visible(true);
                *self = if pending { Self::Pending(pane, state) } else { Self::Pane(pane) };
            }
            Self::Pane(_) | Self::Pending(..) | Self::Empty => unreachable!(),
        }
    }

    fn mark_detached(&mut self, list: &Contents) {
        match std::mem::replace(self, Self::Empty) {
            Self::Detached { .. } | Self::Empty => unreachable!(),
            Self::Pane(pane) => {
                let state = pane.get_state(list);
                *self = Self::Detached { pane, state, pending: false };
            }
            Self::Pending(pane, state) => {
                *self = Self::Detached { pane, state, pending: true };
            }
        }
    }

    fn snapshot_pane(&mut self, list: &Contents) {
        let state = match self {
            Self::Detached { pending, .. } => return *pending = true,
            Self::Pending(..) => return,
            Self::Empty => unreachable!(),
            Self::Pane(p) => p.get_state(list),
        };
        let old = std::mem::replace(self, Self::Empty);
        let Self::Pane(p) = old else { unreachable!() };

        *self = Self::Pending(p, state);
    }

    fn must_resolve_pending(&mut self) -> PaneState {
        let Self::Pending(_p, _state) = self else {
            unreachable!()
        };
        let old = std::mem::replace(self, Self::Empty);
        let Self::Pending(pane, state) = old else {
            unreachable!()
        };

        *self = Self::Pane(pane);
        state
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
    group: Option<Rc<RefCell<Group>>>,

    dir: FlatDir,
    pane: TabPane,

    settings: DirSettings,
    contents: Contents,
    past: Vec<HistoryEntry>,
    future: Vec<HistoryEntry>,
    element: TabElement,

    // Each tab can only be open in one pane at once.
    // In theory we could do many-to-many but it's too niche.
    search: Option<Search>,
}

#[derive(Debug)]
pub(super) struct ClosedTab {
    id: TabUid,
    // group: GroupId,
    pub after: Option<TabId>,
    current: HistoryEntry,
    past: Vec<HistoryEntry>,
    future: Vec<HistoryEntry>,
}

impl Tab {
    pub const fn id(&self) -> TabId {
        self.id.copy()
    }

    // Returns the group only if it's got children
    pub fn multi_tab_group(&self) -> Option<Rc<RefCell<Group>>> {
        self.group.as_ref().filter(|g| !g.borrow().children.is_empty()).cloned()
    }

    pub fn get_or_start_group(&mut self) -> Rc<RefCell<Group>> {
        self.group
            .get_or_insert_with(|| {
                Rc::new(RefCell::new(Group {
                    parent: self.id.copy(),
                    children: Vec::new(),
                }))
            })
            .clone()
    }

    pub fn force_group(&mut self, group: &Rc<RefCell<Group>>) {
        if self.group.as_ref().is_some_and(|g| Rc::ptr_eq(g, group)) {
            return;
        }

        if group.borrow().parent == self.id() {
            unreachable!()
        }

        group.borrow_mut().children.push(self.id());
        self.group = Some(group.clone());
        self.element.set_child(true);
    }

    pub const fn visible(&self) -> bool {
        self.pane.visible()
    }

    pub fn unloaded(&self) -> bool {
        self.dir.state().unloaded()
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

    const fn visible_contents(&self) -> &Contents {
        if let Some(search) = &self.search { search.contents() } else { &self.contents }
    }

    const fn visible_selection(&self) -> &MultiSelection {
        if let Some(search) = &self.search {
            &search.contents().selection
        } else {
            &self.contents.selection
        }
    }

    fn matching_mut<'a>(&'a self, other: &'a mut [Self]) -> impl Iterator<Item = &mut Self> {
        other.iter_mut().filter(|t| self.matches_arc(t.dir.path()))
    }

    fn matching<'a>(&'a self, other: &'a [Self]) -> impl Iterator<Item = &Self> {
        other.iter().filter(|t| self.matches_arc(t.dir.path()))
    }

    pub fn new(
        id: TabUid,
        target: NavTarget,
        existing_tabs: &[Self],
        insert: impl FnOnce(&Widget),
    ) -> (Self, TabElement) {
        debug!("Opening tab {id:?} to {target:?}");
        // fetch metatada synchronously, even with a donor
        let settings = gui_run(|g| g.database.get(target.dir.clone()));

        let element = TabElement::new(id.copy(), &target.dir);
        let dir = FlatDir::new(target.dir);
        let contents = Contents::new(settings.sort);
        let state = PaneState::for_jump(target.scroll);

        let pane = Pane::new_flat(id.copy(), dir.path(), settings, &contents.selection, insert);

        let mut t = Self {
            id,
            group: None,

            dir,
            pane: TP::Detached { pane, state, pending: true },

            settings,
            contents,
            past: Vec::new(),
            future: Vec::new(),
            element: element.clone(),
            search: None,
        };

        t.copy_flat_from_donor(existing_tabs, &[]);

        (t, element)
    }

    pub fn cloned(id: TabUid, source: &Self, insert: impl FnOnce(&Widget)) -> (Self, TabElement) {
        // Assumes inactive tabs cannot be cloned.
        let mut contents = Contents::new(source.settings.sort);
        let element = TabElement::new(id.copy(), Path::new(""));

        contents.clone_from(&source.contents, source.settings.sort);
        element.clone_from(&source.element);

        // Spinner will start spinning for search when pane is attached
        // It's possible for this spinner to be shown, then stop spinning, then start spinning
        // again once the tab is selected.
        if source.dir.state().loading() {
            element.spin();
        } else {
            element.stop_spin();
        }

        let search = source.search.as_ref().map(|s| s.clone_for(&contents));

        let pane = if let Some(search) = &search {
            Pane::new_search(
                id.copy(),
                search.query(),
                source.settings,
                &search.contents().selection,
                search.filter.clone(),
                search.contents().filtered.clone().unwrap(),
                insert,
            )
        } else {
            Pane::new_flat(
                id.copy(),
                source.dir.path(),
                source.settings,
                &contents.selection,
                insert,
            )
        };

        let state = source.pane.clone_state(&source.contents);

        (
            Self {
                id,
                group: None,

                dir: source.dir.clone(),
                pane: TP::Detached { pane, state, pending: true },

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

    pub fn reopen(
        closed: ClosedTab,
        existing_tabs: &[Self],
        insert: impl FnOnce(&Widget),
    ) -> (Self, TabElement) {
        debug!("Reopening closed tab {closed:?}");
        let settings = gui_run(|g| g.database.get(closed.current.location.clone()));

        let element = TabElement::new(closed.id.copy(), &closed.current.location);
        let dir = FlatDir::new(closed.current.location);
        let contents = Contents::new(settings.sort);

        let pane =
            Pane::new_flat(closed.id.copy(), dir.path(), settings, &contents.selection, insert);

        let mut t = Self {
            id: closed.id,
            // TODO [group]
            group: None,

            dir,
            pane: TP::Detached {
                pane,
                state: closed.current.state,
                pending: true,
            },

            settings,
            contents,
            past: closed.past,
            future: closed.future,
            element: element.clone(),
            search: None,
        };

        t.copy_flat_from_donor(existing_tabs, &[]);

        // Open search, if applicable, after copying flat state.
        if let Some(query) = closed.current.search {
            t.open_search(query);
        }

        (t, element)
    }

    fn copy_flat_from_donor(&mut self, left: &[Self], right: &[Self]) -> bool {
        for t in left.iter().chain(right) {
            // Check for value equality, not reference equality
            if **self.dir.path() == **t.dir.path() {
                self.dir = t.dir.clone();
                self.contents.clone_from(&t.contents, self.settings.sort);

                if self.dir.state().loading() {
                    self.element.spin();
                } else {
                    self.element.stop_spin();
                }

                return true;
            }
        }
        false
    }

    #[must_use]
    pub fn split(
        &mut self,
        orient: Orientation,
        forced: bool,
    ) -> Option<(gtk::Paned, Rc<RefCell<Group>>)> {
        let Some(paned) = self.pane.split(orient, forced) else {
            return None;
        };


        Some((paned, self.get_or_start_group()))
    }

    fn open_search(&mut self, query: String) {
        trace!("Creating Search for {:?}", self.id);
        let mut search = Search::new(self.dir.path().clone(), &self.contents, query);

        self.element.search_title(self.dir.path());

        search.start_load();
        self.element.spin();

        self.pane.flat_to_search(
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
            self.element.stop_spin();
        }

        self.element.flat_title(self.dir.path());

        self.pane.search_to_flat(self.dir.path(), &self.contents.selection);
    }

    // Starts a new search or updates an existing one with a new query.
    pub fn search(&mut self, query: String) {
        assert!(self.visible());

        if let Some(_search) = &mut self.search {
            self.pane.update_search(&query);
            return;
        }

        self.past.push(self.current_history());
        self.future.clear();
        self.open_search(query);
    }

    // Only make this take &mut [Self] if truly necessary
    fn load(&mut self, left: &[Self], right: &[Self]) {
        let self_load = self.dir.start_load(self.settings.sort);
        let search_load = self.search.as_mut().map_or(false, Search::start_load);

        if self_load || search_load {
            self.element.spin();
        }

        if self_load {
            for t in self.matching(left).chain(self.matching(right)) {
                t.element.spin();
            }
        }
    }

    // This is the easiest implementation, but it is clumsy
    pub fn unload_unchecked(&mut self) {
        if let Some(pane) = self.pane.get_visible() {
            pane.move_active_focus_to_text();
        }

        if let Some(search) = &mut self.search {
            self.pane.snapshot_pane(search.contents());
            search.unload(self.settings.sort);
        } else {
            self.pane.snapshot_pane(&self.contents);
        }

        self.contents.clear(self.settings.sort);
        self.element.stop_spin();
        self.dir.unload();
    }

    pub fn reload_visible(&mut self, left: &[Self], right: &[Self]) {
        if self.visible() {
            self.load(left, right);
        }
    }

    pub fn matches_watch(&self, id: &Arc<AtomicBool>) -> bool {
        self.dir.matches(id)
    }

    pub fn mark_watch_started(&self, id: Arc<AtomicBool>) {
        self.dir.mark_watch_started(id);
    }

    fn apply_snapshot_inner(&mut self, snap: EntryObjectSnapshot) {
        if self.settings.display_mode == DisplayMode::Icons
            && self.pane.workaround_scroller().vscrollbar_policy() != gtk::PolicyType::Never
        {
            warn!("Locking scrolling to work around gtk crash");
            self.pane.workaround_scroller().set_vscrollbar_policy(gtk::PolicyType::Never);
        }

        if let Some(search) = &mut self.search {
            search.apply_flat_snapshot(snap.clone());
        }
        self.contents.apply_snapshot(snap);
    }

    pub fn apply_snapshot(&mut self, left: &mut [Self], right: &mut [Self], snap: DirSnapshot) {
        let start = Instant::now();
        assert!(self.matches_watch(&snap.id.id));
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

    pub fn overlaps_other_search_update(&self, update: &SearchUpdate) -> bool {
        self.matches_flat_update(&update.update)
            || (self.search.as_ref().is_some_and(|s| s.overlaps_other_update(update)))
    }

    pub fn apply_search_update(&mut self, update: SearchUpdate, allow_mutation: bool) {
        self.search.as_mut().unwrap().apply_search_update(update, allow_mutation);
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
        self.contents.sort(self.settings.sort);

        if let Some(search) = &mut self.search {
            search.update_settings(self.settings);

            self.pane.update_settings(self.settings, search.contents());
        } else {
            self.pane.update_settings(self.settings, &self.contents)
        }
        self.save_settings();
    }

    pub fn update_display_mode(&mut self, mode: DisplayMode) {
        self.settings.display_mode = mode;
        self.update_settings();
    }

    pub fn update_sort(&mut self, sort: SortSettings) {
        self.settings.sort = sort;
        self.update_settings();
    }

    pub fn update_sort_mode(&mut self, mode: SortMode) {
        self.settings.sort.mode = mode;
        self.update_settings();
    }

    pub fn update_sort_dir(&mut self, dir: SortDir) {
        self.settings.sort.direction = dir;
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

        if *self.dir.path() == target.dir {
            if was_search {
                self.close_search();
                return None;
            }
            return Some(target);
        }


        self.element.flat_title(&target.dir);

        self.pane.overwrite_state(PaneState::for_jump(target.scroll));
        self.dir = FlatDir::new(target.dir);

        let old_settings = self.settings;
        self.settings = gui_run(|g| g.database.get(self.dir.path().clone()));

        if was_search {
            // Clear so we don't flicker
            self.contents.clear(self.settings.sort);
        }

        if !self.copy_flat_from_donor(left, right) {
            // We couldn't find any state to steal, so we know we're the only matching tab.

            if was_search {
                // Contents were cleared above
            } else if self.pane.visible() && self.settings == old_settings {
                // Deliberately do not clear or update self.contents here, not yet.
                // This allows us to keep something visible just a tiny bit longer.
                // Safe enough when the settings match.
                self.contents.mark_stale(self.settings.sort);
            } else {
                self.contents.clear(self.settings.sort);
            }
        }

        if was_search {
            self.close_search();
        }

        // Cannot be a search pane
        debug_assert!(self.search.is_none());
        self.pane.update_location(self.dir.path(), self.settings, &self.contents);
        self.load(left, right);
        None
    }

    pub fn set_active(&mut self) {
        assert!(self.visible(), "Called set_active on a tab that isn't visible");
        self.pane.set_active(true);
        self.element.set_active(true);
    }

    pub fn set_inactive(&mut self) {
        self.pane.set_active(false);
        self.element.set_active(false);
    }

    pub fn activate(&self) {
        let display = self.element.display();
        applications::open(self.id(), &display, self.visible_selection().into(), true);
    }

    pub fn open_default(&self) {
        let display = self.element.display();
        applications::open(self.id(), &display, self.visible_selection().into(), false);
    }

    pub fn open_with(&self) {
        gui_run(|g| g.open_with(self.visible_selection().into()));
    }

    pub fn select_if_not(&self, eo: EntryObject) {
        let contents =
            if let Some(search) = &self.search { search.contents() } else { &self.contents };

        // Should not be possible to call this with an item not in this tab.
        let pos = contents.filtered_position_by_sorted(&eo.get()).unwrap();
        if !contents.selection.is_selected(pos) {
            info!("Updating selection to clicked item {:?}", &*eo.get().name);
            contents.selection.select_item(pos, true);
        }
    }

    fn seek_inner(&mut self, fragment: &str, range: impl Iterator<Item = u32>) {
        // Smart case seeking?
        // Prefix instead?
        let fragment = fragment.to_lowercase();
        let contents = self.visible_contents();

        for i in range {
            let eo = contents.selection.item(i).and_downcast::<EntryObject>().unwrap();
            if !eo.matches_seek(&fragment) {
                continue;
            }

            // TODO [gtk4.12]-- explicitly focus, which doesn't work yet.
            debug!("Seeking to {:?}", &*eo.get().name);

            contents.selection.select_item(i, true);

            let mut state = self.pane.clone_state(contents);
            state.scroll_pos = Some(ScrollPosition {
                path: eo.get().abs_path.clone(),
                index: i,
            });
            self.pane.overwrite_state(state);

            self.apply_pane_state();
            return;
        }
    }

    pub fn seek(&mut self, fragment: &str) {
        let contents =
            if let Some(search) = &self.search { search.contents() } else { &self.contents };
        self.seek_inner(fragment, 0..contents.selection.n_items());
    }

    pub fn seek_next(&mut self, fragment: &str) {
        let contents =
            if let Some(search) = &self.search { search.contents() } else { &self.contents };

        // No real fallback if there's no selection
        let sel = contents.selection.selection();
        if !sel.is_empty() {
            self.seek_inner(fragment, sel.nth(0) + 1..contents.selection.n_items());
        } else {
            self.seek_inner(fragment, 0..contents.selection.n_items());
        }
    }

    pub fn seek_prev(&mut self, fragment: &str) {
        let contents =
            if let Some(search) = &self.search { search.contents() } else { &self.contents };

        // No real fallback if there's no selection
        let sel = contents.selection.selection();
        if !sel.is_empty() {
            self.seek_inner(fragment, (0..sel.nth(0)).rev());
        } else {
            self.seek_inner(fragment, 0..contents.selection.n_items());
        }
    }

    pub fn clear_selection(&self) {
        let contents =
            if let Some(search) = &self.search { search.contents() } else { &self.contents };

        contents.selection.unselect_all();
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

        let target = NavTarget::assume_jump(parent, self.dir());
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
            "Navigating {:?} from {:?} to sole child directory {target:?}",
            self.id,
            self.dir.path(),
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
        // We can be done loading without being loaded (flat loaded + search unloaded)
        if !self.loading() {
            self.element.stop_spin()
        }

        if self.loaded() {
            self.apply_pane_state();
        }
    }

    fn apply_pane_state(&mut self) {
        if !self.loaded() {
            trace!("Deferring applying pane state to flat pane until loading is done.");
            return;
        }

        let TP::Pending(pane, state) = &mut self.pane else {
            debug!(
                "Ignoring apply_pane_state on tab {:?} without pending state and ready pane {:?}",
                self.id,
                self.element.imp().title.text()
            );
            return;
        };

        info!("Applying {:?} to tab {:?}", state, self.id);

        // Unconditionally unset it in case mode was swapped.
        pane.workaround_scroller().set_vscrollbar_policy(gtk::PolicyType::Automatic);

        let state = self.pane.must_resolve_pending();
        let pane = self.pane.get_visible_mut().unwrap();

        if let Some(search) = &self.search {
            pane.apply_state(state, search.contents());
        } else {
            pane.apply_state(state, &self.contents);
        }
    }

    pub fn make_visible(&mut self, left: &[Self], right: &[Self]) {
        if self.visible() {
            error!("Pane {:?} already displayed", self.id);
            return;
        }

        if self.group.is_some() {
            self.pane.show_ancestors();
        }

        self.pane.make_visible();
        self.element.set_pane_visible(true);

        self.load(left, right);
        self.apply_pane_state();
    }

    // TODO -- switching away from a tab group is a two-step process
    // We need to save the state before we start mucking around with visibility
    pub fn start_hide(&mut self) {
        if !self.visible() {
            return error!("Pane {:?} already hidden", self.id);
        }

        self.set_inactive();

        if let Some(search) = &self.search {
            self.pane.mark_detached(search.contents());
        } else {
            self.pane.mark_detached(&self.contents);
        }
    }

    pub fn finish_hide(&mut self) {
        if self.visible() {
            return error!("Pane {:?} not hidden", self.id);
        }

        if self.group.is_some() {
            self.pane.hide_ancestors();
        }

        self.pane.set_visible(false);
        self.element.set_pane_visible(false);
    }

    pub fn force_end_child(&mut self, paned: gtk::Paned) {
        self.pane.remove_from_parent();
        self.pane.make_end_child(paned);
    }

    pub fn add_to_visible_group(
        &mut self,
        left: &[Self],
        right: &[Self],
        group: Rc<RefCell<Group>>,
        paned: gtk::Paned,
    ) {
        group.borrow_mut().children.push(self.id());
        self.group = Some(group);

        self.element.set_child(true);

        self.pane.remove_from_parent();
        self.pane.set_visible(true);
        self.pane.make_end_child(paned);

        self.make_visible(left, right);
    }

    pub fn reattach_and_hide_pane(&mut self, parent: &gtk::Box) {
        self.start_hide();
        self.pane.remove_from_parent();
        self.pane.append(parent);
        self.finish_hide();
    }

    pub fn hide_pane(&mut self) {
        self.start_hide();
        self.pane.set_visible(false);
        self.element.set_active(false);
        self.element.set_pane_visible(false);
    }

    pub fn close(self, after: Option<TabId>) -> ClosedTab {
        // TODO [group]
        let current = self.current_history();
        ClosedTab {
            id: self.id,
            after,
            current,
            past: self.past,
            future: self.future,
        }
    }

    pub fn leave_group(&mut self) {
        self.group = None;
        self.element.set_child(false);
    }

    pub fn become_parent(&self) {
        self.element.set_child(false);
    }

    pub fn next_of_kin_by_pane(&self) -> Option<TabId> {
        // If it's not in a group, it will not have kin
        self.multi_tab_group().and_then(|_| self.pane.next_of_kin())
    }

    fn save_settings(&self) {
        gui_run(|g| {
            g.database.store(self.dir.path().clone(), self.settings);
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
                let new = EntryObject::new(entry.get_entry(), true);

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
                // This case can be hit if a directory is deleted while tabs are open to both it
                // and its parent. Duplicate events can be dispatched immediately, since they're
                // removals, before disposals can run.

                info!(
                    "Removed search-only {path:?} from event, or got duplicate event. This should \
                     be uncommon."
                );

                let search_delete = PartiallyAppliedUpdate::Delete(obj);
                for t in left
                    .iter_mut()
                    .chain(right.iter_mut())
                    .filter(|t| !t.matches_arc(self.dir.path()))
                {
                    if let Some(search) = &mut t.search {
                        search.handle_subdir_flat_update(&search_delete);
                    }
                }
                return;
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
        // TODO -- does it make sense to apply deletions here too?
        if !matches!(mutate, PartiallyAppliedUpdate::Mutate(..)) {
            return;
        }

        for t in left
            .iter_mut()
            .chain(right.iter_mut())
            .filter(|t| !t.matches_arc(self.dir.path()))
        {
            if let Some(search) = &mut t.search {
                search.handle_subdir_flat_update(&mutate);
            }
        }
    }

    pub fn content_provider(&self, operation: ClipboardOp) -> SelectionProvider {
        SelectionProvider::new(operation, self.visible_selection())
    }

    pub fn set_clipboard(&self, operation: ClipboardOp) {
        let provider = self.content_provider(operation);
        let text = provider.display_string();

        info!("Setting clipboard as: {text}");
        if let Some(pane) = self.pane.get_visible() {
            pane.set_clipboard_text(&provider.display_string())
        }

        if let Err(e) = self.element.clipboard().set_content(Some(&provider)) {
            show_error(format!("Failed to set clipboard: {e}"));
        }
    }

    pub fn accepts_paste(&self) -> bool {
        self.search.is_none() || CONFIG.paste_into_search
    }

    pub fn paste(&self) {
        if !self.accepts_paste() {
            return show_warning("Cannot paste here");
        }
        handle_clipboard(self.element.display(), self.id(), self.dir());
    }

    pub fn drag_drop(&self, drop_ev: &gtk::gdk::Drop, eo: Option<EntryObject>) -> bool {
        if let Some(eo) = eo {
            if !eo.get().dir() {
                warn!("drag_drop called on a regular file, this shouldn't happen");
                return false;
            }

            debug!("Dropping onto directory {:?} in tab {:?}", eo.get().abs_path, self.id);
            return handle_drop(drop_ev, self.id(), eo.get().abs_path.clone());
        }

        if !self.accepts_paste() {
            show_warning("Cannot drop here");
            return false;
        }

        handle_drop(drop_ev, self.id(), self.dir())
    }

    fn run_deletion(tab: TabId, files: VecDeque<Arc<Path>>, kind: Kind) {
        gui_run(|g| g.start_operation(tab, kind, files))
    }

    pub fn trash(&self) {
        info!("Trashing selected files in {:?}", self.id);
        let files = Selected::from(self.visible_selection())
            .map(|eo| eo.get().abs_path.clone())
            .collect();
        Self::run_deletion(self.id(), files, Kind::Trash(self.dir()));
    }

    pub fn delete(&self) {
        info!("Spawning deletion confirmation dialog in {:?}", self.id);

        let files = Selected::from(self.visible_selection());
        let query = if files.len() == 0 {
            return;
        } else if files.len() == 1 {
            format!("Permanently delete {:?}?", &*files.get(0).get().name)
        } else {
            format!(
                "Permanently delete {} selected items in {}?",
                files.len(),
                self.element.imp().title.text()
            )
        };

        let files: VecDeque<_> = files.map(|eo| eo.get().abs_path.clone()).collect();

        let alert = AlertDialog::builder()
            .buttons(["Cancel", "Delete"])
            .cancel_button(0)
            .default_button(1)
            .message(query)
            .build();

        let kind = Kind::Delete(self.dir());
        let tab = self.id();
        alert.choose(Some(&gui_run(|g| g.window.clone())), Cancellable::NONE, move |button| {
            if button == Ok(1) {
                debug!("Confirmed deletion for {} items in {:?}", files.len(), tab);
                Self::run_deletion(tab, files, kind);
            }
        });
    }

    pub fn rename(&self) {
        let mut files = Selected::from(self.visible_selection());
        if files.len() != 1 {
            return info!("Can't rename {} files", files.len());
        }

        gui_run(|g| g.rename_dialog(self.id(), files.next().unwrap()));
    }

    pub fn properties(&self) {
        let files = Selected::from(self.visible_selection());
        if files.len() == 0 {
            return info!("Can't show properties for empty selection");
        }

        gui_run(|g| g.properties_dialog(self.dir.path(), self.search.is_some(), files));
    }

    pub fn create(&self, folder: bool) {
        gui_run(|g| g.create_dialog(self.id(), self.dir(), folder));
    }

    // TODO -- handle operations inside a dir during search? (annoying edge case)
    fn matches_completed_op(&self, op: &operations::Operation) -> bool {
        if !self.loaded() || !self.visible() {
            info!(
                "Not scrolling to completed operation in tab that is not loaded and visible {:?}",
                self.id
            );
            return false;
        }

        let tab_dir = &**self.dir.path();
        let mut kind = &op.kind;
        // For now, this is fine. Will need to change the logic if trash operations can be undone.
        while let Kind::Undo { prev, .. } = kind {
            kind = &prev.kind;
        }

        match kind {
            Kind::Move(d) | Kind::Copy(d) => {
                if tab_dir == &**d {
                    true
                } else {
                    info!(
                        "Ignoring scroll to completed operation: tab dir {tab_dir:?} not equal to \
                         operation path {d:?}"
                    );
                    false
                }
            }
            Kind::Rename(f) | Kind::MakeDir(f) | Kind::MakeFile(f) => {
                if f.parent() == Some(tab_dir) {
                    true
                } else {
                    warn!(
                        "Ignoring scroll to completed rename/creation: tab dir {tab_dir:?} not \
                         equal to operation path {:?}. This is unusual.",
                        f.parent()
                    );
                    false
                }
            }
            Kind::Undo { .. } => unreachable!(),
            Kind::Trash(_) | Kind::Delete(_) => {
                info!("Not scrolling to completed deletion or trash operation.");
                false
            }
        }
    }

    fn paths_for_op<'a>(&self, outcomes: &'a [Outcome]) -> AHashSet<&'a Path> {
        let tab_dir = &**self.dir.path();
        outcomes
            .iter()
            .filter_map(|out| match out {
                Outcome::Move { dest, .. }
                | Outcome::Copy(dest)
                | Outcome::CopyOverwrite(dest)
                | Outcome::NewFile(dest)
                | Outcome::CreateDestDir(dest)
                | Outcome::MergeDestDir(dest) => {
                    if Some(tab_dir) == dest.parent() {
                        return Some(&**dest);
                    }
                    None
                }
                Outcome::RemoveSourceDir(..)
                | Outcome::Skip
                | Outcome::Delete
                | Outcome::DeleteDir
                | Outcome::Trash => None,
            })
            .collect()
    }

    pub fn scroll_to_completed(&mut self, op: &Rc<operations::Operation>) {
        if !self.matches_completed_op(op) {
            return;
        }

        let outcomes = op.outcomes();
        let new_paths = self.paths_for_op(&outcomes);

        if new_paths.is_empty() {
            return info!("No new paths to scroll to after operation in {:?}", self.id);
        }

        // Look up all file paths. If any are missing, flush notifications or read them.
        // If a path is present in the global map and this tab is loaded, the file will be present
        // in the contents for this tab.
        // There should be pretty few of these.
        let unmatched_paths: Vec<_> = new_paths
            .iter()
            .filter(|p| EntryObject::lookup(p).is_none())
            .map(|p| p.to_path_buf())
            .collect();

        if unmatched_paths.is_empty() {
            return self.scroll_to_completed_inner(new_paths);
        }
        drop(outcomes);

        info!("Found {} unmatched paths after file operation.", unmatched_paths.len());

        // Lower than the normal priority for the main channel.
        // This guarantees that the flushed events are processed first.
        let (s, r) = glib::MainContext::channel(glib::Priority::DEFAULT_IDLE);

        let op = op.clone();
        r.attach(None, move |_| {
            tabs_run(|tlist| {
                let Some(tab) = tlist.find_mut(op.tab) else {
                    return;
                };

                if !tab.matches_completed_op(&op) {
                    return;
                }

                let outcomes = op.outcomes();
                let new_paths = tab.paths_for_op(&outcomes);
                tab.scroll_to_completed_inner(new_paths);
            });
            glib::ControlFlow::Break
        });

        gui_run(|g| g.send_manager(ManagerAction::Flush(unmatched_paths, s)));
    }

    fn scroll_to_completed_inner(&mut self, mut new_paths: AHashSet<&Path>) {
        let selection = self.visible_selection();

        // TODO [incremental] -- if incremental filtering, must disable it now.
        let mut scroll_target = None;

        // The first one may not be the earliest by sort.
        // TODO [efficiency] -- for small new_paths and large n_items, it'd be more efficient to do
        // EntryObject::lookup and find position by sorted.
        for i in 0..selection.n_items() {
            if new_paths.is_empty() {
                break;
            }

            let eo = selection.item(i).and_downcast::<EntryObject>().unwrap();
            if !new_paths.contains(&*eo.get().abs_path) {
                continue;
            }

            new_paths.remove(&*eo.get().abs_path);

            if scroll_target.is_none() {
                selection.unselect_all();
                scroll_target = Some(ScrollPosition {
                    path: eo.get().abs_path.clone(),
                    index: i,
                });
            }

            selection.select_item(i, false);
        }

        let Some(pos) = scroll_target else {
            info!("Could not find first new item to scroll to, giving up, in {:?}", self.id);
            return;
        };

        info!("Scrolling to {pos:?} after completed operation in {:?}", self.id);

        let mut state = self.pane.clone_state(self.visible_contents());
        state.scroll_pos = Some(pos);
        self.pane.overwrite_state(state);

        self.apply_pane_state();
    }

    pub fn context_menu(&self) -> PopoverMenu {
        info!("Spawning context menu for {:?}", self.id());
        let sel: Vec<EntryObject> = Selected::from(self.visible_selection()).collect();

        gui_run(|g| g.menu.get().unwrap().prepare(g, self.settings, sel, &self.dir()))
    }

    pub fn env_vars(&self, prefix: &str, env: &mut Vec<(String, OsString)>) {
        env.push((prefix.to_owned() + "_PATH", self.dir.path().as_os_str().to_owned()));
        if let Some(search) = &self.search {
            env.push((prefix.to_owned() + "_SEARCH", search.query().0.borrow().clone().into()));
        }
    }

    pub fn selection_env_str(&self) -> OsString {
        let mut selected = Selected::from(self.visible_selection());

        let mut out = OsString::new();
        let Some(first) = selected.next() else {
            return out;
        };

        out.push(first.get().abs_path.as_os_str());

        for next in selected {
            out.push("\n");
            out.push(next.get().abs_path.as_os_str());
        }

        out
    }

    pub fn save_group(&self, ids: &AHashMap<TabId, u32>) -> SavedGroup {
        SavedGroup {
            parent: ids[&self.id()],
            split: self.pane.save_splits(ids),
        }
    }

    // https://gitlab.gnome.org/GNOME/gtk/-/issues/5670
    pub fn workaround_disable_rubberband(&self) {
        self.pane.workaround_disable_rubberband();
    }

    // https://gitlab.gnome.org/GNOME/gtk/-/issues/5670
    pub fn workaround_enable_rubberband(&self) {
        self.pane.workaround_enable_rubberband();
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

    use crate::com::{ManagerAction, SortSettings, Update};
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
        fn start(path: Arc<Path>, sort: SortSettings) -> Self {
            let cancel: Arc<AtomicBool> = Arc::default();
            gui_run(|g| {
                g.send_manager(ManagerAction::Open(path.clone(), sort, cancel.clone()));
            });

            Self { path, cancel }
        }
    }


    #[derive(Debug, Default)]
    pub enum DirState {
        #[default]
        Unloaded,
        Initializating {
            watch: WatchedDir,
            start: Instant,
        },
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
                Self::Initializating { watch, .. }
                | Self::Loading { watch, .. }
                | Self::Loaded(watch) => Some(watch),
            }
        }

        pub const fn unloaded(&self) -> bool {
            match self {
                Self::Unloaded => true,
                Self::Initializating { .. } | Self::Loading { .. } | Self::Loaded(_) => false,
            }
        }

        pub const fn loading(&self) -> bool {
            match self {
                Self::Unloaded | Self::Loaded(_) => false,
                Self::Initializating { .. } | Self::Loading { .. } => true,
            }
        }

        pub const fn loaded(&self) -> bool {
            match self {
                Self::Unloaded | Self::Initializating { .. } | Self::Loading { .. } => false,
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

        pub fn matches(&self, id: &Arc<AtomicBool>) -> bool {
            match &*self.state.borrow() {
                DirState::Unloaded => false,
                DirState::Initializating { watch, .. }
                | DirState::Loading { watch, .. }
                | DirState::Loaded(watch) => Arc::ptr_eq(&watch.cancel, id),
            }
        }

        // Returns the update if it wasn't consumed
        pub fn consume(&self, up: Update) -> Option<Update> {
            let mut sb = self.state.borrow_mut();
            match &mut *sb {
                DirState::Unloaded => {
                    info!("Dropping update {up:?} for unloaded tab.");
                    None
                }
                DirState::Initializating { .. } => {
                    debug!("Dropping update {up:?} for initializing tab.");
                    None
                }
                DirState::Loading { pending_updates, .. } => {
                    pending_updates.push(up);
                    None
                }
                DirState::Loaded(_) => Some(up),
            }
        }

        // sort can become stale or not match all tabs,
        // but it's overwhelmingly going to save time in the UI thread.
        pub fn start_load(&self, sort: SortSettings) -> bool {
            let mut sb = self.state.borrow_mut();
            let dstate = &mut *sb;
            match dstate {
                DirState::Loading { .. }
                | DirState::Initializating { .. }
                | DirState::Loaded(_) => false,
                DirState::Unloaded => {
                    let start = Instant::now();
                    debug!("Opening directory for {:?}", self.path);
                    let watch = WatchedDir::start(self.path.clone(), sort);

                    *dstate = DirState::Initializating { watch, start };
                    true
                }
            }
        }

        pub fn mark_watch_started(&self, id: Arc<AtomicBool>) {
            let mut sb = self.state.borrow_mut();
            let dstate = &mut *sb;
            let old = std::mem::take(dstate);

            match old {
                DirState::Unloaded | DirState::Loading { .. } | DirState::Loaded(_) => {
                    unreachable!()
                }
                DirState::Initializating { watch, start } => {
                    assert!(Arc::ptr_eq(&watch.cancel, &id));
                    debug!("Marking watch started in {:?}", self.path);

                    *dstate = DirState::Loading {
                        watch,
                        pending_updates: Vec::new(),
                        start,
                    };
                }
            }
        }

        pub fn unload(&self) {
            self.state.take();
        }

        pub fn take_loaded(&self) -> (Vec<Update>, Instant) {
            let mut sb = self.state.borrow_mut();
            match std::mem::take(&mut *sb) {
                DirState::Unloaded | DirState::Initializating { .. } | DirState::Loaded(_) => {
                    unreachable!()
                }
                DirState::Loading { watch, pending_updates, start } => {
                    *sb = DirState::Loaded(watch);
                    (pending_updates, start)
                }
            }
        }
    }
}
