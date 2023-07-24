use std::cell::{Cell, OnceCell, Ref, RefCell};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::env::current_dir;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut, Index, IndexMut};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::gio::ListStore;
use gtk::glib::Object;
use gtk::prelude::{Cast, ListModelExt, ListModelExtManual, StaticType};
use gtk::subclass::prelude::{ObjectSubclassExt, ObjectSubclassIsExt};
use gtk::traits::{AdjustmentExt, BoxExt, SelectionModelExt, WidgetExt};
use gtk::{glib, Box, MultiSelection, Orientation, ScrolledWindow, Widget};
use path_clean::PathClean;

use self::flat_dir::{DirState, FlatDir};
use super::contents::Contents;
use super::element::TabElement;
use super::id::{TabId, TabUid};
use super::pane::{Pane, PaneExt};
use super::search::SearchPane;
use super::{HistoryEntry, NavTarget, SavedViewState, ScrollPosition};
use crate::com::{
    DirSettings, DirSnapshot, DisplayMode, Entry, EntryObject, EntryObjectSnapshot, FileTime,
    GuiAction, ManagerAction, SnapshotId, SnapshotKind, SortMode, SortSettings,
};
use crate::gui::tabs::PartiallyAppliedUpdate;
use crate::gui::{gui_run, Update};
use crate::natsort::ParsedString;

/* This efficiently supports multiple tabs being open to the same directory with different
 * settings.
 *
 * It's a little more complicated than if they all were required to have the same settings, but
 * the only way to make this code actually simple is to completely abandon efficiency -
 * especially in my common case of opening a new tab.
 */

#[derive(Debug, Default)]
enum CurrentPane {
    #[default]
    Nothing,
    Flat(Pane),
    Search(SearchPane),
}


impl CurrentPane {
    // Nothing is going to happen so often that dynamic dispatch is a problem.
    fn get(&self) -> Option<&dyn PaneExt> {
        match self {
            Self::Nothing => None,
            Self::Flat(p) => Some(p),
            Self::Search(p) => Some(p),
        }
    }

    fn get_mut(&mut self) -> Option<&mut dyn PaneExt> {
        match self {
            Self::Nothing => None,
            Self::Flat(p) => Some(p),
            Self::Search(p) => Some(p),
        }
    }

    fn flat(&mut self) -> Option<&mut Pane> {
        match self {
            Self::Nothing | Self::Search(_) => None,
            Self::Flat(p) => Some(p),
        }
    }

    fn search(&mut self) -> Option<&mut SearchPane> {
        match self {
            Self::Nothing | Self::Flat(_) => None,
            Self::Search(p) => Some(p),
        }
    }
}

// Current limitations:
// - All tabs open to the same directory are in the same TabState
//   - Slightly increases memory usage over the absolute bare minimum.
//
// Invariants:
// - All tabs open to the same directory share the same watcher
// - All tabs open to the same directory share the same snapshots
// - All tabs open to the same directory are in the same TabState
//
// The net result is going to be higher memory usage than strictly necessary if multiple tabs are
// opened to the same directory and some of them could have been deactivated.
#[derive(Debug)]
pub(super) struct Tab {
    // displayed_path: PathBuf,
    id: TabUid,
    dir: FlatDir,
    settings: DirSettings,
    contents: Contents,
    // TODO -- this should only store snapshots and be sporadically updated/absent
    view_state: Option<SavedViewState>,
    past: Vec<HistoryEntry>,
    future: Vec<HistoryEntry>,
    element: TabElement,

    // Each tab can only be open in one pane at once.
    // In theory we could do many-to-many but it's too niche.
    pane: CurrentPane,
}

impl Tab {
    pub const fn id(&self) -> TabId {
        self.id.copy()
    }

    pub fn visible(&self) -> bool {
        self.pane.get().map_or(false, PaneExt::visible)
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
        let view_state = Some(SavedViewState::for_jump(target.scroll));


        let mut t = Self {
            id,
            dir,
            settings,
            contents,
            view_state,
            past: Vec::new(),
            future: Vec::new(),
            element: element.clone(),
            pane: CurrentPane::Nothing,
        };

        t.copy_from_donor(existing_tabs, &[]);

        (t, element)
    }

