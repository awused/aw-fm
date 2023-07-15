use std::collections::hash_map;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::event::{ModifyKind, RenameMode};
use notify::RecursiveMode::NonRecursive;
use notify::{Event, EventKind, Watcher};
use tokio::time::{Duration, Instant};

use super::Manager;
use crate::com::{Entry, GuiAction, Update};

const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);


impl Manager {
    pub(super) fn watch_dir(&mut self, path: &Path) {
        if let Err(e) = self.watcher.watch(path, NonRecursive) {
            // Handle as a directory error
            todo!()
        }
    }

    pub(super) fn unwatch_dir(&mut self, path: &Path) {
        if let Err(e) = self.watcher.unwatch(path) {
            // ?????
            todo!()
        }
    }

    pub(super) fn send_update(&mut self, path: PathBuf) {
        match Entry::new(path) {
            Ok(entry) => Self::send_gui(&self.gui_sender, GuiAction::Update(Update::Entry(entry))),
            Err((path, e)) => {
                error!("Error handling file update {path:?}: {e:?}");
                // todo!();
            }
        }
    }

    pub(super) fn handle_event(&mut self, event: Event) {
        use notify::EventKind::*;
        let now = Instant::now();

        match event.kind {
            // Access events are worthless
            // Other events are probably meaningless
            // For renames we use the single path events
            Access(_) | Other | Modify(ModifyKind::Name(RenameMode::Both)) => return,
            Create(_) | Modify(ModifyKind::Name(RenameMode::To)) => {
                trace!("Create {:?}", event.paths);
                assert_eq!(event.paths.len(), 1);

                self.recent_mutations.remove(&event.paths[0]);
            }
            Remove(_) | Modify(ModifyKind::Name(RenameMode::From)) => {
                trace!("Remove {:?}", event.paths);
                assert_eq!(event.paths.len(), 1);

                let mut event = event;
                let path = event.paths.pop().unwrap();

                self.recent_mutations.remove(&path);
                Self::send_gui(&self.gui_sender, GuiAction::Update(Update::Removed(path)));
                return;
            }
            // Treat Any as a generic Modify
            Modify(_) | Any => {
                trace!("Modification {:?}", event.kind);
            }
        }

        if event.paths.len() != 1 {
            error!(
                "Received an event of kind {:?} with {}. Ignoring.",
                event.kind,
                event.paths.len()
            );
            return;
        }
        let mut event = event;
        let path = event.paths.pop().unwrap();

        match self.recent_mutations.entry(path.clone()) {
            hash_map::Entry::Occupied(occupied) => {
                let (_, pending) = occupied.into_mut();
                trace!("Debouncing event for {path:?}");
                *pending = true;
                return;
            }
            hash_map::Entry::Vacant(vacant) => {
                let expiry = Instant::now() + DEBOUNCE_DURATION;
                vacant.insert((expiry, false));
                if self.next_tick.is_none() {
                    // Add 10ms just so we can batch things more efficiently
                    self.next_tick = Some(expiry + Duration::from_millis(10));
                }
            }
        }

        self.send_update(path)
    }

    pub(super) fn handle_pending_updates(&mut self) {
        let now = Instant::now();
        let mut next_tick = now + DEBOUNCE_DURATION;
        let mut keys = Vec::new();

        // TODO -- this would match drain_filter
        for (k, (expiry, _pending)) in &self.recent_mutations {
            if expiry < &now {
                // Wasteful clone, but very short lived
                keys.push(k.clone());
            } else if expiry < &next_tick {
                next_tick = *expiry;
            }
        }

        for path in keys {
            let (_, pending) = self.recent_mutations.remove(&path).unwrap();
            if pending {
                trace!("Sending debounced update for {path:?}");
                self.send_update(path)
            }
        }


        if self.recent_mutations.is_empty() {
            self.next_tick = None;
        } else {
            self.next_tick = Some(next_tick + Duration::from_millis(10));
        }
    }
}
