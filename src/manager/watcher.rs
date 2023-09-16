use std::collections::btree_map;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gtk::glib;
use notify::event::{ModifyKind, RenameMode};
use notify::RecursiveMode::NonRecursive;
use notify::{Event, Watcher};
use tokio::task::spawn_blocking;
use tokio::time::{timeout, Duration, Instant};

use super::read_dir::flat_dir_count;
use super::{Manager, RecurseId};
use crate::closing;
use crate::com::{Entry, GuiAction, SearchUpdate, Update};
use crate::config::CONFIG;

// Use our own debouncer as the existing notify debouncers leave a lot to be desired.
// Send the first event ~immediately and then at most one event per period.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(1000);
// Used to optimize the common case where a bunch of notifies arrive basically instantly for one
// file. Especially after creation.
const DEDUPE_DELAY: Duration = Duration::from_millis(5);

// Used so we tick less often and handle events as batches more.
const BATCH_GRACE: Duration = Duration::from_millis(5);

// Nothing -> Deduping
// Deduping -> Expiring
// Expiring -> Debouncing
// Debouncing -> Expiring
// Expiring -> nothing

#[derive(Debug, Eq, PartialEq)]
enum State {
    Deduping,
    Expiring,
    Debounced,
}


#[derive(Debug, Default)]
pub struct Sources {
    flat: bool,
    searches: Vec<RecurseId>,
}

impl Sources {
    pub(super) const fn new_flat() -> Self {
        Self { flat: true, searches: Vec::new() }
    }
}

#[derive(Debug)]
pub struct PendingUpdates(Instant, State, Sources);

impl Manager {
    pub(super) fn watch_dir(&mut self, path: &Arc<Path>) -> bool {
        if let Err(e) = self.watcher.watch(path, NonRecursive) {
            // Treat like the directory was removed.
            // The tab only opened to this directory because it was a directory very recently.
            error!("Failed to open directory {path:?}: {e}");
            if let Err(e) =
                self.gui_sender.send(GuiAction::DirectoryOpenError(path.clone(), e.to_string()))
            {
                if !closing::closed() {
                    error!("Gui channel unexpectedly closed: {e}");
                }
                closing::close();
            }
            false
        } else {
            trace!("Watching {path:?}");
            true
        }
    }

    pub(super) fn unwatch_dir(&mut self, path: &Path) {
        trace!("Unwatching {path:?}");
        if let Err(e) = self.watcher.unwatch(path) {
            warn!("Failed to unwatch {path:?}, removed or never started: {e}");
        }
    }

    pub(super) fn unwatch_search(&mut self, cancel: RecurseId) {
        let pos = self.open_searches.iter().position(|(id, _watcher)| Arc::ptr_eq(&cancel, id));
        if let Some(pos) = pos {
            debug!("Removing recursive search watcher");
            let (_, watcher) = self.open_searches.swap_remove(pos);
            drop(watcher);
        } else if Some(0) != CONFIG.search_max_depth {
            error!("Stopped watching non-existent search. Updates were broken.");
        }
    }

    pub(super) fn send_update(sender: &glib::Sender<GuiAction>, path: Arc<Path>, sources: Sources) {
        // It's probably true that sources.flat means any search updates are wasted.
        // But the flat tab could have been closed.

        let (entry, needs_full_count) = match Entry::new(path) {
            Ok(entry) => entry,
            Err((path, e)) => {
                // For now, don't convey this error.
                return error!("Error handling search file update for {path:?}: {e}");
            }
        };

        let entry = Arc::new(entry);

        for search_id in sources.searches {
            let update = Update::Entry(entry.clone());
            let s_up = SearchUpdate { search_id, update };

            sender.send(GuiAction::SearchUpdate(s_up)).unwrap_or_else(|e| {
                error!("{e}");
                closing::close()
            })
        }

        if sources.flat {
            let p = entry.abs_path.clone();
            sender.send(GuiAction::Update(Update::Entry(entry))).unwrap_or_else(|e| {
                error!("{e}");
                closing::close()
            });

            if needs_full_count {
                flat_dir_count(p, sender.clone());
            }
        }
    }

