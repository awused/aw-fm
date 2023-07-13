use std::borrow::Borrow;
use std::cell::{Cell, OnceCell, Ref, RefCell};
use std::collections::VecDeque;
use std::env::current_dir;
use std::ffi::OsString;
use std::fmt;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut, Index, IndexMut};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::gio::ListStore;
use gtk::glib::Object;
use gtk::prelude::{Cast, ListModelExt, StaticType};
use gtk::traits::{AdjustmentExt, BoxExt, WidgetExt};
use gtk::{glib, Box, MultiSelection, Orientation, ScrolledWindow};
use notify::{Event, RecursiveMode, Watcher};
use path_clean::PathClean;

use self::pane::Pane;
use super::GUI;
use crate::com::{
    DirSettings, DirSnapshot, EntryObject, FileTime, GuiAction, GuiActionContext, ManagerAction,
    SnapshotKind,
};
use crate::natsort::ParsedString;

mod pane;

#[derive(Debug)]
struct WatchedDir {
    path: Arc<Path>,
}

impl Drop for WatchedDir {
    fn drop(&mut self) {
        GUI.with(|g| {
            let g = g.get().unwrap();
            if let Err(e) = g.watcher.borrow_mut().unwatch(&self.path) {
                let msg = format!("Error unwatching directory {:?}: {e}", self.path);
                error!("{msg}");
                g.convey_error(msg);
            }
        });
    }
}

impl WatchedDir {
    fn start(path: Arc<Path>) -> Rc<Self> {
        GUI.with(|g| {
            let g = g.get().unwrap();
            if let Err(e) = g.watcher.borrow_mut().watch(&path, RecursiveMode::NonRecursive) {
                let msg = format!("Error watching directory {path:?}: {e}");
                error!("{msg}");
                g.convey_error(msg);
            }
        });

        Rc::new(Self { path })
    }
}


#[derive(Debug, Default, Clone)]
enum LoadingState {
    #[default]
    Unloaded,
    Loading {
        watch: Rc<WatchedDir>,
        pending_events: Vec<Event>,
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
}

#[derive(Debug, Clone)]
struct HistoryEntry {
    displayed_path: PathBuf,
    location: Arc<Path>,
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

// A unique identifier for tabs.
// Options considered:
//   Incrementing u64:
//      + Easy implementation
//      + Fast, no allocations
//      - Can theoretically overflow
//      - Uniqueness isn't statically guaranteed
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
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct TabId(u64);

// Current limitations:
// - All tabs open to the same directory share the same watcher
// - All tabs open to the same directory share the same snapshots
// - All tabs open to the same directory are in the same TabState
// - Tabs cannot be unloaded, refreshed, or reopened while they're opening
//   - Tabs can be closed while opening
//
// The net result is going to be higher memory usage than strictly necessary if multiple tabs are
// opened to the same directory and some of them could have been deactivated.
#[derive(Debug)]
pub(super) struct Tab {
    // displayed_path: PathBuf,
    // TODO -- remove pub
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
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.path, &other.path)
    }

    fn new(id: TabId, path: PathBuf, element: (), existing_tabs: &[Self]) -> Self {
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
        let metadata = DirSettings::default();

        let state = LoadingState::Unloaded;

        let contents = Contents::default();

        let mut t = Self {
            id,
            path,
            settings: metadata,
            loading: state,
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

    fn cloned(id: TabId, source: &Self, element: ()) -> Self {
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

                // TODO -- open all the other matching tabs
                let state = LoadingState::Loading { watch, pending_events: Vec::new() };

                // Clone the new state into all matching tabs
                for t in left_tabs.iter_mut().chain(right_tabs).filter(|t| t.matches(self)) {
                    assert!(
                        matches!(t.loading, LoadingState::Unloaded),
                        "Loading a directory with other tabs that were not unloaded"
                    );
                    t.loading = state.clone();
                }

                GUI.with(|g| {
                    g.get().unwrap().send_manager((
                        ManagerAction::Open(self.path.clone()),
                        GuiActionContext::default(),
                        None,
                    ))
                });

                self.loading = state;
            }
        }
    }

