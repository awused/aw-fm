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
use gtk::{glib, Box, MultiSelection, Orientation, ScrolledWindow};
use path_clean::PathClean;

use super::contents::Contents;
use super::element::TabElement;
use super::id::{TabId, TabUid};
use super::pane::{Pane, PaneExt};
use super::search::SearchPane;
use super::{HistoryEntry, SavedViewState};
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

#[derive(Debug)]
struct WatchedDir {
    path: Arc<Path>,
    cancel: Arc<AtomicBool>,
}

impl Drop for WatchedDir {
    fn drop(&mut self) {
        self.cancel.store(true, Relaxed);
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
enum DirState {
    #[default]
    Unloaded,
    Loading {
        watch: WatchedDir,
        pending_updates: Vec<Update>,
    },
    // ReOpening{} -- Transitions to Opening once a Start arrives, or Opened with Complete
    // UnloadedWhileOpening -- takes snapshots and drops them
    Loaded(WatchedDir),
}

impl DirState {
    const fn watched(&self) -> Option<&WatchedDir> {
        match self {
            Self::Unloaded => None,
            Self::Loading { watch, .. } | Self::Loaded(watch) => Some(watch),
        }
    }

    const fn loaded(&self) -> bool {
        match self {
            Self::Unloaded | Self::Loading { .. } => false,
            Self::Loaded(_) => true,
        }
    }
}

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
    path: Arc<Path>,
    settings: DirSettings,
    dir_state: Rc<RefCell<DirState>>,
    contents: Contents,
    // TODO -- this should only store snapshots and be sporadically updated/absent
    view_state: Option<SavedViewState>,
    history: VecDeque<HistoryEntry>,
    future: VecDeque<HistoryEntry>,
    element: TabElement,

    // Each tab can only be open in one pane at once.
    // In theory we could do many-to-many but it's too niche.
    pane: CurrentPane,
}

impl Drop for Tab {
    fn drop(&mut self) {
        todo!()
    }
}

impl Tab {
    pub const fn id(&self) -> TabId {
        self.id.copy()
    }

    pub fn visible(&self) -> bool {
        self.pane.get().is_some()
    }

    pub fn matches_arc(&self, other: &Arc<Path>) -> bool {
        Arc::ptr_eq(&self.path, other)
    }

    fn matching_mut<'a>(&'a self, other: &'a mut [Self]) -> impl Iterator<Item = &mut Self> {
        other.iter_mut().filter(|t| self.matches_arc(&t.path))
    }

