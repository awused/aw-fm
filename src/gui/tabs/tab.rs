use std::borrow::Borrow;
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::gio::ListStore;
use gtk::glib::Object;
use gtk::prelude::{Cast, ListModelExt, ListModelExtManual, StaticType};
use gtk::traits::{AdjustmentExt, BoxExt, SelectionModelExt, WidgetExt};
use gtk::{glib, Box, MultiSelection, Orientation, ScrolledWindow};
use path_clean::PathClean;

use self::pane::Pane;
use super::TabId;
use crate::com::{
    DirSettings, DirSnapshot, DisplayMode, Entry, EntryObject, EntryObjectSnapshot, FileTime,
    GuiAction, GuiActionContext, ManagerAction, SnapshotId, SnapshotKind, SortMode, SortSettings,
};
use crate::gui::{Update, GUI};
use crate::natsort::ParsedString;

mod pane;


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
}

impl Drop for WatchedDir {
    fn drop(&mut self) {
        GUI.with(|g| {
            let g = g.get().unwrap();
            g.send_manager((
                ManagerAction::Close(self.path.clone()),
                GuiActionContext::default(),
                None,
            ));
        });
    }
}

impl WatchedDir {
    fn start(path: Arc<Path>) -> Rc<Self> {
        GUI.with(|g| {
            let g = g.get().unwrap();
            g.send_manager((ManagerAction::Open(path.clone()), GuiActionContext::default(), None));
        });

        Rc::new(Self { path })
    }
}


// A new object needs to be copied between every relevant ListStore
#[derive(Debug)]
enum InsertOrDelete {
    Insert(EntryObject),
    Delete(EntryObject),
}

#[derive(Debug)]
enum PartiallyAppliedUpdate {
    // Updates without any potential sort change (rare) would be fully applied, but usually mtime
    // will change, so no sense worrying about it.
    Mutate(Entry),
    InsertOrDelete(InsertOrDelete),
}


// Applying an Event gives us an update, which may require a sort, or a new object, which always
// requires a sort.
// Removals do not require sorting on their own.
#[derive(Debug)]
enum PendingUpdates {
    Nothing,
    Unapplied(Vec<Update>),
    // We always sort when appending a snapshot, so there's no need for tracking whether some
    // of the updates were mutations.
    PartiallyApplied(Vec<InsertOrDelete>),
}

// Despite being complicated, this is Clone rather than being !Clone and using refcounting.
// If the ListStores were all stored between tabs this, too, could be shared between tabs.
#[derive(Debug, Default, Clone)]
enum LoadingState {
    #[default]
    Unloaded,
    Loading {
        watch: Rc<WatchedDir>,
        pending_events: Rc<RefCell<PendingUpdates>>,
        // needed_resort
        //pending_scroll: f64,
        // spinner: enum Displayed/pending
    },
    // ReOpening{} -- Transitions to Opening once a Start arrives, or Opened with Complete
    // UnloadedWhileOpening -- takes snapshots and drops them
    Loaded(Rc<WatchedDir>),
}

impl LoadingState {
    fn watched(&self) -> Option<&WatchedDir> {
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

#[derive(Debug, Clone)]
struct HistoryEntry {
    // This is intentionally not the same Arc<Path> we use for active tabs.
    // If there is a matching tab when we activate this history entry, steal that Arc and state.
    // If there is none, we need a new, fresh Arc<> that definitely has no pending snapshots.
    location: Rc<Path>,
    scroll_pos: f64, // View snapshot?
}

struct Contents {
    list: ListStore,
    selection: MultiSelection,
}

impl fmt::Debug for Contents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Contents size: {}", self.list.n_items())
    }
}

impl Default for Contents {
    fn default() -> Self {
        let list = ListStore::new(EntryObject::static_type());
        let selection = MultiSelection::new(Some(list.clone()));
        Self { list, selection }
    }
}

impl Clone for Contents {
    fn clone(&self) -> Self {
        let list = ListStore::new(EntryObject::static_type());
        let selection = MultiSelection::new(Some(list.clone()));

        for item in &self.list {
            list.append(&item.unwrap());
        }

        Self { list, selection }
    }
}


// Not kept up to date, maybe an enum?
#[derive(Debug, Clone)]
struct SavedViewState {
    pub scroll_pos: f64,
    // Selected items?
}