    pub fn cloned(id: TabUid, source: &Self) -> (Self, TabElement) {
        // Assumes inactive tabs cannot be cloned.
        let view_state = source.take_view_snapshot();
        let mut contents = Contents::new(source.settings.sort);
        let element = TabElement::new(id.copy(), Path::new(""));

        contents.clone_from(&source.contents, source.settings.sort);
        element.clone_from(&source.element);

        (
            Self {
                id,
                dir: source.dir.clone(),
                settings: source.settings,
                contents,
                view_state,
                past: source.past.clone(),
                future: source.future.clone(),
                element: element.clone(),
                pane: CurrentPane::default(),
            },
            element,
        )
    }

    fn copy_from_donor(&mut self, left: &[Self], right: &[Self]) -> bool {
        // TODO -- clone tab_element state from donor, like spinner
        for t in left.iter().chain(right) {
            // Check for value equality, not reference equality
            if **self.dir.path() == **t.dir.path() {
                self.dir = t.dir.clone();
                self.contents.clone_from(&t.contents, self.settings.sort);
                self.element.clone_from(&t.element);

                if let Some(pane) = self.pane.get() {
                    if matches!(*self.dir.state(), DirState::Loaded(_)) {
                        error!("Unsetting GTK crash workaround");
                        pane.workaround_scroller()
                            .set_vscrollbar_policy(gtk::PolicyType::Automatic);

                        self.apply_view_state();
                    }
                }

                return true;
            }
        }
        false
    }

    pub fn split(&mut self, orient: Orientation) -> Option<gtk::Paned> {
        self.pane.get().unwrap().split(orient)
    }

    // Only make this take &mut [Self] if truly necessary
    pub fn load(&mut self, left: &[Self], right: &[Self]) {
        if !self.dir.start_load() {
            return;
        }
        // TODO [search]

        // TODO -- spinners
        self.element.imp().spinner.start();
        self.element.imp().spinner.set_visible(true);
        for t in self.matching(left).chain(self.matching(right)) {
            t.element.imp().spinner.start();
            t.element.imp().spinner.set_visible(true);
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
            if let Some(pane) = &self.pane.get() {
                if pane.workaround_scroller().vscrollbar_policy() != gtk::PolicyType::Never {
                    error!("Locking scrolling to work around gtk crash");
                    pane.workaround_scroller().set_vscrollbar_policy(gtk::PolicyType::Never);
                }
            }
        }

        self.contents.apply_snapshot(snap, self.settings.sort);
        if let Some(search) = self.pane.search() {
            todo!()
            // TODO [search]
            // self.search.apply_tab_snapshot(snap, self.settings.sort);
        }
    }

    pub fn apply_snapshot(&mut self, left: &mut [Self], right: &mut [Self], snap: DirSnapshot) {
        let start = Instant::now();
        assert!(self.matches_snapshot(&snap.id));
        let snap: EntryObjectSnapshot = snap.into();


        let force_search_sort = snap.had_search_updates;

        // The actual tab order doesn't matter here, so long as it's applied to every matching tab.
        // Avoid one clone by doing the other tabs first.
        self.matching_mut(right).for_each(|t| t.apply_snapshot_inner(snap.clone()));

        let kind = snap.id.kind;
        let len = snap.entries.len();

        self.apply_snapshot_inner(snap);

        if force_search_sort {
            // We just sorted this tab and all exact matching tabs to the right.
            left.iter_mut().for_each(Self::search_sort_after_snapshot);
            right
                .iter_mut()
                .filter(|t| !t.matches_arc(self.dir.path()))
                .for_each(Self::search_sort_after_snapshot);
        }

        debug!(
            "Applied {kind:?} snapshot for {:?} with {len} items in {:?}",
            self.dir.path(),
            start.elapsed()
        );

        if !kind.finished() {
            // Not completed, no changes to dir loading state.
            return;
        }

        // If we're finished, update all tabs to reflect this.
        let (updates, start) = self.dir.take_loaded();

        for u in updates {
            self.matched_flat_update(left, right, u);
        }

        self.finish_flat_load();
        // there will be no exact matches to the left.
        self.matching_mut(right).for_each(Self::finish_flat_load);
        info!("Finished loading {:?} in {:?}", self.dir.path(), start.elapsed());
    }