    fn matching<'a>(&'a self, other: &'a [Self]) -> impl Iterator<Item = &Self> {
        other.iter().filter(|t| self.matches_arc(&t.path))
    }

    pub fn new(id: TabUid, path: PathBuf, existing_tabs: &[Self]) -> (Self, TabElement) {
        // Clean the path without resolving symlinks.
        let mut path = path.clean();
        if path.is_relative() {
            error!("Dealing with a relative directory, this shouldn't happen");
            let mut abs = current_dir().unwrap();
            abs.push(path);
            path = abs.clean();
        }

        let path: Arc<Path> = path.into();

        // fetch metatada synchronously, even with a donor
        let settings = gui_run(|g| g.database.get(&path));

        let dir_state = Rc::new(RefCell::new(DirState::Unloaded));

        let contents = Contents::new(settings.sort);

        let element = TabElement::new(id.copy(), "TODO -- title");

        let mut t = Self {
            id,
            path,
            settings,
            dir_state,
            contents,
            view_state: None,
            history: VecDeque::new(),
            future: VecDeque::new(),
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
        contents.clone_from(&source.contents, source.settings.sort);
        // TODO -- clone tab_element state, like spinner

        (
            Self {
                id,
                path: source.path.clone(),
                settings: source.settings,
                dir_state: source.dir_state.clone(),
                contents,
                view_state,
                history: source.history.clone(),
                future: source.future.clone(),
                element: todo!(),
                pane: CurrentPane::default(),
            },
            todo!(),
        )
    }

    fn copy_from_donor(&mut self, left: &[Self], right: &[Self]) -> bool {
        // TODO -- clone tab_element state from donor, like spinner
        for t in left.iter().chain(right) {
            // Check for value equality, not reference equality
            if *self.path == *t.path {
                self.path = t.path.clone();
                self.dir_state = t.dir_state.clone();
                self.contents.clone_from(&t.contents, self.settings.sort);

                return true;
            }
        }
        false
    }

    // Only make this take &mut [Self] if truly necessary
    pub fn load(&mut self, left: &[Self], right: &[Self]) {
        let mut sb = self.dir_state.borrow_mut();
        let dstate = &mut *sb;
        match dstate {
            DirState::Loading { .. } | DirState::Loaded(_) => return,
            DirState::Unloaded => {
                debug!("Opening directory for {:?}", self.path);
                let watch = WatchedDir::start(self.path.clone());

                *dstate = DirState::Loading { watch, pending_updates: Vec::new() };
            }
        }
        drop(sb);

        // TODO -- spinners
        //self.show_spinner()
        for t in self.matching(left).chain(self.matching(right)) {
            //t.show_spinner();
        }
    }

    pub fn matches_snapshot(&self, snap: &SnapshotId) -> bool {
        if !Arc::ptr_eq(&self.path, &snap.path) {
            return false;
        }

        use SnapshotKind::*;

        let sb = self.dir_state.borrow();

        match (&snap.kind, &*sb) {
            (_, DirState::Unloaded | DirState::Loaded(_)) => false,
            (Complete | Start | Middle | End, DirState::Loading { watch, .. }) => {
                Arc::ptr_eq(&watch.cancel, &snap.id)
            }
        }
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
        // TODO [search]
        // self.search.apply_tab_snapshot(snap, self.settings.sort);
        self.search_extend();
    }

    // Returns true if we had any search updates. Expected to be rare, so that path isn't
    // terribly efficient.
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
            // We just sorted this tab and all matching tabs to the right.
            left.iter_mut().for_each(Self::search_sort_after_snapshot);
            right
                .iter_mut()
                .filter(|t| !t.matches_arc(&self.path))
                .for_each(Self::search_sort_after_snapshot);
        }

        debug!(
            "Applied {kind:?} snapshot for {:?} with {len} items in {:?}",
            self.path,
            start.elapsed()
        );

        if !kind.finished() {
            // Not completed, no changes to dir loading state.
            return;
        }

        // If we're finished, update all tabs to reflect this.
        let mut sb = self.dir_state.borrow_mut();
        let updates = match std::mem::take(&mut *sb) {
            DirState::Unloaded | DirState::Loaded(_) => unreachable!(),
            DirState::Loading { watch, pending_updates } => {
                *sb = DirState::Loaded(watch);
                pending_updates
            }
        };

        drop(sb);

        for u in updates {
            self.matched_update(left, right, u);
        }

        self.start_apply_view_state();
        self.matching_mut(right).for_each(Self::start_apply_view_state);
        // TODO -- stop showing spinners.
        info!("Finished loading {:?}", self.path);
    }

    pub fn matches_update(&self, ev: &Update) -> bool {
        // Not watching anything -> cannot match an update
        let Some(watch) = self.dir_state.borrow().watched() else {
            return false;
        };

        Some(&*self.path) == ev.path().parent()
    }

    pub fn matched_update(&mut self, left: &mut [Self], right: &mut [Self], up: Update) {
        assert!(self.matches_update(&up));

        let mut sb = self.dir_state.borrow_mut();

        match &mut *sb {
            DirState::Unloaded => unreachable!(),
            DirState::Loading { pending_updates, .. } => {
                pending_updates.push(up);
                return;
            }
            DirState::Loaded(_) => {}
        }
        drop(sb);


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
                        "Search tab read item {:?} before it was inserted into a flat tab.",
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
                trace!("Inserted existing {:?} from event", new.get().abs_path);
                PartiallyAppliedUpdate::Insert(new)
            }
            (Update::Removed(path), Some(i), Some(obj)) => {
                self.contents.remove(i);
                trace!("Removed {:?} from event", path);
                PartiallyAppliedUpdate::Delete(obj)
            }
            (Update::Removed(path), None, Some(obj)) => {
                trace!("Removed {:?} from event", path);
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


        for t in self.matching_mut(right) {
            t.contents.finish_update(&partial);
        }


        // We know that an insert in this tab is also an insert in this sort.
        // Very often this will be the only sort, and this is marginally more efficient.
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

        // TODO [search] it's possible for a Mutate to need to be treated as an insert for other
        // tabs. We promote insertions into mutates into insertions if they match a tab, but not
        // all searches may be up-to-date with those.
        // if !searching return
        error!("TODO -- finish_search_update")
    }

    pub fn check_directory_deleted(&self, removed: &Path) -> bool {
        removed == &*self.path
    }

    // Need to handle this to avoid the case where a directory is "Loaded" but no watcher is
    // listening for events.
    // Even if the directory is recreated, the watcher won't work.
    pub fn handle_directory_deleted(&mut self, left: &[Self], right: &[Self]) {
        let cur_path = self.path.clone();
        let mut path = &*cur_path;
        while !path.exists() || !path.is_dir() {
            path = path.parent().unwrap()
        }

        self.change_location(left, right, path);
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
    fn change_location(&mut self, left: &[Self], right: &[Self], target: &Path) {
        if &*self.path == target {
            return;
        }

        self.view_state = None;
        if self.pane.search().is_some() {
            // TODO [search]
            // replace existing search pane with flat pane
            // And make sure said flat pane is empty.
            todo!()
        }

        // It is important we always create a new Arc here.
        self.path = target.into();

        if !self.copy_from_donor(left, right) {
            // We couldn't find any state to steal, so we know we're the only matching tab.

            self.dir_state.replace(DirState::Unloaded);
            let old_settings = self.settings;
            self.settings = gui_run(|g| g.database.get(&self.path));
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
            pane.update_location(&self.path, self.settings, &self.contents);
            self.load(left, right);
        }
    }

    pub fn navigate(&mut self, left: &[Self], right: &[Self], target: &Path) {
        info!("Navigating from {:?} to {:?}", self.path, target);

        // TODO -- view snapshots
        // self.take_view_snapshot()take_view_snapshot
        // push onto history


        self.change_location(left, right, target)
    }

    pub fn parent(&mut self, left: &[Self], right: &[Self]) {
        let Some(parent) = self.path.parent() else {
            warn!("No parent for {:?}", self.path);
            return;
        };

        let target = parent.to_path_buf();
        self.navigate(left, right, &target)
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
        // Only called on active tab
        error!("TODO -- take_view_snapshot");
        None
    }

    fn save_view_snapshot(&mut self) {
        self.view_state = self.take_view_snapshot();
    }

    fn start_apply_view_state(&mut self) {
        let Some(pane) = self.pane.get_mut() else {
            trace!("Ignoring start_apply_view_state on tab {:?} with no pane", self.id);
            return;
        };


        if self.settings.display_mode == DisplayMode::Icons {
            error!("Unsetting GTK crash workaround");
            pane.workaround_scroller().set_vscrollbar_policy(gtk::PolicyType::Automatic);
            // glib::timeout_add_local_once(Duration::from_millis(300), finish);
        }

        // Doing this immediately can cause it to be skipped
        // let id = self.id.copy();
        // TODO -- inline once we confirm new scrolling code doesn't crash GTK
        self.finish_apply_view_state();
        // let finish = move || tabs_run(|t| t.finish_apply_view_state(id));
        // glib::idle_add_local_once(finish);
    }

    // Doesn't start loading yet.
    fn replace_pane(&mut self, visible: &mut Self) {
        visible.take_pane().unwrap();
        todo!()
    }

    pub fn new_pane(&mut self, parent: &gtk::Box) {
        if let Some(pane) = self.pane.get_mut() {
            debug!("Pane already displayed");
            return;
        }


        let pane =
            Pane::new_flat(self.id(), &self.path, self.settings, &self.contents.selection, parent);
        self.pane = CurrentPane::Flat(pane);

        if matches!(&*self.dir_state.borrow(), DirState::Loaded(_)) {
            self.start_apply_view_state();
        }
    }

    pub fn close_pane(&mut self) {
        if self.pane.get().is_some() {
            drop(self.take_pane());
        }
    }

    fn take_pane(&mut self) -> Option<Pane> {
        if self.pane.get().is_none() {
            error!("Called take_pane on tab with no pane");
            // Probably should panic here.
            return None;
        }

        // Take the pane,
        // TODO [search]
        if matches!(&*self.dir_state.borrow(), DirState::Loaded(_)) {
            self.take_view_snapshot();
        }

        match std::mem::take(&mut self.pane) {
            CurrentPane::Nothing => unreachable!(),
            CurrentPane::Flat(p) => Some(p),
            CurrentPane::Search(sp) => todo!(),
        }
    }

    fn finish_apply_view_state(&mut self) {
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
        pane.apply_view_state(view_state);
    }

    fn save_settings(&self) {
        gui_run(|g| {
            g.database.store(&self.path, self.settings);
        });
    }
}