    pub(super) fn matches_snapshot(&self, snap: &DirSnapshot) -> bool {
        if !Arc::ptr_eq(&self.path, &snap.path) {
            return false;
        }

        use SnapshotKind::*;

        match (&snap.kind, &self.loading) {
            (_, LoadingState::Unloaded) => {
                // TODO -- find some decent way of preventing two identical tabs from opening to
                // the same directory at once.
                error!(
                    "Received snapshot for an unloaded tab, this could cause weirdness unless \
                     they're all unloaded."
                );
                // This might not be an error if all tabs have been inactivated.
                // panic!("Received snapshot for an inactive tab")
                false
            }
            (Complete | Start, LoadingState::Loading { .. } | LoadingState::Loaded(_))
            | (Middle | End, LoadingState::Loading { .. }) => true,
            (Middle | End, LoadingState::Loaded(_)) => {
                error!("Received {:?} snapshot for opened tab.", snap.kind);
                false
            }
        }
    }

    // TODO -- Not terribly efficient if you have a great many tabs open to the same directory.
    // Can do something clever with Rc<SomeState> and share it between tabs with equivalent
    pub(super) fn apply_snapshot(&mut self, snap: DirSnapshot, active: bool) {
        assert!(self.matches_snapshot(&snap));
        debug!(
            "Applying {:?} snapshot for {:?} with {} items",
            snap.kind,
            self.path,
            snap.entries.len()
        );

        match snap.kind {
            SnapshotKind::Complete => {
                if active {
                    self.save_view_snapshot();
                }
                self.contents.list.remove_all();
            }
            SnapshotKind::Start => {
                match &self.loading {
                    // Tabs must be moved to inactive explicitly
                    LoadingState::Unloaded => unreachable!(),
                    LoadingState::Loading { .. } => {
                        // For now, we don't support opening/refreshing directories while they're
                        // already being opened.
                        assert!(
                            self.contents.list.n_items() == 0,
                            "Received second Initial snapshot for opening directory, a directory \
                             is being opened twice at once"
                        );
                    }
                    LoadingState::Loaded(watch) => {
                        self.loading = LoadingState::Loading {
                            watch: watch.clone(),
                            pending_events: Vec::new(),
                        }
                    }
                }

                if active {
                    self.save_view_snapshot();
                }
                self.contents.list.remove_all();
            }
            SnapshotKind::Middle | SnapshotKind::End => {}
        }

        self.contents.list.extend(snap.entries.into_iter().map(EntryObject::new));
        self.contents.list.sort(self.settings.sort.comparator());

        // let a = self.contents.list.item(0).unwrap();
        // let settings = self.settings;
        // glib::timeout_add_local_once(Duration::from_secs(5), move || {
        //     let b = a.downcast::<EntryObject>().unwrap();
        //     let mut c = b.get().clone();
        //     c.name = ParsedString::from(OsString::from("asdf"));
        //     error!("Updating file for no reason");
        //     b.update(c, settings.sort);
        // });

        match snap.kind {
            SnapshotKind::Complete | SnapshotKind::End => {
                match std::mem::take(&mut self.loading) {
                    LoadingState::Unloaded | LoadingState::Loaded(_) => unreachable!(),
                    LoadingState::Loading { watch, pending_events } => {
                        self.loading = LoadingState::Loaded(watch);
                        // TODO -- apply pending_events
                    }
                }

                if active {
                    self.apply_view_snapshot();
                }
            }
            SnapshotKind::Start | SnapshotKind::Middle => {}
        }
    }

    pub(super) fn matches_event(&self, ev: &Event) -> bool {
        let Some(watch) = self.loading.watched() else {
            return false;
        };

        // match ev {}
        //
        todo!()
    }

    fn navigate(&mut self, left_tabs: &[Self], right_tabs: &[Self]) {
        // Look for another matching tab and steal its state.
        // Only if that fails do we open a new watcher.
        todo!()
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
        let Some(view_state) = &self.view_state else { return };

        GUI.with(|g| {
            // Only called on active tab
            error!("TODO -- apply_view_snapshot");
        });
    }