    pub(super) fn handle_event(&mut self, event: notify::Result<Event>, source: Option<RecurseId>) {
        use notify::EventKind::*;

        let event = match event {
            Ok(ev) => ev,
            Err(e) => {
                let e = format!("Error in notify watcher {e}");
                error!("{e}");
                return self.send(GuiAction::ConveyError(e));
            }
        };

        match event.kind {
            // Access events are worthless
            // Other events are probably meaningless
            // For renames we use the single path events
            Access(_) | Other | Modify(ModifyKind::Name(RenameMode::Both)) => return,
            Create(_) | Modify(ModifyKind::Name(RenameMode::To)) => {
                trace!("Create {:?}", event.paths);
                assert_eq!(event.paths.len(), 1);

                let path = &*event.paths[0];

                // Creations jump the queue and reset it to dedupe mode.
                if let Some(PendingUpdates(expiry, state, _)) = self.recent_mutations.get_mut(path)
                {
                    if *state != State::Deduping {
                        *state = State::Deduping;
                        *expiry = Instant::now() + DEDUPE_DELAY + BATCH_GRACE;

                        self.next_tick =
                            self.next_tick.map_or(Some(*expiry), |nt| Some(nt.min(*expiry)));
                    }
                }
            }
            Remove(_) | Modify(ModifyKind::Name(RenameMode::From)) => {
                trace!("Remove {:?} {:?}", event.kind, event.paths);
                assert_eq!(event.paths.len(), 1);

                let mut event = event;
                let path = event.paths.pop().unwrap();

                self.recent_mutations.remove(&*path);
                let update = Update::Removed(path.into());
                match source {
                    Some(search_id) => {
                        self.send(GuiAction::SearchUpdate(SearchUpdate { search_id, update }))
                    }
                    None => self.send(GuiAction::Update(update)),
                }
                return;
            }
            // Treat Any as a generic Modify
            Modify(_) | Any => {}
        }

        if event.paths.len() != 1 {
            error!(
                "Received an event of kind {:?} with paths {}. Ignoring.",
                event.kind,
                event.paths.len()
            );
            return;
        }

        let mut event = event;
        let path: Arc<Path> = event.paths.pop().unwrap().into();

        match self.recent_mutations.entry(path.clone()) {
            btree_map::Entry::Occupied(occupied) => {
                let PendingUpdates(_expiry, state, sources) = occupied.into_mut();

                if *state == State::Deduping {
                    trace!("Deduping event for {path:?} from {source:?}");
                } else {
                    // trace!("Debouncing event for {path:?} from {source:?}");
                }

                match source {
                    Some(search) => {
                        if !sources.searches.iter().any(|s| Arc::ptr_eq(&search, s)) {
                            sources.searches.push(search);
                        }
                    }
                    None => sources.flat = true,
                }

                if *state == State::Expiring {
                    *state = State::Debounced;
                    // Expiry remains the same, and next_tick should already be set.
                    assert!(self.next_tick.is_some());
                }
            }
            btree_map::Entry::Vacant(vacant) => {
                let expiry = Instant::now() + DEDUPE_DELAY + BATCH_GRACE;
                let mut sources = Sources::default();
                match source {
                    Some(search) => sources.searches.push(search),
                    None => sources.flat = true,
                }

                vacant.insert(PendingUpdates(expiry, State::Deduping, sources));

                self.next_tick = self.next_tick.map_or(Some(expiry), |nt| Some(nt.min(expiry)));
            }
        }
    }

