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
    dir_state: Rc<RefCell<DirState>>,
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
            dir_state: source.dir_state.clone(),
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
                self.dir_state = t.dir_state.clone();
                self.contents = t.contents.clone();

                let comparator = self.settings.sort.comparator();
                self.contents.list.sort(comparator);
                return;
            }
        }
    }

    pub(super) fn load(&mut self, left_tabs: &mut [Self], right_tabs: &mut [Self]) {
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
        for t in left_tabs.iter_mut().chain(right_tabs).filter(|t| t.matches(self)) {
            //t.show_spinner();
        }
    }

    pub(super) fn matches_snapshot(&self, snap: &SnapshotId) -> bool {
        if !Arc::ptr_eq(&self.path, &snap.path) {
            return false;
        }

        use SnapshotKind::*;

        let sb = self.dir_state.borrow();

        match (&snap.kind, &*sb) {
            (_, DirState::Unloaded) => {
                // TODO -- find some decent way of preventing two identical tabs from opening to
                // the same directory at once.
                unreachable!("Received {:?} snapshot for an unloaded tab", snap.kind);
            }
            (_, DirState::Loaded(_)) => {
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
    }

    pub(super) fn apply_snapshot(&mut self, right_tabs: &mut [Self], snap: DirSnapshot) {
        assert!(self.matches_snapshot(&snap.id));
        let start = Instant::now();
        let snap: EntryObjectSnapshot = snap.into();
        println!("Mapped in {:?}", start.elapsed());

        // The actual tab order doesn't matter here, so long as it's applied to every matching tab.
        for t in right_tabs.iter_mut().filter(|t| self.matches(t)) {
            t.apply_obj_snapshot(snap.clone());
        }

        let snap_kind = snap.id.kind;

        self.apply_obj_snapshot(snap);

        if snap_kind.finished() {
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
                self.apply_update(right_tabs, u);
            }
            // TODO -- stop showing spinners.
            info!("Finished loading {:?}", self.path);
        }
    }

    pub(super) fn matches_update(&self, ev: &Update) -> bool {
        // Not watching anything -> cannot match an update
        let Some(watch) = self.dir_state.borrow().watched() else {
            return false;
        };

        Some(&*self.path) == ev.path().parent()
    }

    // Look for the location in the list.
    // The list may no longer be sorted because of an update, but except for the single update to
    // "entry's EntryObject" the list must be sorted.
    // So using the old, pre-update version of entry we can search for the old location.
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

    fn reinsert_updated(&mut self, old: &Entry, new: &EntryObject) {
        let i = self.position_by_sorted_entry(old);

        let was_selected = self.contents.selection.is_selected(i);

        // After removing the lone updated item, the list is guaranteed to be sorted.
        // So we can reinsert it much more cheaply than sorting the entire list again.
        self.contents.list.remove(i);
        let new_idx = self.contents.list.insert_sorted(new, self.settings.sort.comparator());

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

    pub(super) fn apply_update(&mut self, right_tabs: &mut [Self], up: Update) {
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


        let start = Instant::now();
        let existing = EntryObject::lookup(up.path());
        println!("{:?}", start.elapsed());

        let partial = match (up, existing) {
            (Update::Entry(entry), Some(obj)) => {
                let Some(old) = obj.update(entry) else {
                    // An unimportant update.
                    // No need to check other tabs.
                    return;
                };

                println!("{:?}", start.elapsed());
                self.reinsert_updated(&old, &obj);

                println!("{:?}", start.elapsed());
                trace!("Updated {:?} from event", old.abs_path);
                PartiallyAppliedUpdate::Mutate(old, obj)
            }
            (Update::Entry(entry), None) => {
                let new = EntryObject::new(entry);

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
                return;
            }
        };


        for t in right_tabs.iter_mut().filter(|t| self.matches(t)) {
            t.finish_update(&partial);
        }

        // TODO -- The thumbnailer needs to be careful to only hold onto weak refs.
        if let PartiallyAppliedUpdate::Delete(obj) = partial {
            obj.assert_destroyed();
        }
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

        if matches!(&*self.dir_state.borrow(), DirState::Loaded(_)) {
            self.apply_view_snapshot();
        }
    }

    fn hide(&mut self) {
        if matches!(&*self.dir_state.borrow(), DirState::Loaded(_)) {
            self.take_view_snapshot();
        }
    }
}