    // TODO -- include index or have the TabsList allocate boxes and pass those down.
    fn display(&mut self, parent: &gtk::Box) {
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

// TODO -- to support multiple panes, move active out of this
#[derive(Debug)]
pub(super) struct TabsList {
    tabs: Vec<Tab>,
    active: usize,
    next_id: NonZeroU64,
    tabs_container: Box,
    pane_container: Box,
}

impl Deref for TabsList {
    type Target = [Tab];

    fn deref(&self) -> &Self::Target {
        &self.tabs
    }
}

impl DerefMut for TabsList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tabs
    }
}


impl TabsList {
    pub(super) fn new(path: PathBuf) -> Self {
        let tabs_container = Box::new(Orientation::Horizontal, 0);
        tabs_container.set_hexpand(true);

        let first_tab_element = ();

        let pane_container = Box::new(Orientation::Horizontal, 0);
        pane_container.set_hexpand(true);
        pane_container.set_vexpand(true);

        Self {
            tabs: vec![Tab::new(TabId(0), path, first_tab_element, &[])],
            active: 0,
            next_id: NonZeroU64::new(1).unwrap(),
            tabs_container,
            pane_container,
        }
    }

    pub(super) fn initialize(&mut self, parent: &Box) {
        parent.append(&self.tabs_container);
        parent.append(&self.pane_container);
        self.tabs[self.active].load(&mut [], &mut []);
        self.tabs[self.active].display(&self.pane_container);
        // Open and activate first and only tab
    }

    pub(super) fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub(super) const fn active_index(&self) -> usize {
        self.active
    }

    fn split_around_mut(&mut self, index: usize) -> (&mut [Tab], &mut Tab, &mut [Tab]) {
        let (left, rest) = self.split_at_mut(index);
        let ([center], right) = rest.split_at_mut(1) else {
            unreachable!();
        };

        (left, center, right)
    }

    pub(super) fn apply_snapshot(&mut self, snap: DirSnapshot) {
        let first_match = self.iter().position(|t| t.matches_snapshot(&snap));
        if let Some(i) = first_match {
            for (j, t) in &mut self.tabs[i + 1..]
                .iter_mut()
                .enumerate()
                .filter(|(j, t)| t.matches_snapshot(&snap))
            {
                t.apply_snapshot(snap.clone(), i + j + 1 == self.active);
            }
            self.tabs[i].apply_snapshot(snap, i == self.active);
        }
    }

    fn navigate(&mut self) {
        let (left, tab, right) = self.split_around_mut(self.active);
        // Look for another matching tab and steal its state + contents, but not its view_contents.
        // Only if that fails do we open a new watcher.
        //
        // do navigate
        tab.copy_from_donor(left, right);
        todo!()
    }

    fn clone_tab(&mut self, index: usize) {
        let id = TabId(self.next_id.get());
        self.next_id = self.next_id.checked_add(1).unwrap();
        let mut new_tab = Tab::cloned(id, &self.tabs[index], ());

        self.tabs.insert(index + 1, new_tab);

        if index < self.active {
            error!("Cloning inactive tab, this shouldn't happen (yet)");
            self.active += 1
        }
        todo!()
    }

    // For now, tabs always open right of the active tab
    fn open_tab(&mut self, path: PathBuf) {
        let id = TabId(self.next_id.get());
        self.next_id = self.next_id.checked_add(1).unwrap();
        let mut new_tab = Tab::new(id, path, (), &self.tabs);

        self.tabs.insert(self.active + 1, new_tab);
    }

    pub(super) fn close_tab(&mut self, index: usize) {
        // TODO -- If all tabs are active
        if self.tabs.len() == 1 {
            self.open_tab(".".into());
        }

        self.tabs.remove(index);
        let was_active = index == self.active;
        if index <= self.active {
            self.active = self.active.saturating_sub(1);
        }

        if was_active {
            // TODO -- handle case where multiple tabs are active in panes
            // Should grab the next
            let (left, new_active, right) = self.split_around_mut(self.active);
            new_active.load(left, right);
            self.tabs[self.active].display(&self.pane_container);
        }
    }
}
