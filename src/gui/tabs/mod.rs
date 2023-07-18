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
use gtk::subclass::prelude::ObjectSubclassExt;
use gtk::traits::{AdjustmentExt, BoxExt, SelectionModelExt, WidgetExt};
use gtk::{glib, Box, MultiSelection, Orientation, ScrolledWindow};
use path_clean::PathClean;

use self::pane::Pane;
use crate::com::{
    DirSettings, DirSnapshot, DisplayMode, Entry, EntryObject, EntryObjectSnapshot, FileTime,
    GuiAction, GuiActionContext, ManagerAction, SnapshotId, SnapshotKind, SortMode, SortSettings,
};
use crate::gui::{Update, GUI};
use crate::natsort::ParsedString;

mod list;
mod pane;

use id::{TabId, TabUid};
pub(super) use list::TabsList;


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

        // TODO -- trigger a cleanup of dangling EntityObject weak refs
        // Doing it here should be okay, but would need to ensure that all liststores are cleared
        // out first.
    }
}

impl WatchedDir {
    fn start(path: Arc<Path>) -> Self {
        GUI.with(|g| {
            let g = g.get().unwrap();
            g.send_manager((ManagerAction::Open(path.clone()), GuiActionContext::default(), None));
        });

        Self { path }
    }
}


#[derive(Debug)]
enum PartiallyAppliedUpdate {
    // Updates without any potential sort change (rare) would be fully applied, but usually mtime
    // will change, so no sense worrying about it.
    Mutate(Entry, EntryObject),
    Insert(EntryObject),
    Delete(EntryObject),
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

impl Contents {
    fn clone_from(&mut self, source: &Self) {
        self.list.remove_all();

        self.list.extend(source.list.iter::<EntryObject>().flatten())
    }
}


// Not kept up to date, maybe an enum?
#[derive(Debug, Clone, Default)]
struct SavedViewState {
    pub scroll_pos: f64,
    // Selected items?
}

#[derive(Debug)]
struct Search();

// Current limitations:
// - Tabs cannot be unloaded, refreshed, or reopened while they're opening
//   - Tabs can be closed while opening
//   - Dropping this requirement isn't super hard but leaves room for bugs.
//   - Likely to be too niche to really matter.
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
struct Tab {
    // displayed_path: PathBuf,
    id: TabUid,
    path: Arc<Path>,
    // visible: bool, -- whether the tab contents are currently visible -- only needed to support
    // paned views.
    settings: DirSettings,
    dir_state: Rc<RefCell<DirState>>,
    contents: Contents,
    // TODO -- this should only store snapshots and be sporadically updated/absent
    view_state: Option<SavedViewState>,
    history: VecDeque<HistoryEntry>,
    future: Vec<HistoryEntry>,
    element: (),

    // Each tab can only be open in one pane at once.
    // In theory we could do many-to-many but it's too niche.
    pane: Option<Pane>,
    // Searches do not share state with other searches and aren't copied.
    // Each Search gets its own dedicated recursive notify watcher, if we care to listen for search
    // updates.
    search: Option<Search>,
}

impl Tab {
    const fn id(&self) -> TabId {
        self.id.copy()
    }

    fn matches_arc(&self, other: &Arc<Path>) -> bool {
        Arc::ptr_eq(&self.path, other)
    }

    fn matching_mut<'a>(&'a self, other: &'a mut [Self]) -> impl Iterator<Item = &mut Self> {
        other.iter_mut().filter(|t| self.matches_arc(&t.path))
    }

