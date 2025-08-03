use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::mem::replace;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use TabPane as TP;
use ahash::{AHashMap, AHashSet};
use gtk::gio::Cancellable;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{AlertDialog, Bitset, MultiSelection, Orientation, PopoverMenu, Widget, glib};
use hashlink::LinkedHashMap;
use tokio::sync::oneshot;

use super::contents::Contents;
use super::element::TabElement;
use super::flat_dir::FlatDir;
use super::id::{TabId, TabUid};
use super::list::Group;
use super::pane::Pane;
use super::search::Search;
use super::{CachedDir, HistoryEntry, NavTarget, PaneState, TabContext};
use crate::com::{
    DirSettings, DirSnapshot, DisplayMode, EntryObject, EntryObjectSnapshot, GetEntry,
    ManagerAction, SearchSnapshot, SearchUpdate, SortDir, SortMode, SortSettings,
};
use crate::config::CONFIG;
use crate::database::SavedGroup;
use crate::gui::clipboard::{ClipboardOp, SelectionProvider, handle_clipboard, handle_drop};
use crate::gui::operations::{self, Kind, Outcome};
use crate::gui::tabs::{
    ExistingEntry, FocusState, PartiallyAppliedUpdate, ScrollPosition, cache_open_dir,
};
use crate::gui::{
    CompletionResult, Selected, Update, applications, gui_run, show_error, show_warning, tabs_run,
};

/* This efficiently supports multiple tabs being open to the same directory with different
 * settings.
 *
 * It's a little more complicated than if they all were required to have the same settings, but
 * the only way to make this code actually simple is to completely abandon efficiency -
 * especially in my common case of opening a new tab.
 */


#[derive(Debug)]
enum TabPane {
    Displayed(Pane),
    // The tab is displayed but the contents are loading, when loading finishes the PaneState will
    // be applied.
    Loading(Pane, PaneState),
    Detached {
        pane: Pane,
        state: PaneState,
        pending: bool,
    },
    // Temporary state
    Empty,
}

impl Deref for TabPane {
    type Target = Pane;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Displayed(pane) | Self::Loading(pane, _) | Self::Detached { pane, .. } => pane,
            Self::Empty => unreachable!(),
        }
    }
}

impl DerefMut for TabPane {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Displayed(pane) | Self::Loading(pane, _) | Self::Detached { pane, .. } => pane,
            Self::Empty => unreachable!(),
        }
    }
}

impl TabPane {
    const fn visible(&self) -> bool {
        match &self {
            Self::Displayed(_) | Self::Loading(..) => true,
            Self::Detached { .. } => false,
            Self::Empty => unreachable!(),
        }
    }

    const fn get_visible(&self) -> Option<&Pane> {
        match self {
            Self::Displayed(p) | Self::Loading(p, _) => Some(p),
            Self::Detached { .. } => None,
            Self::Empty => unreachable!(),
        }
    }

    const fn get_visible_mut(&mut self) -> Option<&mut Pane> {
        match self {
            Self::Displayed(p) | Self::Loading(p, _) => Some(p),
            Self::Detached { .. } => None,
            Self::Empty => unreachable!(),
        }
    }

    fn clone_state(&self, list: &Contents) -> PaneState {
        match self {
            Self::Displayed(p) => p.get_state(list),
            Self::Detached { state, .. } | Self::Loading(_, state) => state.clone(),
            Self::Empty => unreachable!(),
        }
    }

    fn overwrite_state(&mut self, new_state: PaneState) {
        match self {
            Self::Displayed(_p) => {
                let Self::Displayed(pane) = std::mem::replace(self, Self::Empty) else {
                    unreachable!()
                };
                *self = Self::Loading(pane, new_state);
            }
            Self::Loading(_, state) => *state = new_state,
            Self::Detached { state, pending, .. } => {
                *state = new_state;
                *pending = true;
            }
            Self::Empty => unreachable!(),
        }
    }

    fn make_visible(&mut self) {
        match std::mem::replace(self, Self::Empty) {
            Self::Detached { pane, state, pending } => {
                pane.set_visible(true);
                *self = if pending { Self::Loading(pane, state) } else { Self::Displayed(pane) };
            }
            Self::Displayed(_) | Self::Loading(..) | Self::Empty => unreachable!(),
        }
    }