    pub fn matches_flat_update(&self, ev: &Update) -> bool {
        // Not watching anything -> cannot match an update
        let Some(watch) = self.dir.state().watched() else {
            return false;
        };

        Some(&**self.dir.path()) == ev.path().parent()
    }

    pub fn matched_flat_update(&mut self, left: &mut [Self], right: &mut [Self], up: Update) {
        assert!(self.matches_flat_update(&up));

        let Some(up) = self.dir.consume(up) else {
            return;
        };

        // If it exists in any tab (search or flat)
        let existing_global = EntryObject::lookup(up.path());
        // If it exists in flat tabs (which all share the same state).
        let local_position = existing_global
            .clone()
            .and_then(|eg| self.contents.position_by_sorted_entry(&eg.get()));


        // If something exists globally but not locally, it might need special handling.
        let mut search_mutate = None;

        let partial = match (up, local_position, existing_global) {
            // It simply can't exist locally but not globally
            (_, Some(_), None) => unreachable!(),
            (Update::Entry(entry), Some(pos), Some(obj)) => {
                let Some(old) = obj.update(entry) else {
                    // An unimportant update.
                    // No need to check other tabs.
                    return;
                };

                self.contents.reinsert_updated(pos, &obj);

                trace!("Updated {:?} from event", old.abs_path);
                PartiallyAppliedUpdate::Mutate(old, obj)
            }
            (Update::Entry(entry), None, Some(obj)) => {
                // This means the element already existed in a search tab, somewhere else, and
                // we're updating it.
                //
                // It does not exist within this search tab.
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


                self.contents.insert(&obj, self.settings.sort);
                trace!("Inserted existing {:?} from event", obj.get().abs_path);
                PartiallyAppliedUpdate::Insert(obj)
            }
            (Update::Entry(entry), None, None) => {
                let new = EntryObject::new(entry);

                self.contents.insert(&new, self.settings.sort);
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
                // It's possible this entry is living in a yet-to-be applied search snapshot, but
                // that's not important since it will be skipped over by apply_search_snapshot.
                return;
            }
        };


        // None of the tabs in `left` are an exact match.
        for t in self.matching_mut(right) {
            t.contents.finish_update(&partial);
        }


        // An insert in this tab is also an insert in this sort.
        // Very often this will be the only sort.
        self.finish_search_update(&partial);


        // Apply to any matching search tabs, even to the left.
        let other_tab_search_update = search_mutate.unwrap_or(partial);
        for t in left.iter_mut().chain(right.iter_mut()) {
            t.finish_search_update(&other_tab_search_update)
        }
    }

    fn search_sort_after_snapshot(&mut self) {
        let Some(search) = self.pane.search() else {
            return;
        };
        // TODO [search]
    }

    fn search_extend(&mut self) {
        let Some(search) = self.pane.search() else {
            return;
        };
        // TODO [search]
    }

    // It hasn't matched any tabs, but it might match some searches.
    //
    // Generally, updates will match at least one tab, so we will rarely hit this without a search
    // actually existing.
    pub fn handle_unmatched_update(tabs: &mut [Self], update: Update) {
        let existing = EntryObject::lookup(update.path());
        let partial = match (update, existing) {
            (Update::Entry(entry), None) => PartiallyAppliedUpdate::Insert(EntryObject::new(entry)),
            (Update::Entry(entry), Some(obj)) => {
                let Some(old) = obj.update(entry) else {
                    return;
                };
                PartiallyAppliedUpdate::Mutate(old, obj)
            }
            (Update::Removed(_), None) => {
                return;
            }
            (Update::Removed(_), Some(existing)) => PartiallyAppliedUpdate::Delete(existing),
        };

        for t in tabs {
            t.finish_search_update(&partial);
        }
    }