    pub(super) fn handle_pending_updates(&mut self) {
        let now = Instant::now();
        // let starting_len = self.recent_mutations.len();
        let mut maybe_tick = now + DEBOUNCE_DURATION;
        let mut expired_keys = Vec::new();

        for (path, PendingUpdates(expiry, state, sources)) in &mut self.recent_mutations {
            if *expiry < now {
                match state {
                    State::Expiring => {
                        // Wasteful clone, but very short lived
                        expired_keys.push(path.clone());
                        continue;
                    }
                    State::Deduping => {
                        *state = State::Expiring;
                        trace!("Sending update for {path:?} and {sources:?}");
                    }
                    State::Debounced => {
                        *state = State::Expiring;
                        trace!("Sending debounced update for {path:?} and {sources:?}");
                    }
                }

                *expiry += DEBOUNCE_DURATION;
                maybe_tick = maybe_tick.min(*expiry);
                Self::send_update(&self.gui_sender, path.clone(), std::mem::take(sources));
            } else if *expiry < maybe_tick {
                maybe_tick = *expiry;
            }
        }

        for path in expired_keys {
            self.recent_mutations.remove(&path).unwrap();
        }

        if self.recent_mutations.is_empty() {
            self.next_tick = None;
        } else {
            self.next_tick = Some(maybe_tick + BATCH_GRACE);
        }

        // trace!("Processed {starting_len} events in {:?}", now.elapsed());
    }

    pub(super) fn flush_updates(&mut self, mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
        // Grab any pending updates waiting in the channel.
        // A more robust option would be to wait for ~1ms up to ~5ms, but this is always going to
        // be best-effort.
        while let Ok((ev, id)) = self.notify_receiver.try_recv() {
            self.handle_event(ev, id);
        }

        paths.retain_mut(|p| {
            let Some(PendingUpdates(_expiry, state, sources)) = self.recent_mutations.get_mut(&**p)
            else {
                return true;
            };

            *state = State::Expiring;

            debug!("Flushing update for {p:?} and {sources:?}");
            Self::send_update(&self.gui_sender, std::mem::take(p).into(), std::mem::take(sources));
            false
        });

        paths
    }

    pub(super) async fn watch_search(&mut self, path: Arc<Path>, cancel: RecurseId) {
        if Some(0) == CONFIG.search_max_depth {
            return;
        }

        debug!("Watching {path:?} recursively");

        let sender = self.notify_sender.clone();
        let search_root = path.clone();
        let id = cancel.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = &res {
                if ev.paths.is_empty() {
                    return;
                }
                let Some(parent) = ev.paths[0].parent() else {
                    return;
                };

                if &*search_root == parent {
                    // Ignore changes inside the directory itself
                    return;
                }

                if let Some(depth) = CONFIG.search_max_depth {
                    match parent.strip_prefix(&search_root) {
                        Ok(dirs) => {
                            if dirs.components().count() > depth as usize {
                                trace!(
                                    "Ignoring search event in {:?} since it was too deep",
                                    ev.paths[0].parent()
                                );
                                return;
                            }
                        }
                        Err(_e) => {
                            error!("Path {:?} wasn't in search root {:?}", parent, search_root);
                            return;
                        }
                    }
                }
            }

            if let Err(e) = sender.send((res, Some(id.clone()))) {
                if !closing::closed() {
                    error!("Error sending from notify watcher: {e}");
                }
                closing::close();
            }
        });

        let mut watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                let msg = format!("Search watcher error: {e}");
                error!("{msg}");
                self.send(GuiAction::DirectoryError(path, msg));
                return;
            }
        };

        // This can be glacially slow on, say, networked fuse drives.
        // We want it, but we don't want to wait forever for it
        let p = path.clone();
        let watcher = timeout(
            Duration::from_secs(5),
            spawn_blocking(move || {
                let res = watcher.watch(&p, notify::RecursiveMode::Recursive);
                (watcher, res)
            }),
        )
        .await;

        if watcher.is_err() {
            let msg = format!("Search watcher in {path:?} timed out, updates will not be received");
            error!("{msg}");
            self.send(GuiAction::DirectoryError(path, msg));
            return;
        }

        if let Ok(Err(e)) = watcher {
            let msg = format!("Search watcher error: {e}");
            error!("{msg}");
            self.send(GuiAction::DirectoryError(path, msg));
            return;
        }

        if let Ok(Ok((_watcher_, Err(e)))) = watcher {
            let msg = format!("Search watcher error: {e}");
            error!("{msg}");
            self.send(GuiAction::DirectoryError(path, msg));
            return;
        }

        self.open_searches.push((cancel, watcher.unwrap().unwrap().0));
    }
}
