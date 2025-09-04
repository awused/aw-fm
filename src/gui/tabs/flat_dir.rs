use std::cell::{Ref, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use gtk::gio::ListStore;
use gtk::glib::object::ObjectExt;

use super::tab::Tab;
use crate::com::{
    EntryObject, ExistingEntry, GetEntry, ManagerAction, SortSettings, Update,
    liststore_drop_batched, liststore_entry_for_update, liststore_needs_reinsert,
};
use crate::gui::gui_run;
use crate::gui::tabs::PartiallyAppliedUpdate;

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

    // Returns the update if it wasn't saved for later or dropped
    pub fn maybe_drop_or_delay(&self, up: Update) -> Option<Update> {
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
            DirState::Loading { .. } | DirState::Initializating { .. } | DirState::Loaded(_) => {
                false
            }
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

    pub fn try_into_cached(self) -> Option<(Arc<Path>, WatchedDir)> {
        let state = Rc::try_unwrap(self.state).ok()?.into_inner();
        if let DirState::Loaded(watch) = state {
            Some((self.path, watch))
        } else {
            None
        }
    }
}


#[derive(Debug)]
pub(super) struct CachedDir {
    // We only create these from fully loaded directories
    pub dir: Arc<Path>,
    watch: Option<WatchedDir>,
    // This is the sort settings of the last tab to close the directory, matching the sort order of
    // the contents field, which may be different from the sort setting of the directory itself
    // when reopened.
    pub sort: SortSettings,
    pub list: ListStore,
}

impl Drop for CachedDir {
    fn drop(&mut self) {
        if self.watch.is_some() {
            debug!("Closing cached flat directory {:?}", self.dir);
        }

        if self.list.ref_count() == 1 {
            liststore_drop_batched(self.list.clone());
        }
    }
}


impl CachedDir {
    pub fn new(dir: Arc<Path>, watch: WatchedDir, sort: SortSettings, list: ListStore) -> Self {
        debug!("Caching open flat directory {dir:?}");
        Self { dir, watch: Some(watch), sort, list }
    }

    pub fn make_flat_once(&mut self) -> FlatDir {
        debug!("Restoring cached flat directory {:?}", self.dir);
        FlatDir {
            path: self.dir.clone(),
            state: Rc::new(RefCell::new(DirState::Loaded(self.watch.take().unwrap()))),
        }
    }

    // This is simplified from the version on a single Tab since it definitely doesn't exist in
    // other flat tabs. The item may exist in other search tabs.
    pub fn apply_update(&mut self, update: Update, tabs: &mut [Tab]) {
        // Needs to handle silly renames and search updates/inserts.
        // Also handle silly rename deletions
        // Specifically it'd be invalid to update something here and not reflect it in other tabs.
        let existing = liststore_entry_for_update(&self.list, self.sort, &update);

        let search_update = match (update, existing) {
            (Update::Entry(entry), ExistingEntry::Present(obj, pos)) => {
                let Some(old) = obj.update(entry) else {
                    // An unimportant update.
                    // No need to check other tabs.
                    return;
                };

                let t_pos = pos.0;
                if liststore_needs_reinsert(&self.list, self.sort, pos, &obj) {
                    self.list.remove(t_pos);
                    self.list.insert_sorted(&obj, self.sort.comparator());
                }


                trace!("Updated {:?} from event", old.abs_path);
                PartiallyAppliedUpdate::Mutate(old, obj)
            }
            (Update::Entry(entry), ExistingEntry::NotLocal(obj)) => {
                // This means the element already existed in a search tab, somewhere else, and
                // we're updating it.
                let old = obj.update(entry);
                self.list.insert_sorted(&obj, self.sort.comparator());

                trace!("Inserted existing {:?} from event", obj.get().abs_path);

                if let Some(old) = old {
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
                    PartiallyAppliedUpdate::Mutate(old, obj)
                } else {
                    return;
                }
            }
            (Update::Entry(entry), ExistingEntry::Missing) => {
                let new = EntryObject::new(entry.get_entry(), true);
                self.list.insert_sorted(&new, self.sort.comparator());
                trace!("Inserted new {:?} from event", new.get().abs_path);
                return;
            }
            (Update::Removed(path), ExistingEntry::Present(obj, pos)) => {
                self.list.remove(pos.0);
                trace!("Removed {path:?} from event");
                PartiallyAppliedUpdate::Delete(obj)
            }
            (Update::Removed(path), ExistingEntry::NotLocal(obj)) => {
                info!(
                    "Removed search-only {path:?} from event, or got duplicate event. This should \
                     be uncommon."
                );
                PartiallyAppliedUpdate::Delete(obj)
            }
            (Update::Removed(path), ExistingEntry::Missing) => {
                // Unusual case, probably shouldn't happen often.
                // Maybe if something is removed while loading the directory before we read it.
                warn!("Got removal for {path:?} which wasn't present");
                return;
            }
        };

        tabs.iter_mut().for_each(|t| t.handle_search_subdir_flat_update(&search_update));
    }
}