    fn matching<'a>(&'a self, other: &'a [Self]) -> impl Iterator<Item = &Self> {
        other.iter().filter(|t| self.matches_arc(&t.path))
    }

    pub(super) fn new(id: TabUid, path: PathBuf, element: (), existing_tabs: &[Self]) -> Self {
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

        let dir_state = Rc::new(RefCell::new(DirState::Unloaded));

        let contents = Contents::default();

        let mut t = Self {
            id,
            path,
            settings,
            dir_state,
            contents,
            view_state: None,
            history: VecDeque::new(),
            future: Vec::new(),
            element,
            pane: None,
            search: None,
        };

        t.copy_from_donor(existing_tabs, &[]);

        t
    }

    pub(super) fn cloned(id: TabUid, source: &Self, element: ()) -> Self {
        // Assumes inactive tabs cannot be cloned.
        let view_state = source.take_view_snapshot();
        let mut contents = Contents::default();
        contents.clone_from(&source.contents);

        Self {
            id,
            path: source.path.clone(),
            settings: source.settings,
            dir_state: source.dir_state.clone(),
            contents,
            view_state,
            history: source.history.clone(),
            future: source.future.clone(),
            element,
            pane: None,
            search: None,
        }
    }

    fn copy_from_donor(&mut self, left_tabs: &[Self], right_tabs: &[Self]) -> bool {
        for t in left_tabs.iter().chain(right_tabs) {
            // Check for value equality, not reference equality
            if *self.path == *t.path {
                self.path = t.path.clone();
                self.dir_state = t.dir_state.clone();
                self.contents.clone_from(&t.contents);

                let comparator = self.settings.sort.comparator();
                self.contents.list.sort(comparator);
                return true;
            }
        }
        false
    }

    // Only make this take &mut [Self] if truly necessary
    fn load(&mut self, left_tabs: &[Self], right_tabs: &[Self]) {
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
        for t in self.matching(left_tabs).chain(self.matching(right_tabs)) {
            //t.show_spinner();
        }
    }

    fn matches_snapshot(&self, snap: &SnapshotId) -> bool {
        if !Arc::ptr_eq(&self.path, &snap.path) {
            return false;
        }

        use SnapshotKind::*;

        let sb = self.dir_state.borrow();

        match (&snap.kind, &*sb) {
            (_, DirState::Unloaded) => {
                // This should never happen. Maybe unloading needs to forcibly change the Arc out
                // for a new one.
                unreachable!("Received {:?} snapshot for an unloaded tab", snap.kind);
            }
            (_, DirState::Loaded(_)) => {
                // This should never happen while loading/reloading.
                unreachable!("Received {:?} snapshot for loaded tab.", snap.kind);
            }
            (Complete | Start | Middle | End, DirState::Loading { .. }) => true,
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

        if self.settings.display_mode == DisplayMode::Icons {
            if let Some(pane) = &self.pane {
                if pane.workaround_scroller().vscrollbar_policy() != gtk::PolicyType::Never {
                    error!("Locking scrolling to work around gtk crash");
                    pane.workaround_scroller().set_vscrollbar_policy(gtk::PolicyType::Never);
                }
            }
        }

        self.contents.list.extend(snap.entries.into_iter());
        let start = Instant::now();
        self.contents.list.sort(self.settings.sort.comparator());
        self.search_extend();
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
    }

    // Returns true if we had any search updates. Expected to be rare, so that path isn't
    // terribly efficient.
    fn apply_snapshot(
        &mut self,
        left_tabs: &mut [Self],
        right_tabs: &mut [Self],
        snap: DirSnapshot,
    ) {
        assert!(self.matches_snapshot(&snap.id));
        let snap: EntryObjectSnapshot = snap.into();
        let force_search_sort = snap.had_search_updates;

        // The actual tab order doesn't matter here, so long as it's applied to every matching tab.
        // Avoid one clone by doing the other tabs first.
        self.matching_mut(right_tabs).for_each(|t| t.apply_obj_snapshot(snap.clone()));

        let snap_kind = snap.id.kind;

        self.apply_obj_snapshot(snap);

        if force_search_sort {
            // We just sorted this tab, and we sorted all matching tabs to the right.
            left_tabs.iter_mut().for_each(Self::search_sort);
            right_tabs
                .iter_mut()
                .filter(|t| !t.matches_arc(&self.path))
                .for_each(Self::search_sort);
        }

        // If we're finished, update all tabs to reflect this.
        if !snap_kind.finished() {
            // Not completed, no changes to dir loading state.
            return;
        }

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
            self.apply_update(left_tabs, right_tabs, u);
        }

        self.start_apply_view_state();
        self.matching_mut(right_tabs).for_each(Self::start_apply_view_state);
        // TODO -- stop showing spinners.
        info!("Finished loading {:?}", self.path);
    }

    fn matches_update(&self, ev: &Update) -> bool {
        // Not watching anything -> cannot match an update
        let Some(watch) = self.dir_state.borrow().watched() else {
            return false;
        };

        Some(&*self.path) == ev.path().parent()
    }

    // Look for the location in the list.
    // The list may no longer be sorted because of an update, but except for the single update to
    // "entry's corresponding EntryObject" the list will be sorted.
    // So using the old version of the entry we can search for the old location.
    fn position_by_sorted_entry(&self, entry: &Entry) -> u32 {
        let mut start = 0;
        let mut end = self.contents.list.n_items();
        assert_ne!(end, 0);

        let comp = self.settings.sort.comparator();

        while start < end {
            let mid = start + (end - start) / 2;

            let obj = self.contents.list.item(mid).unwrap().downcast::<EntryObject>().unwrap();

            let inner = obj.get();
            if inner.abs_path == entry.abs_path {
                // The equality check below may fail even with abs_path being equal due to updates.
                return mid;
            }

            match entry.cmp(&inner, self.settings.sort) {
                Ordering::Equal => unreachable!(),
                Ordering::Less => end = mid,
                Ordering::Greater => start = mid + 1,
            }
        }

        // The old object MUST exist in all relevant liststores, so this should not fail.
        unreachable!()
    }

    fn reinsert_updated(&mut self, sorted: &Entry, new: &EntryObject) {
        let i = self.position_by_sorted_entry(sorted);

        let comp = self.settings.sort.comparator();
        if (i == 0
            || comp(&self.contents.list.item(i - 1).unwrap(), new.upcast_ref::<Object>()).is_lt())
            && (i == self.contents.list.n_items() - 1
                || comp(&self.contents.list.item(i + 1).unwrap(), new.upcast_ref::<Object>())
                    .is_gt())
        {
            debug!("Did not reinsert item as it was already in the correct position");
            return;
        }

        let was_selected = self.contents.selection.is_selected(i);

        // After removing the lone updated item, the list is guaranteed to be sorted.
        // So we can reinsert it much more cheaply than sorting the entire list again.
        self.contents.list.remove(i);
        let new_idx = self.contents.list.insert_sorted(new, comp);

        if was_selected {
            self.contents.selection.select_item(new_idx, false);
        }
    }

    fn finish_update(&mut self, update: &PartiallyAppliedUpdate) {
        match update {
            PartiallyAppliedUpdate::Mutate(old, new) => self.reinsert_updated(old, new),
            PartiallyAppliedUpdate::Insert(new) => {
                let comp = self.settings.sort.comparator();
                self.contents.list.insert_sorted(new, comp);
            }
            PartiallyAppliedUpdate::Delete(old) => {
                let i = self.position_by_sorted_entry(&old.get());
                self.contents.list.remove(i);
            }
        }
    }

    fn apply_update(&mut self, left_tabs: &mut [Self], right_tabs: &mut [Self], up: Update) {
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


        let existing = EntryObject::lookup(up.path());

        let mut search_mutate = None;

        let partial = match (up, existing) {
            (Update::Entry(entry), Some(obj)) => {
                let Some(old) = obj.update(entry) else {
                    // An unimportant update.
                    // No need to check other tabs.
                    return;
                };

                self.reinsert_updated(&old, &obj);

                trace!("Updated {:?} from event", old.abs_path);
                PartiallyAppliedUpdate::Mutate(old, obj)
            }
            (Update::Entry(entry), None) => {
                let (new, old) = EntryObject::create_or_update(entry);
                // This existing means the element existing in a search tab, somewhere, and we've
                // updated it.
                search_mutate = old.map(|old| PartiallyAppliedUpdate::Mutate(old, new.clone()));

                let comp = self.settings.sort.comparator();
                self.contents.list.insert_sorted(&new, comp);

                trace!("Inserted {:?} from event", new.get().abs_path);
                PartiallyAppliedUpdate::Insert(new)
            }
            (Update::Removed(path), Some(obj)) => {
                let i = self.position_by_sorted_entry(&obj.get());
                self.contents.list.remove(i);

                trace!("Removed {:?} from event", path);
                PartiallyAppliedUpdate::Delete(obj)
            }
            (Update::Removed(path), None) => {
                // Unusual case, probably shouldn't happen.
                // Maybe if something is removed while loading the directory.
                warn!("Got removal for {path:?} which wasn't present");
                // It's possible this entry is living in a yet-to-be applies search snapshot, but
                // that's not important right now.
                return;
            }
        };


        for t in self.matching_mut(right_tabs) {
            t.finish_update(&partial);
        }

        let search_update = search_mutate.unwrap_or(partial);

        // Apply to any matching search tabs, even to the left.
        self.finish_search_update(&search_update);
        for t in left_tabs.iter_mut().chain(right_tabs.iter_mut()) {
            t.finish_search_update(&search_update)
        }
    }

    fn search_sort(&mut self) {
        let Some(search) = &self.search else {
            return;
        };
        // if !searching return
        // TODO [search]
    }

    fn search_extend(&mut self) {
        let Some(search) = &self.search else {
            return;
        };
        // TODO [search]
    }

    fn finish_search_update(&mut self, update: &PartiallyAppliedUpdate) {
        let Some(search) = &self.search else {
            return;
        };
        // if !searching return
        error!("TODO -- finish_search_update")
    }

    fn check_directory_deleted(&self, removed: &Path) -> bool {
        removed == &*self.path
    }

    // Need to handle this to avoid the case where a directory is "Loaded" but no watcher is
    // listening for events.
    // Even if the directory is recreated, the watcher won't work.
    fn handle_directory_deleted(&mut self, left_tabs: &[Self], right_tabs: &[Self]) {
        let cur_path = self.path.clone();
        let mut path = &*cur_path;
        while !path.exists() || !path.is_dir() {
            path = path.parent().unwrap()
        }

        self.change_location(left_tabs, right_tabs, path);
    }

    fn update_sort(&mut self, sort: SortSettings) {
        self.settings.sort = sort;
        self.contents.list.sort(self.settings.sort.comparator());
        self.pane.as_mut().unwrap().update_settings(self.settings);
        self.save_settings();
    }

    fn update_display_mode(&mut self, mode: DisplayMode) {
        self.settings.display_mode = mode;
        self.pane.as_mut().unwrap().update_settings(self.settings);
        self.save_settings();
    }

    fn change_location(&mut self, left_tabs: &[Self], right_tabs: &[Self], target: &Path) {
        self.view_state = None;
        self.search = None;

        self.path = target.into();

        if !self.copy_from_donor(left_tabs, right_tabs) {
            // We couldn't find any state to steal, so we know we're the only matching tab.

            self.dir_state.replace(DirState::Unloaded);
            self.settings = GUI.with(|g| g.get().unwrap().database.get(&self.path));
            self.contents.list.remove_all();
        }

        if let Some(pane) = &mut self.pane {
            pane.update_location(&self.path, self.settings);
            self.load(left_tabs, right_tabs);
        }
    }

    fn navigate(&mut self, left_tabs: &[Self], right_tabs: &[Self], target: &Path) {
        info!("Navigating from {:?} to {:?}", self.path, target);

        // TODO -- is an active search part of a history entry?
        //
        // self.take_view_snapshot()
        // Look for another matching tab and steal its state.
        // Only if that fails do we open a new watcher.
        // todo!()
    }

    fn forward(&mut self) {
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

    fn start_apply_view_state(&mut self) {
        let Some(pane) = &self.pane else {
            trace!("Ignoring start_apply_view_state on tab {:?} with no pane", self.id);
            return;
        };

        let id = self.id.copy();
        let finish =
            move || GUI.with(|g| g.get().unwrap().tabs.borrow_mut().finish_apply_view_state(id));

        if self.settings.display_mode != DisplayMode::Icons {
            // Doing this immediately can cause it to be skipped
            glib::idle_add_local_once(finish);
        } else {
            error!("Unsetting GTK crash workaround");
            pane.workaround_scroller().set_vscrollbar_policy(gtk::PolicyType::Automatic);
            glib::timeout_add_local_once(Duration::from_millis(300), finish);
        }
    }

    // TODO -- include index or have the TabsList allocate boxes and pass those down.
    pub(super) fn display(&mut self, parent: &gtk::Box) {
        if let Some(pane) = &self.pane {
            trace!("Displaying existing pane");
            pane.display(parent);
        } else {
            let pane = Pane::new(self);
            pane.display(parent);
            self.pane = Some(pane);
        }

        if matches!(&*self.dir_state.borrow(), DirState::Loaded(_)) {
            self.start_apply_view_state();
        }
    }

    fn hide(&mut self) {
        if matches!(&*self.dir_state.borrow(), DirState::Loaded(_)) {
            self.take_view_snapshot();
        }
    }

    pub(super) fn finish_apply_view_state(&mut self) {
        let Some(pane) = &mut self.pane else {
            warn!("Pane was closed after we asked to apply view state");
            return;
        };
        let view_state = self.view_state.take().unwrap_or_default();

        pane.apply_view_state(view_state);
    }

    fn save_settings(&self) {
        GUI.with(|g| {
            g.get().unwrap().database.store(&self.path, self.settings);
        });
    }
}