    fn mark_detached(&mut self, list: &Contents) {
        match std::mem::replace(self, Self::Empty) {
            Self::Detached { .. } | Self::Empty => unreachable!(),
            Self::Displayed(pane) => {
                let state = pane.get_state(list);
                *self = Self::Detached { pane, state, pending: false };
            }
            Self::Loading(pane, state) => {
                *self = Self::Detached { pane, state, pending: true };
            }
        }
    }

    fn prepare_for_unload(&mut self, list: &Contents) {
        let state = match self {
            Self::Detached { pending, .. } => return *pending = true,
            Self::Loading(..) => return,
            Self::Empty => unreachable!(),
            Self::Displayed(p) => p.get_state(list),
        };

        let Self::Displayed(p) = std::mem::replace(self, Self::Empty) else {
            unreachable!()
        };

        *self = Self::Loading(p, state);
    }

    fn maybe_resolve_pending(&mut self) -> Option<(&mut Pane, PaneState)> {
        let old = std::mem::replace(self, Self::Empty);
        if let Self::Loading(pane, state) = old {
            *self = Self::Displayed(pane);
            Some((self.get_visible_mut().unwrap(), state))
        } else {
            *self = old;
            None
        }
    }

    fn set_detached_pending_flag(&mut self) {
        match self {
            Self::Displayed(_) | Self::Loading(..) => {}
            Self::Detached { pending, .. } => *pending = true,
            Self::Empty => unreachable!(),
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

        assert_ne!(group.borrow().parent, self.id());

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

    pub fn loading(&self) -> bool {
        self.dir.state().loading() || self.search.as_ref().is_some_and(Search::loading)
    }

    fn loaded(&self) -> bool {
        self.dir.state().loaded() && self.search.as_ref().is_none_or(Search::loaded)
    }

    pub fn dir(&self) -> Arc<Path> {
        self.dir.path().clone()
    }

    pub fn overlaps(&self, p: &Arc<Path>) -> bool {
        // TODO [refresh] actually consider overlapping searches
        self.matches_arc(p)
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

    fn matching_mut<'a>(&'a self, other: &'a mut [Self]) -> impl Iterator<Item = &'a mut Self> {
        other.iter_mut().filter(|t| self.matches_arc(t.dir.path()))
    }

    fn matching<'a>(&'a self, other: &'a [Self]) -> impl Iterator<Item = &'a Self> {
        other.iter().filter(|t| self.matches_arc(t.dir.path()))
    }

    pub fn new(
        id: TabUid,
        target: NavTarget,
        mut context: TabContext<'_>,
        initial_width: u32,
        insert: impl FnOnce(&Widget),
    ) -> (Self, TabElement) {
        debug!("Opening tab {id:?} to {target:?}");
        // fetch metatada synchronously, even with a donor
        let settings = gui_run(|g| g.database.get(target.dir.clone()));

        let element = TabElement::new(id.copy(), &target.dir);
        let dir = FlatDir::new(target.dir);
        let contents = Contents::new(settings.sort);
        let state = PaneState::for_jump(target.scroll);

        let pane = Pane::new_flat(
            id.copy(),
            dir.path(),
            settings,
            &contents.selection,
            initial_width,
            insert,
        );

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

        t.copy_flat_from_donor(&mut context);

        (t, element)
    }

    pub fn cloned(
        id: TabUid,
        source: &Self,
        initial_width: u32,
        insert: impl FnOnce(&Widget),
    ) -> (Self, TabElement) {
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
                initial_width,
                insert,
            )
        } else {
            Pane::new_flat(
                id.copy(),
                source.dir.path(),
                source.settings,
                &contents.selection,
                initial_width,
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
        mut context: TabContext<'_>,
        initial_width: u32,
        insert: impl FnOnce(&Widget),
    ) -> (Self, TabElement) {
        debug!("Reopening closed tab {closed:?}");
        let settings = gui_run(|g| g.database.get(closed.current.location.clone()));

        let element = TabElement::new(closed.id.copy(), &closed.current.location);
        let dir = FlatDir::new(closed.current.location);
        let contents = Contents::new(settings.sort);

        let pane = Pane::new_flat(
            closed.id.copy(),
            dir.path(),
            settings,
            &contents.selection,
            initial_width,
            insert,
        );

        let mut t = Self {
            id: closed.id,
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

        t.copy_flat_from_donor(&mut context);

        // Open search, if applicable, after copying flat state.
        if let Some(query) = closed.current.search {
            t.open_search(query);
        }

        (t, element)
    }

    fn copy_flat_from_donor(&mut self, context: &mut TabContext<'_>) -> bool {
        for t in context.left.iter().chain(context.right) {
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

        if let Some(mut cached) = context.cached.remove(self.dir.path()) {
            self.dir = cached.make_flat_once();
            self.contents.inherit_cached(self.settings.sort, &cached);
            self.element.stop_spin();
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn split(
        &mut self,
        orient: Orientation,
        forced: bool,
    ) -> Option<(gtk::Paned, Rc<RefCell<Group>>)> {
        let paned = self.pane.split(orient, forced)?;

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
        let Some(_search) = self.search.take() else {
            return;
        };
        trace!("Closing Search for {:?}", self.id);

        if !self.loading() {
            self.element.stop_spin();
        }

        self.element.flat_title(self.dir.path());

        self.pane.search_to_flat(self.dir.path(), &self.contents.selection);
    }

    // Starts a new search or updates an existing one with a new query.
    pub fn search(&mut self, query: String) {
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
        let search_load = self.search.as_mut().is_some_and(Search::start_load);

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
            self.pane.prepare_for_unload(search.contents());
            search.unload(self.settings.sort);
        } else {
            self.pane.prepare_for_unload(&self.contents);
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

        // If it's initial && finished, it's a complete snapshot
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
        if self.visible() {
            if let Update::Removed(path) = &update.update {
                if let Some(eo) = EntryObject::lookup(path) {
                    self.pane.workaround_focus_before_delete(&eo);
                }
            }
        }

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
    pub fn handle_directory_deleted(&mut self, mut context: TabContext<'_>) -> bool {
        let cur_path = self.dir.path().clone();
        let mut path = &*cur_path;
        while !path.exists() || !path.is_dir() {
            path = path.parent().unwrap()
        }

        if path.exists() {
            // Honestly should just close the tab, but this means something is seriously broken.
            drop(self.change_location_flat(&mut context, NavTarget::assume_dir(path)));
            true
        } else {
            false
        }
    }

    fn update_settings(&mut self) {
        self.contents.sort(self.settings.sort);

        let set_pending = if let Some(search) = &mut self.search {
            search.update_settings(self.settings);

            self.pane.update_settings(self.settings, search.contents())
        } else {
            self.pane.update_settings(self.settings, &self.contents)
        };

        if set_pending {
            self.pane.set_detached_pending_flag();
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

    pub fn update_sort_direction(&mut self, dir: SortDir) {
        self.settings.sort.direction = dir;
        self.update_settings();
    }

    // Changes location without managing history or view states.
    // Returns the navigation target if nothing happens
    // Will end a search.
    fn change_location_flat(
        &mut self,
        context: &mut TabContext<'_>,
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

        let old_dir = replace(&mut self.dir, FlatDir::new(target.dir));
        let new_cache = old_dir
            .try_into_cached()
            .map(|(path, watch)| self.contents.list_into_cache(path, watch));

        let old_settings = self.settings;
        self.settings = gui_run(|g| g.database.get(self.dir.path().clone()));

        if was_search {
            // Clear so we don't flicker
            self.contents.clear(self.settings.sort);
        }

        if !self.copy_flat_from_donor(context) {
            // We couldn't find any state to steal, so we know we're the only matching tab.

            if was_search {
                // Contents were cleared above
            } else if self.pane.visible() && self.settings.allow_stale(old_settings) {
                // Deliberately do not clear or update self.contents here, not yet.
                // This allows us to keep something visible just a tiny bit longer.
                // Safe enough when the settings match, though there's no point in doing it when
                // the display mode changes.
                self.contents.mark_stale(self.settings.sort);
            } else {
                self.contents.clear(self.settings.sort);
            }
        }

        if let Some(new) = new_cache {
            cache_open_dir(context.cached, new);
        }

        if was_search {
            self.close_search();
        }

        // Cannot be a search pane
        debug_assert!(self.search.is_none());
        self.pane.update_location(self.dir.path(), self.settings, &self.contents);
        self.load(context.left, context.right);
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
        applications::open(
            self.id(),
            self.dir.path(),
            &display,
            self.visible_selection().into(),
            true,
        );
    }

    pub fn open_default(&self) {
        let display = self.element.display();
        applications::open(
            self.id(),
            self.dir.path(),
            &display,
            self.visible_selection().into(),
            false,
        );
    }

    pub fn open_with(&self) {
        gui_run(|g| g.open_with(self.visible_selection().into()));
    }

    pub fn select_if_not(&self, eo: EntryObject) {
        let contents = self.visible_contents();

        // Should not be possible to call this with an item not in this tab.
        let pos = contents.filtered_position_by_sorted(&eo.get()).unwrap();
        if !contents.selection.is_selected(pos) {
            info!("Updating selection to clicked item {:?}", &*eo.get().name);
            contents.selection.select_item(pos, true);
        }
    }

    fn seek_inner(&mut self, fragment: &str, range: impl Iterator<Item = u32>) -> bool {
        // Smart case seeking?
        // Prefix instead?
        // This probably doesn't need to be normalized, but it might be helpful
        let fragment = fragment.to_lowercase();
        let contents = self.visible_contents();

        for i in range {
            let eo = contents.selection.item(i).and_downcast::<EntryObject>().unwrap();
            if !eo.matches_seek(&fragment) {
                continue;
            }

            debug!("Seeking to {:?}", &*eo.get().name);

            if self.loaded() && self.visible() {
                self.pane.get_visible().unwrap().seek_to(i);
            } else {
                contents.selection.select_item(i, true);

                let mut state = self.pane.clone_state(contents);
                state.scroll = Some(ScrollPosition {
                    precise: None,
                    path: eo.get().abs_path.clone(),
                    index: i,
                });
                state.focus = Some(FocusState {
                    path: eo.get().abs_path.clone(),
                    select: true,
                });
                self.pane.overwrite_state(state);

                self.apply_pane_state();
            }

            return true;
        }
        false
    }

    pub fn seek(&mut self, fragment: &str) {
        let contents = self.visible_contents();
        self.seek_inner(fragment, 0..contents.selection.n_items());
    }

    pub fn seek_next(&mut self, fragment: &str) {
        let contents = self.visible_contents();

        // No real fallback if there's no selection
        let sel = contents.selection.selection();
        if !sel.is_empty() {
            let found = self.seek_inner(fragment, sel.nth(0) + 1..contents.selection.n_items());
            if !found && CONFIG.seek_wraparound {
                self.seek_inner(fragment, 0..sel.nth(0));
            }
        } else {
            self.seek_inner(fragment, 0..contents.selection.n_items());
        }
    }

    pub fn seek_prev(&mut self, fragment: &str) {
        let contents = self.visible_contents();

        // No real fallback if there's no selection
        let sel = contents.selection.selection();
        if !sel.is_empty() {
            let n_items = contents.selection.n_items();
            let found = self.seek_inner(fragment, (0..sel.nth(0)).rev());
            if !found && CONFIG.seek_wraparound {
                self.seek_inner(fragment, (sel.nth(0) + 1..n_items).rev());
            }
        } else {
            self.seek_inner(fragment, 0..contents.selection.n_items());
        }
    }

    pub fn clear_selection(&self) {
        self.visible_selection().unselect_all();
    }

    pub fn navigate(&mut self, context: TabContext<'_>, target: NavTarget) {
        self.navigate_inner(context, target, true);
    }

    fn navigate_inner(&mut self, mut context: TabContext<'_>, target: NavTarget, forward: bool) {
        info!("Navigating {:?} from {:?} to {target:?}", self.id, self.dir.path());

        let history = self.current_history();

        let Some(unconsumed) = self.change_location_flat(&mut context, target) else {
            if forward {
                self.past.push(history);
                self.future.clear();
            } else {
                self.past.clear();
                self.future.push(history);
            }
            return;
        };

        let Some(path) = unconsumed.scroll else {
            return;
        };

        // Location didn't change, treat this as a jump
        let mut state = self.pane.clone_state(&self.contents);

        state.scroll = Some(ScrollPosition {
            precise: None,
            path: path.clone(),
            index: 0,
        });
        state.focus = Some(FocusState { path, select: true });
        self.pane.overwrite_state(state);

        self.apply_pane_state();
    }

    pub fn parent(&mut self, context: TabContext<'_>) {
        self.parent_inner(context, true);
    }

    fn parent_inner(&mut self, context: TabContext<'_>, forward: bool) {
        let Some(parent) = self.dir.path().parent() else {
            warn!("No parent for {:?}", self.dir.path());
            return;
        };

        let target = NavTarget::assume_jump(parent, self.dir());
        self.navigate_inner(context, target, forward);
    }

    pub fn forward(&mut self, mut context: TabContext<'_>) {
        let Some(next) = self.future.pop() else {
            warn!("No future for {:?} to go forward to", self.id);
            return;
        };

        info!("Forwards in tab {:?} to {next:?}", self.id);
        let history = self.current_history();

        self.past.push(history);
        self.apply_history(&mut context, next);
    }

    pub(super) fn back(&mut self, context: &mut TabContext<'_>) -> bool {
        let Some(prev) = self.past.pop() else {
            warn!("No history for {:?} to go back to", self.id);
            return false;
        };

        info!("Back in tab {:?} to {prev:?}", self.id);
        let history = self.current_history();

        self.future.push(history);
        self.apply_history(context, prev);
        true
    }

    fn apply_history(&mut self, context: &mut TabContext<'_>, hist: HistoryEntry) {
        // Shouldn't be a jump, could be a search starting/ending.
        if hist.location == *self.dir.path() {
            if let Some(query) = hist.search {
                self.open_search(query);
            } else {
                self.close_search();
            }
            self.pane.overwrite_state(hist.state);
            return self.apply_pane_state();
        }

        let target = NavTarget::assume_dir(hist.location);

        if let Some(unconsumed) = self.change_location_flat(context, target) {
            error!(
                "Failed to change location {unconsumed:?} after already checking it was a change."
            );
            self.pane.overwrite_state(hist.state);
            return self.apply_pane_state();
        }

        if let Some(query) = hist.search {
            self.open_search(query);
        } else {
            self.close_search();
        }

        self.pane.overwrite_state(hist.state);
        self.apply_pane_state();
    }

    pub(super) fn back_or_parent(&mut self, mut context: TabContext<'_>) {
        if !self.back(&mut context) {
            self.parent_inner(context, false);
        }
    }

    // Goes to the child directory if one was previously open or if there's only one.
    pub(super) fn child(&mut self, mut context: TabContext<'_>) {
        if self.search.is_some() {
            warn!("Can't move to child in search tab {:?}", self.id);
            return;
        }

        // Try the past and future first.
        if Some(&**self.dir.path()) == self.past.last().and_then(|h| h.location.parent()) {
            info!("Found past entry for child dir while handling Child in {:?}", self.id);
            self.back(&mut context);
            return;
        }
        if Some(&**self.dir.path()) == self.future.last().and_then(|h| h.location.parent()) {
            info!("Found future entry for child dir while handling Child in {:?}", self.id);
            return self.forward(context);
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
            let second = self.contents.selection.item(1).and_downcast::<EntryObject>().unwrap();
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

        if let Some(unconsumed) = self.change_location_flat(&mut context, target) {
            error!(
                "Failed to change location {unconsumed:?} after already checking it was a change."
            );
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

        let Some((pane, state)) = self.pane.maybe_resolve_pending() else {
            debug!(
                "Ignoring apply_pane_state on tab {:?} without pending state and ready pane {:?}",
                self.id,
                self.element.imp().title.text()
            );
            return;
        };

        info!("Applying {state:?} to tab {:?}", self.id);

        pane.start_apply_state(state);
    }

    pub fn finish_apply_state(&mut self) {
        if let TabPane::Displayed(pane) = &mut self.pane {
            if let Some(search) = &self.search {
                pane.finish_apply_state(search.contents());
            } else {
                pane.finish_apply_state(&self.contents);
            }
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

        self.apply_pane_state()
    }

    pub fn start_hide(&mut self) {
        if !self.visible() {
            return warn!("Pane {:?} already hidden", self.id);
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

    pub fn hide_single_pane(&mut self) {
        self.start_hide();
        self.pane.set_visible(false);
        self.element.set_active(false);
        self.element.set_pane_visible(false);
    }

    pub fn close(
        mut self,
        after: Option<TabId>,
        cache: &mut LinkedHashMap<Arc<Path>, CachedDir>,
    ) -> ClosedTab {
        let current = self.current_history();
        if let Some((path, watch)) = self.dir.try_into_cached() {
            cache_open_dir(cache, self.contents.list_into_cache(path, watch));
        }

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

        let Some(update) = self.dir.maybe_drop_or_delay(up) else {
            return;
        };

        let existing_entry = self.contents.entry_for_flat_update(&update);

        // If something exists globally but not locally, it needs to be applied to search tabs.
        let mut search_mutate = None;

        let partial = match (update, existing_entry) {
            (Update::Entry(entry), ExistingEntry::Present(obj, pos)) => {
                let Some(old) = obj.update(entry) else {
                    // An unimportant update.
                    // No need to check other tabs.
                    return;
                };

                self.contents.reinsert_updated(pos, &obj, &old);

                trace!("Updated {:?} from event", old.abs_path);
                PartiallyAppliedUpdate::Mutate(old, obj)
            }
            (Update::Entry(entry), ExistingEntry::NotLocal(obj)) => {
                // This means the element already existed in a search tab, somewhere else, and
                // we're updating it.
                if let Some(old) = obj.update(entry) {
                    // It's an update for (some) search tabs, but an insertion for flat tabs.
                    //
                    // This means that a different search tab happened to read a newly created file
                    // before it was inserted into this tab.
                    //
                    // The item is potentially missing from other search tabs, but that's not a
                    // major concern since search snapshots prefer existing entries.
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
            (Update::Entry(entry), ExistingEntry::Missing) => {
                let new = EntryObject::new(entry.get_entry(), true);

                self.contents.insert(&new);
                trace!("Inserted new {:?} from event", new.get().abs_path);
                PartiallyAppliedUpdate::Insert(new)
            }
            (Update::Removed(path), ExistingEntry::Present(obj, pos)) => {
                if self.pane.visible() {
                    self.pane.workaround_focus_before_delete(&obj);
                }
                self.contents.remove(pos);
                trace!("Removed {path:?} from event");
                PartiallyAppliedUpdate::Delete(obj)
            }
            (Update::Removed(path), ExistingEntry::NotLocal(obj)) => {
                // This case can be hit if a directory is deleted while tabs are open to both it
                // and its parent. Duplicate events can be dispatched immediately, since they're
                // removals, before disposals can run.

                info!(
                    "Removed search-only {path:?} from event, or got duplicate event. This should \
                     be uncommon."
                );

                let search_delete = PartiallyAppliedUpdate::Delete(obj);
                left.iter_mut()
                    .chain(right.iter_mut())
                    .filter(|t| !t.matches_arc(self.dir.path()))
                    .for_each(|t| t.handle_search_subdir_flat_update(&search_delete));
                return;
            }
            (Update::Removed(path), ExistingEntry::Missing) => {
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
            if t.visible() {
                if let PartiallyAppliedUpdate::Delete(eo) = &partial {
                    t.pane.workaround_focus_before_delete(eo);
                }
            }

            t.contents.finish_update(&partial);
            if let Some(search) = &mut t.search {
                search.finish_flat_update(&partial)
            }
        }

        // Apply mutations to any matching search tabs, even to the left.
        let mutate = search_mutate.unwrap_or(partial);
        // TODO -- does it make sense to apply deletions here too?
        // If so, need to also fix focus
        if !matches!(mutate, PartiallyAppliedUpdate::Mutate(..)) {
            return;
        }

        left.iter_mut()
            .chain(right.iter_mut())
            .filter(|t| !t.matches_arc(self.dir.path()))
            .for_each(|t| t.handle_search_subdir_flat_update(&mutate));
    }

    pub fn handle_search_subdir_flat_update(&mut self, partial: &PartiallyAppliedUpdate) {
        if let Some(search) = &mut self.search {
            search.handle_subdir_flat_update(partial);
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

    pub fn focus_location_bar(&self) {
        if let Some(p) = self.pane.get_visible() {
            p.focus_location_bar();
        }
    }

    pub fn unselect(&self) {
        if self.unloaded() {
            return;
        }

        self.clear_selection();
    }

    pub fn create(&self, folder: bool) {
        gui_run(|g| g.create_dialog(self.id(), self.dir(), folder));
    }

    pub fn handle_completion(&self, completed: CompletionResult) {
        if self.search.is_some() {
            return;
        }

        let Some(pane) = self.pane.get_visible() else {
            return;
        };

        pane.handle_completion(completed);
    }

    // TODO -- handle operations inside a subdir during search? (annoying edge case)
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

    // TODO -- handle operations inside a subdir during search?
    fn new_paths_for_op(&self, outcomes: &[Outcome]) -> AHashSet<Arc<Path>> {
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
                        return Some(dest.clone());
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

    pub fn scroll_to_completed(&self, op: &Rc<operations::Operation>) {
        if !self.matches_completed_op(op) {
            return;
        }

        let outcomes = op.outcomes();
        let all_paths = self.new_paths_for_op(&outcomes);

        if all_paths.is_empty() {
            return info!("No new paths to scroll to after operation in {:?}", self.id);
        }

        // Look up all file paths. We'll flush updates for all paths, but if any are missing, we
        // need to also read them from the disk. There should be pretty few of those.
        //
        // If a path is present in the global map and this tab is loaded, the file will be present
        // in the contents for this tab.
        //
        // There is still a potential race where a pending update for a known file could be
        // missed. The fix is to also read any files with recent updates, but that seems very
        // wasteful.
        let unmatched_paths: AHashSet<_> =
            all_paths.iter().filter(|p| EntryObject::lookup(p).is_none()).cloned().collect();

        drop(outcomes);

        info!("Found {} unmatched paths after file operation.", unmatched_paths.len());

        // Lower than the normal priority for the main channel.
        // This guarantees that the flushed events are processed first.
        let (s, r) = oneshot::channel();

        let ctx = glib::MainContext::ref_thread_default();
        let op = op.clone();
        ctx.spawn_local_with_priority(glib::Priority::DEFAULT_IDLE, async move {
            let Ok(all_paths) = r.await else {
                return;
            };

            tabs_run(|tlist| {
                let Some(tab) = tlist.find_mut(op.tab) else {
                    return;
                };

                if !tab.matches_completed_op(&op) {
                    return;
                }

                tab.scroll_to_completed_inner(all_paths);
            });
        });

        gui_run(|g| {
            g.send_manager(ManagerAction::Flush { all_paths, unmatched_paths, finished: s })
        });
    }

    fn scroll_to_completed_inner(&mut self, mut new_paths: AHashSet<Arc<Path>>) {
        let selection = self.visible_selection();

        // TODO [incremental] -- if incremental filtering, must disable it now.
        let mut new_state = None;

        let select_set = Bitset::new_empty();

        // The first completed operation may not be the earliest by sort.
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

            if new_state.is_none() {
                selection.unselect_all();

                info!(
                    "Scrolling to {:?} after completed operation in {:?}",
                    eo.get().abs_path,
                    self.id
                );

                new_state = Some(PaneState {
                    scroll: Some(ScrollPosition {
                        precise: None,
                        path: eo.get().abs_path.clone(),
                        index: i,
                    }),
                    focus: Some(FocusState {
                        path: eo.get().abs_path.clone(),
                        select: false,
                    }),
                });
            }

            select_set.add(i);
        }

        selection.set_selection(&select_set, &select_set);

        let Some(state) = new_state else {
            info!("Could not find first new item to scroll to, giving up, in {:?}", self.id);
            return;
        };

        self.pane.overwrite_state(state);

        self.apply_pane_state();
    }

    pub fn context_menu(&self) -> PopoverMenu {
        info!("Spawning context menu for {:?}", self.id);
        let sel: Vec<EntryObject> = Selected::from(self.visible_selection()).collect();

        gui_run(|g| g.menu.get().unwrap().prepare(g, self.id(), self.settings, sel, &self.dir()))
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
