use std::collections::hash_map;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gtk::glib::Sender;
use notify::event::{ModifyKind, RenameMode};
use notify::RecursiveMode::NonRecursive;
use notify::{Event, EventKind, Watcher};
use tokio::time::{Duration, Instant};

use super::Manager;
use crate::com::{Entry, GuiAction, Update};

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

#[derive(Debug)]
enum State {
    Deduping,
    Expiring,
    Debounced,
}

#[derive(Debug)]
pub struct PendingUpdate(Instant, State);

impl Manager {
    pub(super) fn watch_dir(&mut self, path: &Path) {
        if let Err(e) = self.watcher.watch(path, NonRecursive) {
            // Treat like the directory was removed.
            // The tab only opened to this directory because it was a directory very recently.
            todo!()
        }
    }

    pub(super) fn unwatch_dir(&mut self, path: &Path) {
        if let Err(e) = self.watcher.unwatch(path) {
            // ?????
            todo!()
        }
    }

    fn send_update(gui_sender: &Sender<GuiAction>, path: Arc<Path>) {
        match Entry::new(path) {
            Ok(entry) => Self::send_gui(gui_sender, GuiAction::Update(Update::Entry(entry))),
            Err((path, e)) => {
                error!("Error handling file update {path:?}: {e:?}");
                // todo!();
            }
        }
    }

    pub(super) fn handle_event(&mut self, event: Event) {
        use notify::EventKind::*;

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
                Self::send_gui(&self.gui_sender, GuiAction::Update(Update::Removed(path.into())));
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

        let now = Instant::now();
        let mut event = event;
        let path: Arc<Path> = event.paths.pop().unwrap().into();

        match self.recent_mutations.entry(path.clone()) {
            hash_map::Entry::Occupied(occupied) => {
                let PendingUpdate(expiry, state) = occupied.into_mut();
                match state {
                    State::Deduping => {
                        trace!("Deduping event for {path:?}");
                        return;
                    }
                    State::Expiring => {
                        *state = State::Debounced;
                        // Expiry remains the same, and next_tick should already be set.
                        assert!(self.next_tick.is_some());
                    }
                    State::Debounced => {}
                }
                trace!("Debouncing event for {path:?}");
            }
            hash_map::Entry::Vacant(vacant) => {
                let expiry = Instant::now() + DEDUPE_DELAY + BATCH_GRACE;
                vacant.insert(PendingUpdate(expiry, State::Deduping));

                self.next_tick = self.next_tick.map_or(Some(expiry), |nt| Some(nt.min(expiry)));
            }
        }
    }

    pub(super) fn handle_pending_updates(&mut self) {
        let now = Instant::now();
        let mut maybe_tick = now + DEBOUNCE_DURATION;
        let mut expired_keys = Vec::new();

        // TODO -- this would match drain_filter
        for (path, PendingUpdate(expiry, state)) in &mut self.recent_mutations {
            if *expiry < now {
                match state {
                    State::Expiring => {
                        // Wasteful clone, but very short lived
                        expired_keys.push(path.clone());
                        continue;
                    }
                    State::Deduping => {
                        *state = State::Expiring;
                        trace!("Sending update for {path:?}");
                    }
                    State::Debounced => {
                        *state = State::Expiring;
                        trace!("Sending debounced update for {path:?}");
                    }
                }

                *expiry += DEBOUNCE_DURATION;
                maybe_tick = maybe_tick.min(*expiry);
                Self::send_update(&self.gui_sender, path.clone());
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