mod id {
    use std::cell::Cell;

    thread_local! {
        static NEXT_ID: Cell<u64> = Cell::new(0);
    }

    // A unique identifier for tabs.
    // Options considered:
    //   Incrementing u64:
    //      + Easy implementation
    //      + Fast, no allocations
    //      - Can theoretically overflow
    //      - Uniqueness isn't trivially statically guaranteed
    //      - Linear searching for tabs
    //   Rc<()>:
    //      + Easy implementation
    //      + Rc::ptr_eq is as fast as comparing u64
    //      + Tabs can create their own
    //      + Uniqueness is guaranteed provided tabs always construct their own
    //      - Wasted heap allocations
    //      - Linear searching for tabs
    //  Rc<Cell<index>>:
    //      + No need for linear searching to find tabs
    //      + Rc::ptr_eq is as fast as comparing u64
    //      + Uniqueness is guaranteed
    //      - Most complicated implementation. Must be manually kept up-to-date.
    //      - If the index is ever wrong, weird bugs can happen
    //      - Heap allocation
    //  UUIDs:
    //      - Not really better than a bare u64
    #[derive(Debug, Eq, PartialEq)]
    pub struct TabUid(u64);

    #[derive(Debug, Eq, PartialEq, Clone, Copy)]
    pub struct TabId(u64);

    pub fn next_id() -> TabUid {
        TabUid(NEXT_ID.with(|n| {
            let o = n.get();
            n.set(o + 1);
            o
        }))
    }

    impl TabUid {
        pub const fn copy(&self) -> TabId {
            TabId(self.0)
        }
    }
}