// Current limitations:
// - Tabs cannot be unloaded, refreshed, or reopened while they're opening
//   - Tabs can be closed while opening
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
pub(in crate::gui) struct Tab {
    // displayed_path: PathBuf,
    id: TabId,
    path: Arc<Path>,
    // visible: bool, -- whether the tab contents are currently visible -- only needed to support
    // paned views.
    settings: DirSettings,
    loading: LoadingState,
    contents: Contents,
    // TODO -- this should only store snapshots and be sporadically updated/absent
    view_state: Option<SavedViewState>,
    history: VecDeque<HistoryEntry>,
    future: Vec<HistoryEntry>,
    tab_element: (),
    // Each tab can only be open in one pane at once.
    // In theory we could do many-to-many but it's too niche.
    pane: OnceCell<Pane>,
}

impl Tab {
    pub(super) const fn id(&self) -> TabId {
        self.id
    }

    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.path, &other.path)
    }

    pub(super) fn new(id: TabId, path: PathBuf, element: (), existing_tabs: &[Self]) -> Self {
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
        let settings = GUI.with(|g| g.get().unwrap().database.get(&path));

        let loading = LoadingState::Unloaded;

        let contents = Contents::default();

        let mut t = Self {
            id,
            path,
            settings,
            loading,
            contents,
            view_state: None,
            history: VecDeque::new(),
            future: Vec::new(),
            tab_element: element,
            pane: OnceCell::new(),
        };

        t.copy_from_donor(existing_tabs, &[]);

        t
    }

    pub(super) fn cloned(id: TabId, source: &Self, element: ()) -> Self {
        // Assumes inactive tabs cannot be cloned.
        let view_state = source.take_view_snapshot();

        Self {
            id,
            path: source.path.clone(),
            settings: source.settings,
            loading: source.loading.clone(),
            contents: source.contents.clone(),
            view_state,
            history: source.history.clone(),
            future: source.future.clone(),
            tab_element: element,
            pane: OnceCell::new(),
        }
    }

    fn copy_from_donor(&mut self, left_tabs: &[Self], right_tabs: &[Self]) {
        for t in left_tabs.iter().chain(right_tabs) {
            // Check for value equality, not reference equality
            if *self.path == *t.path {
                self.path = t.path.clone();
                self.loading = t.loading.clone();
                self.contents = t.contents.clone();

                let comparator = self.settings.sort.comparator();
                self.contents.list.sort(comparator);
                return;
            }
        }
    }

    pub(super) fn load(&mut self, left_tabs: &mut [Self], right_tabs: &mut [Self]) {
        match self.loading {
            LoadingState::Loading { .. } | LoadingState::Loaded(_) => (),
            LoadingState::Unloaded => {
                debug!("Opening directory for {:?}", self.path);
                let watch = WatchedDir::start(self.path.clone());

                let state = LoadingState::Loading {
                    watch,
                    pending_events: Rc::new(PendingUpdates::Nothing.into()),
                };

                // Clone the new state into all matching tabs
                for t in left_tabs.iter_mut().chain(right_tabs).filter(|t| t.matches(self)) {
                    assert!(
                        matches!(t.loading, LoadingState::Unloaded),
                        "Loading a directory with other tabs that were not unloaded"
                    );
                    t.loading = state.clone();
                }

                self.loading = state;
            }
        }
    }

    pub(super) fn matches_snapshot(&self, snap: &SnapshotId) -> bool {
        if !Arc::ptr_eq(&self.path, &snap.path) {
            return false;
        }

        use SnapshotKind::*;

        match (&snap.kind, &self.loading) {
            (_, LoadingState::Unloaded) => {
                // TODO -- find some decent way of preventing two identical tabs from opening to
                // the same directory at once.
                unreachable!("Received {:?} snapshot for an unloaded tab", snap.kind);
            }
            (_, LoadingState::Loaded(_)) => {
                unreachable!("Received {:?} snapshot for loaded tab.", snap.kind);
            }
            (Complete | Start | Middle | End, LoadingState::Loading { .. }) => true,
        }
    }

    fn apply_obj_snapshot(&mut self, snap: EntryObjectSnapshot) {
        assert!(self.matches_snapshot(&snap.id));
        debug!(
            "Applying {:?} snapshot for {:?} with {} items",
            snap.id.kind,
            self.path,
            snap.entries.len()
        );

        if snap.id.kind.initial() {
            assert!(self.contents.list.n_items() == 0);
        }

        self.contents.list.extend(snap.entries.into_iter());
        let start = Instant::now();
        self.contents.list.sort(self.settings.sort.comparator());
        println!("sort time {:?}", start.elapsed());

        // let a = self.contents.list.item(0).unwrap();
        // let settings = self.settings;
        // glib::timeout_add_local_once(Duration::from_secs(5), move || {
        //     let b = a.downcast::<EntryObject>().unwrap();
        //     let mut c = b.get().clone();
        //     c.name = ParsedString::from(OsString::from("asdf"));
        //     error!("Updating file for no reason");
        //     b.update(c, settings.sort);
        // });

        if snap.id.kind.finished() {
            match std::mem::take(&mut self.loading) {
                LoadingState::Unloaded | LoadingState::Loaded(_) => unreachable!(),
                LoadingState::Loading { watch, pending_events } => {
                    self.loading = LoadingState::Loaded(watch);
                    // TODO -- apply pending_events
                    // if unapplied -> pending_events = PartiallyApplied
                }
            }

            // TODO -- if active apply view snapshot
            // If no view snapshot, scroll to top.
            // if active {
            //     self.apply_view_snapshot();
            // }
        }
    }

    pub(super) fn apply_snapshot(&mut self, right_tabs: &mut [Self], snap: DirSnapshot) {
        assert!(self.matches_snapshot(&snap.id));
        let snap: EntryObjectSnapshot = snap.into();

        // The actual tab order doesn't matter, so long as it's applied to every matching tab.
        for t in right_tabs.iter_mut().filter(|t| t.matches_snapshot(&snap.id)) {
            t.apply_obj_snapshot(snap.clone());
        }

        self.apply_obj_snapshot(snap);
    }

    pub(super) fn matches_update(&self, ev: &Update) -> bool {
        // Not watching anything -> cannot match an update
        let Some(watch) = self.loading.watched() else {
            return false;
        };

        Some(&*self.path) == ev.path().parent()
    }

    fn finish_update(&mut self, ev: &PartiallyAppliedUpdate) {}

    // Make a best-effort attempt to find the existing item efficiently, then fall back to a
    // naive linear search if we don't find it.
    // The list may not still be in sorted order, so it's possible for binary searching to fail.
    //
    // Mtime sort mode makes this particularly annoying, but given that the most recently updated
    // item is also pretty likely to be the next updated item, this is worth attempting.
    //
    // The only sure-fire mechanism is keeping a hashmap, which isn't the worst idea but
    // possibly isn't necessary.
    fn search_by_maybe_mismatched_entry(&self, entry: &Entry) -> Option<usize> {
        let mut start = 0;
        let mut end = self.contents.list.n_items();
        if end == 0 {
            return None;
        }

        let comp = self.settings.sort.comparator();

        while start < end {
            let mid = start + (end - start) / 2;

            let obj = self.contents.list.item(mid).unwrap().downcast::<EntryObject>().unwrap();

            let inner = obj.get();
            if inner.abs_path == entry.abs_path {
                return Some(mid as usize);
            }

            match entry.cmp(&inner, self.settings.sort) {
                Ordering::Equal => unreachable!(),
                Ordering::Less => end = mid,
                Ordering::Greater => start = mid + 1,
            }
        }

        // Names can't change, because that just becomes a new file.
        // Assuming a file can't atomically become a directory and vice versa, this is safe.
        if self.settings.sort.mode == SortMode::Name {
            return None;
        }

        self.linear_search_path(&entry.abs_path)
    }

    // Last resort/fallback search
    fn linear_search_path(&self, path: &Path) -> Option<usize> {
        self.contents
            .list
            .iter::<EntryObject>()
            .position(|r| r.unwrap().get().abs_path == path)
    }

    // Apply a single event, inefficient for many pending events but eh.
    pub(super) fn apply_single_update(&mut self, right_tabs: &mut [Self], ev: Update) {
        assert!(self.matches_update(&ev));

        match &mut self.loading {
            LoadingState::Unloaded => unreachable!(),
            LoadingState::Loading { pending_events, .. } => {
                let mut pe = pending_events.borrow_mut();
                match &mut *pe {
                    // PartiallyApplied should be a transient state that never outlives a
                    // Complete/End snapshot being finalized.
                    PendingUpdates::PartiallyApplied(_) => unreachable!(),
                    PendingUpdates::Nothing => {
                        *pe = PendingUpdates::Unapplied(vec![ev]);
                    }
                    PendingUpdates::Unapplied(pending) => pending.push(ev),
                }

                return;
            }
            LoadingState::Loaded(_) => {}
        }


        let start = Instant::now();
        let index = match &ev {
            Update::Entry(entry) => self.search_by_maybe_mismatched_entry(entry),
            Update::Removed(path) => self.linear_search_path(path),
        };
        println!("{:?}", start.elapsed());

        let partial = match (ev, index) {
            (Update::Entry(entry), Some(i)) => {
                let obj = self.contents.list.item(i as u32).unwrap();
                let obj = obj.downcast::<EntryObject>().unwrap();

                let Some(old) = obj.update(entry) else {
                    // An unimportant update.
                    // No need to check other tabs.
                    return;
                };

                println!("{:?}", start.elapsed());
                let was_selected = self.contents.selection.is_selected(i as u32);
                self.contents.list.remove(i as u32);

                // After removing the lone updated item, the list is guaranteed to be sorted.
                // So we can reinsert it much more cheaply than sorting the entire list again.
                let new_idx =
                    self.contents.list.insert_sorted(&obj, self.settings.sort.comparator());
                if was_selected {
                    self.contents.selection.select_item(new_idx, false);
                }

                println!("{:?}", start.elapsed());
                trace!("Updated {:?} from event", old.abs_path);
                PartiallyAppliedUpdate::Mutate(old)
            }
            (Update::Entry(entry), None) => {
                let new = EntryObject::new(entry);

                self.contents.list.insert_sorted(&new, self.settings.sort.comparator());

                trace!("Inserted {:?} from event", new.get().abs_path);
                PartiallyAppliedUpdate::InsertOrDelete(InsertOrDelete::Insert(new))
            }
            (Update::Removed(path), Some(i)) => {
                let obj = self.contents.list.item(i as u32).unwrap();
                let obj = obj.downcast::<EntryObject>().unwrap();
                self.contents.list.remove(i as u32);

                trace!("Removed {:?} from event", path);
                PartiallyAppliedUpdate::InsertOrDelete(InsertOrDelete::Delete(obj))
            }
            (Update::Removed(path), None) => {
                // Unusual case
                warn!("Got removal for {path:?} which wasn't present");
                return;
            }
        };

        error!("TODO  -- finish applying update to other tabs {partial:?}");
    }

    // Need to handle this to avoid the case where a directory is "Loaded" but no watcher is
    // listening for events.
    // Even if the directory is recreated, the watcher won't work.
    pub(super) fn check_directory_deleted(&self, removed: &Path) -> bool {
        removed == &*self.path
    }

    pub(super) fn handle_directory_deleted(&mut self, left_tabs: &[Self], right_tabs: &[Self]) {
        let cur_path = self.path.clone();
        let mut path = &*cur_path;
        while !path.exists() || !path.is_dir() {
            path = path.parent().unwrap()
        }
        self.navigate(left_tabs, right_tabs, path);
    }

    pub(super) fn update_sort(&mut self, sort: SortSettings) {
        self.settings.sort = sort;
        self.contents.list.sort(self.settings.sort.comparator());
        self.pane.get().unwrap().update_sort(sort);
        // TODO -- update database
    }

    pub(super) fn update_display_mode(&mut self, mode: DisplayMode) {
        self.settings.display_mode = mode;
        self.pane.get_mut().unwrap().update_mode(self.settings);
        // TODO -- update database
    }

    fn navigate(&mut self, left_tabs: &[Self], right_tabs: &[Self], target: &Path) {
        info!("Navigating from {:?} to {:?}", self.path, target);
        // Look for another matching tab and steal its state.
        // Only if that fails do we open a new watcher.
        // todo!()
    }

    pub(super) fn forward(&mut self) {
        todo!()
    }

    pub(super) fn backward(&mut self) {
        todo!()
    }

    pub(super) fn parent(&mut self) {
        todo!()
    }

    // Goes to the child directory if one was previous open or if there's only one.
    pub(super) fn child(&mut self) {
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
        GUI.with(|g| {
            // Only called on active tab
            error!("TODO -- take_view_snapshot");
        });

        None
    }

    fn save_view_snapshot(&mut self) {
        self.view_state = self.take_view_snapshot();
    }

    fn apply_view_snapshot(&mut self) {
        let Some(view_state) = &self.view_state else {
            return;
        };

        GUI.with(|g| {
            // Only called on active tab
            error!("TODO -- apply_view_snapshot");
        });
    }

    // TODO -- include index or have the TabsList allocate boxes and pass those down.
    pub(super) fn display(&mut self, parent: &gtk::Box) {
        self.pane.get_or_init(|| Pane::new(self)).display(parent);

        if matches!(self.loading, LoadingState::Loaded(_)) {
            self.apply_view_snapshot();
        }
    }

    fn hide(&mut self) {
        if matches!(self.loading, LoadingState::Loaded(_)) {
            self.take_view_snapshot();
        }
    }
}
