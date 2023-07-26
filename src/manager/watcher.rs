use std::collections::hash_map;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gtk::glib::{self, Sender};
use notify::event::{ModifyKind, RenameMode};
use notify::RecursiveMode::NonRecursive;
use notify::{Event, Watcher};
use tokio::time::{Duration, Instant};

use super::Manager;
use crate::closing;
use crate::com::{Entry, GuiAction, SearchUpdate, Update};

// Use our own debouncer as the existing notify debouncers leave a lot to be desired.
// Send the first event immediately and then at most one event per period.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);
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

type Search = Arc<AtomicBool>;

#[derive(Debug, Default)]
pub struct Sources {
    flat: bool,
    searches: Vec<Search>,
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
            true
        }
    }

    pub(super) fn unwatch_dir(&mut self, path: &Path) {
        trace!("Unwatching {path:?}");
        if let Err(e) = self.watcher.unwatch(path) {
            warn!("Failed to unwatch {path:?}, removed or never started: {e}");
        }
    }

    pub(super) fn watch_search(&mut self, path: Arc<Path>, cancel: Arc<AtomicBool>) {
        error!("TODO -- search");
    }

    pub(super) fn unwatch_search(&mut self, cancel: Arc<AtomicBool>) {
        let pos = self.open_searches.iter().position(|(id, _watcher)| Arc::ptr_eq(&cancel, id));
        if let Some(pos) = pos {
            debug!("Removing search watcher");
            let (_, watcher) = self.open_searches.swap_remove(pos);
            drop(watcher);
        }
    }

    fn send_update(sender: &glib::Sender<GuiAction>, path: Arc<Path>, sources: Sources) {
        for search in sources.searches {
            match Entry::new(path.clone()) {
                Ok(entry) => {
                    sender.send(GuiAction::Update(Update::Entry(entry))).unwrap_or_else(|e| {
                        error!("{e}");
                        closing::close()
                    })
                }
                Err((path, e)) => {
                    error!("Error handling search file update for {path:?}: {e}");
                    // For now, don't convey this error.
                }
            }
        }

        if sources.flat {
            match Entry::new(path) {
                Ok(entry) => {
                    sender.send(GuiAction::Update(Update::Entry(entry))).unwrap_or_else(|e| {
                        error!("{e}");
                        closing::close()
                    })
                }
                Err((path, e)) => {
                    error!("Error handling file update {path:?}, assuming it was removed: {e}");
                    // For now, don't convey this error.
                }
            }
        }
    }

    pub(super) fn handle_event(&mut self, event: notify::Result<Event>, source: Option<Search>) {
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
            // Creations jump the queue and reset it to dedupe mode.
            Create(_) | Modify(ModifyKind::Name(RenameMode::To)) => {
                trace!("Create {:?}", event.paths);
                assert_eq!(event.paths.len(), 1);
                self.recent_mutations.remove(&*event.paths[0]);
            }
            Remove(_) | Modify(ModifyKind::Name(RenameMode::From)) => {
                trace!("Remove {:?}", event.paths);
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
            hash_map::Entry::Occupied(occupied) => {
                let PendingUpdates(_expiry, state, sources) = occupied.into_mut();

                if *state == State::Deduping {
                    trace!("Deduping event for {path:?} from {source:?}");
                } else {
                    trace!("Debouncing event for {path:?} from {source:?}");
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
            hash_map::Entry::Vacant(vacant) => {
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
        let mut maybe_tick = now + DEBOUNCE_DURATION;
        let mut expired_keys = Vec::new();

        // TODO -- this would match drain_filter
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
    }
}