    fn finish_search_update(&mut self, update: &PartiallyAppliedUpdate) {
        let Some(search) = self.pane.search() else {
            return;
        };

        todo!()

        // TODO [search] it's possible for a Mutate to need to be treated as an insert for other
        // tabs. We promote insertions into mutates into insertions if they match a tab, but not
        // all searches may be up-to-date with those.
        // if !searching Returns
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
            self.change_location(left, right, NavTarget::assume_dir(path));
            true
        } else {
            false
        }
    }

    pub fn update_sort(&mut self, sort: SortSettings) {
        self.settings.sort = sort;
        self.contents.sort(sort);
        if let Some(p) = self.pane.get_mut() {
            p.update_settings(self.settings, &self.contents)
        }
        self.save_settings();
    }

    pub fn update_display_mode(&mut self, mode: DisplayMode) {
        self.settings.display_mode = mode;
        if let Some(p) = self.pane.get_mut() {
            p.update_settings(self.settings, &self.contents)
        }
        self.save_settings();
    }

    // Changes location without managing history or view states.
    // Returns the navigation target if nothing happens
    fn change_location(
        &mut self,
        left: &[Self],
        right: &[Self],
        target: NavTarget,
    ) -> Option<NavTarget> {
        if **self.dir.path() == *target.dir {
            return Some(target);
        }

        self.element.flat_title(&target.dir);

        self.view_state = None;
        if self.pane.search().is_some() {
            // TODO [search]
            // replace existing search pane with flat pane
            // And make sure said flat pane is empty.
            todo!()
        }

        // It is important we always create a new Arc here.
        self.dir = FlatDir::new(target.dir);

        if !self.copy_from_donor(left, right) {
            // We couldn't find any state to steal, so we know we're the only matching tab.

            let old_settings = self.settings;
            self.settings = gui_run(|g| g.database.get(self.dir.path()));
            self.view_state = Some(SavedViewState::for_jump(target.scroll));

            // Deliberately do not clear or update self.contents here, not yet.
            // This allows us to keep something visible just a tiny bit longer.
            // Safe enough when the settings match.
            if self.pane.flat().is_some() && self.settings == old_settings {
                self.contents.mark_stale();
            } else {
                self.contents.clear(self.settings.sort);
            }
        }

        if let Some(pane) = self.pane.flat() {
            pane.update_location(self.dir.path(), self.settings, &self.contents);
            self.load(left, right);
        }
        None
    }

    pub fn set_active(&mut self) {
        assert!(self.visible(), "Called set_active on a tab that isn't visible");
        self.pane.get_mut().unwrap().set_active(true);
        self.element.set_active(true);
    }

    pub fn set_inactive(&mut self) {
        if let Some(pane) = self.pane.get_mut() {
            pane.set_active(false);
        }
        self.element.set_active(false);
    }

    pub fn navigate(&mut self, left: &[Self], right: &[Self], target: NavTarget) {
        info!("Navigating {:?} from {:?} to {:?}", self.id, self.dir.path(), target);

        let state = self.take_view_snapshot().unwrap_or_default();
        let history = HistoryEntry { location: self.dir(), state };

        let Some(target) = self.change_location(left, right, target) else {
            self.past.push(history);
            self.future.clear();
            return;
        };

        let Some(path) = target.scroll else {
            return;
        };

        let mut current_state =
            self.view_state.take().or_else(|| self.take_view_snapshot()).unwrap_or_default();

        current_state.scroll_pos = Some(ScrollPosition { path, index: 0 });
        self.view_state = Some(current_state);

        if matches!(&*self.dir.state(), DirState::Loaded(_)) {
            self.apply_view_state();
        }
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
        todo!()
    }

    pub(super) fn backward(&mut self, left: &[Self], right: &[Self]) {
        todo!()
    }

    // Goes to the child directory if one was previous open or if there's only one.
    pub(super) fn child(&mut self, left: &[Self], right: &[Self]) {
        todo!()
    }

    pub(super) fn unload_if_not_matching(&mut self) {
        // Transition tab to an Inactive state only if Opened
        // Clear out all items
        //
        todo!()
    }

    #[must_use]
    fn take_view_snapshot(&self) -> Option<SavedViewState> {
        self.pane.get().map(|p| p.get_view_state(&self.contents))
    }

    fn save_view_snapshot(&mut self) {
        self.view_state = self.take_view_snapshot();
        debug!("Saved state {:?} for {:?}", self.view_state, self.id());
    }

    fn finish_flat_load(&mut self) {
        if let Some(search) = self.pane.search() {
            todo!()
        } else {
            let e = self.element.imp();
            e.spinner.stop();
            e.spinner.set_visible(false);
        }

        self.apply_view_state();
    }

    fn apply_view_state(&mut self) {
        let Some(pane) = self.pane.get_mut() else {
            trace!("Ignoring start_apply_view_state on tab {:?} with no pane", self.id);
            return;
        };

        if self.settings.display_mode == DisplayMode::Icons {
            error!("Unsetting GTK crash workaround");
        }
        // Unconditionally unset it in case mode was swapped.
        pane.workaround_scroller().set_vscrollbar_policy(gtk::PolicyType::Automatic);

        if let Some(search) = self.pane.search() {
            // TODO [search]
            // It might be possible to switch to search through a &mut dyn Pane
            return;
        }

        let Some(pane) = self.pane.flat() else {
            warn!("Pane was closed after we asked to apply view state");
            return;
        };

        let view_state = self.view_state.take().unwrap_or_default();

        // self.pane.apply_view_state
        pane.apply_view_state(view_state, &self.contents);
    }

    pub fn attach_pane<F: FnOnce(&Widget)>(&mut self, attach: F) {
        if self.visible() {
            debug!("Pane already displayed");
            return;
        }

        if let Some(search) = self.pane.search() {
            todo!();
        } else {
            let pane = Pane::new_flat(
                self.id(),
                self.dir.path(),
                self.settings,
                &self.contents.selection,
                attach,
            );
            self.pane = CurrentPane::Flat(pane);
        }

        self.element.set_pane_visible(true);

        if matches!(&*self.dir.state(), DirState::Loaded(_)) {
            self.apply_view_state();
        }
    }

    // Doesn't start loading
    pub fn replace_pane(&mut self, other: &mut Self) {
        trace!("Replacing pane for {:?} with pane from {:?}", other.id(), self.id());
        if self.visible() {
            debug!("Pane already displayed");
            return;
        }

        let old = other.take_pane().unwrap();

        if let Some(search) = self.pane.search() {
            todo!()
        }

        let pane = Pane::new_flat(
            self.id(),
            self.dir.path(),
            self.settings,
            &self.contents.selection,
            |new| old.replace(new),
        );

        self.pane = CurrentPane::Flat(pane);
        self.element.set_pane_visible(true);


        if matches!(&*self.dir.state(), DirState::Loaded(_)) {
            self.apply_view_state();
        }
    }

    pub fn close_pane(&mut self) {
        if self.pane.get().is_some() {
            drop(self.take_pane());
        }
        self.element.set_active(false);
    }

    pub fn next_of_kin_by_pane(&self) -> Option<TabId> {
        self.pane.get().and_then(PaneExt::next_of_kin)
    }

    fn take_pane(&mut self) -> Option<Pane> {
        if self.pane.get().is_none() {
            error!("Called take_pane on tab with no pane");
            // Probably should panic here.
            return None;
        }

        // Take the pane,
        // TODO [search]
        if matches!(&*self.dir.state(), DirState::Loaded(_)) {
            self.save_view_snapshot();
        }
        self.element.set_pane_visible(false);

        match std::mem::take(&mut self.pane) {
            CurrentPane::Nothing => unreachable!(),
            CurrentPane::Flat(p) => Some(p),
            CurrentPane::Search(sp) => todo!(),
        }
    }

    fn save_settings(&self) {
        gui_run(|g| {
            g.database.store(self.dir.path(), self.settings);
        });
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
