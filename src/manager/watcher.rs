use std::collections::btree_map;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use ahash::AHashSet;
use notify::RecursiveMode::NonRecursive;
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, Watcher};
use tokio::select;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{spawn_blocking, spawn_local};
use tokio::time::{Duration, Instant};

use super::read_dir::flat_dir_count;
use super::{Manager, RecurseId};
use crate::closing;
use crate::com::{Entry, GuiAction, SearchUpdate, Update};
use crate::config::{CONFIG, NfsPolling};

// Use our own debouncer as the existing notify debouncers leave a lot to be desired.
// Send the first event ~immediately and then at most one event per period.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(1000);
// Used to optimize the common case where a bunch of notifies arrive basically instantly for one
// file. Especially after creation. Ideally we only need to send one update unless the file is
// actively being written to.
const DEDUPE_DELAY: Duration = Duration::from_millis(3);

// Used so we tick less often and handle events as batches more.
const BATCH_GRACE: Duration = Duration::from_millis(3);


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
pub struct PendingUpdates {
    expiry: Instant,
    state: State,
    sources: Sources,
    // The only two categories we care about are any kind of update or a removal.
    removal: bool,
}

impl Manager {
    pub(super) fn watch_dir(&mut self, path: &Arc<Path>) -> bool {
        let nfs = unsafe {
            let mut statfs: libc::statfs = std::mem::zeroed();
            let cpath = CString::new(path.as_os_str().as_bytes()).unwrap();
            let ret = libc::statfs(cpath.as_ptr(), &mut statfs);
            ret == 0 && statfs.f_type == libc::NFS_SUPER_MAGIC
        };

        if nfs {
            match CONFIG.nfs_polling {
                NfsPolling::Off => {
                    // Polling runs often enough to keep the connection alive, so this is only
                    // necessary when only using inotify.
                    trace!("Starting NFS keepalive for {path:?}");
                    self.nfs_keepalives.insert(path.clone());
                }
                NfsPolling::On => return self.poll_watch(path),
                NfsPolling::Both => {
                    if !self.poll_watch(path) {
                        return false;
                    }
                }
            }
        }

        self.default_watch(path)
    }

    fn default_watch(&mut self, path: &Arc<Path>) -> bool {
        if let Err(e) = self.watcher.watch(path, NonRecursive) {
            // Treat like the directory was removed.
            // The tab only opened to this directory because it was a directory very recently.
            error!("Failed to open directory {path:?}: {e}");
            if let Err(e) =
                self.gui_sender.send(GuiAction::DirectoryOpenError(path.clone(), e.to_string()))
            {
                if !closing::closed() {
                    closing::fatal(format!("Gui channel unexpectedly closed: {e}"));
                }
            }
            false
        } else {
            trace!("Watching {path:?}");
            true
        }
    }

    fn poll_watch(&mut self, path: &Arc<Path>) -> bool {
        if let Err(e) = self.poll_watcher.as_mut().unwrap().watch(path, NonRecursive) {
            // Treat like the directory was removed.
            // The tab only opened to this directory because it was a directory very recently.
            error!("Failed to open directory for polling {path:?}: {e}");
            if let Err(e) =
                self.gui_sender.send(GuiAction::DirectoryOpenError(path.clone(), e.to_string()))
            {
                if !closing::closed() {
                    closing::fatal(format!("Gui channel unexpectedly closed: {e}"));
                }
            }
            false
        } else {
            trace!("Started polling {path:?}");
            true
        }
    }

    pub(super) fn unwatch_dir(&mut self, path: &Path) {
        trace!("Unwatching {path:?}");
        self.nfs_keepalives.remove(path);
        let unpoll = self.poll_watcher.as_mut().map(|w| w.unwatch(path));
        if let (Err(e), Some(Err(_)) | None) = (self.watcher.unwatch(path), unpoll) {
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

    fn send_removal(sender: &UnboundedSender<GuiAction>, path: Arc<Path>, sources: Sources) {
        for search_id in sources.searches {
            let update = Update::Removed(path.clone());
            let s_up = SearchUpdate { search_id, update };

            sender.send(GuiAction::SearchUpdate(s_up)).unwrap_or_else(|e| {
                closing::fatal(e.to_string());
            })
        }

        if sources.flat {
            let update = Update::Removed(path);
            sender.send(GuiAction::Update(update)).unwrap_or_else(|e| {
                closing::fatal(e.to_string());
            });
        }
    }

    pub(super) fn send_update(
        sender: &UnboundedSender<GuiAction>,
        path: Arc<Path>,
        sources: Sources,
    ) {
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
                closing::fatal(e.to_string());
            })
        }

        if sources.flat {
            let p = entry.abs_path.clone();
            sender.send(GuiAction::Update(Update::Entry(entry))).unwrap_or_else(|e| {
                closing::fatal(e.to_string());
            });

            if needs_full_count {
                flat_dir_count(p, sender.clone());
            }
        }
    }

    fn jump_queue(&mut self, path: &Path) {
        // Jump the queue and change it to dedupe mode
        if let Some(PendingUpdates { expiry, state, .. }) = self.recent_mutations.get_mut(path) {
            if *state != State::Deduping {
                *state = State::Deduping;
                *expiry = Instant::now() + DEDUPE_DELAY;
                let next = *expiry + BATCH_GRACE;

                self.next_tick = self.next_tick.map_or(Some(next), |nt| Some(nt.min(next)));
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

        let mut removal = false;

        match event.kind {
            // Access events are worthless
            // Other events are probably meaningless and the important unmount events aren't
            // present.
            // For renames we use the single path events
            Access(_) | Other | Modify(ModifyKind::Name(RenameMode::Both)) => return,
            Create(_) | Modify(ModifyKind::Name(RenameMode::To)) => {
                trace!("Create {:?}", event.paths);
                assert_eq!(event.paths.len(), 1);

                self.jump_queue(&event.paths[0]);
            }
            Remove(_) | Modify(ModifyKind::Name(RenameMode::From)) => {
                trace!("Remove {:?} {:?}", event.kind, event.paths);
                assert_eq!(event.paths.len(), 1);

                removal = true;

                self.jump_queue(&event.paths[0]);
            }
            // Treat Any as a generic Modify
            Modify(_) | Any => {
                if event.paths.len() != 1 {
                    error!(
                        "Received an event of kind {:?} with paths {}. Ignoring.",
                        event.kind,
                        event.paths.len()
                    );
                    return;
                }
            }
        }

        let mut event = event;
        let path: Arc<Path> = event.paths.pop().unwrap().into();

        match self.recent_mutations.entry(path.clone()) {
            btree_map::Entry::Occupied(occupied) => {
                let p = occupied.into_mut();

                // This can turn a deletion into a creation or vice versa, but it doesn't
                // mechanically matter unless the events are interleaved across different batches.
                // Events should be reflected across all listeners.
                p.removal = removal;

                if p.state == State::Deduping {
                    trace!("Deduping event for {path:?} from {source:?}");
                } else {
                    // trace!("Debouncing event for {path:?} from {source:?}");
                }

                match source {
                    Some(search) => {
                        if !p.sources.searches.iter().any(|s| Arc::ptr_eq(&search, s)) {
                            p.sources.searches.push(search);
                        }
                    }
                    None => p.sources.flat = true,
                }

                if p.state == State::Expiring {
                    p.state = State::Debounced;
                    // next_tick must already be set to be in this branch.
                    // It's possible that this expiry time hasn't been cleaned up yet,
                    // but is before next_tick.
                    self.next_tick = Some(self.next_tick.unwrap().min(p.expiry))
                }
            }
            btree_map::Entry::Vacant(vacant) => {
                let expiry = Instant::now() + DEDUPE_DELAY;
                let mut sources = Sources::default();
                match source {
                    Some(search) => sources.searches.push(search),
                    None => sources.flat = true,
                }

                let next = expiry + BATCH_GRACE;

                vacant.insert(PendingUpdates {
                    expiry,
                    state: State::Deduping,
                    sources,
                    removal,
                });

                self.next_tick = self.next_tick.map_or(Some(next), |nt| Some(nt.min(next)));
            }
        }
    }

    pub(super) fn handle_pending_updates(&mut self) {
        let now = Instant::now();
        let mut maybe_tick = now + DEBOUNCE_DURATION;

        self.recent_mutations.retain(|path, p| {
            let kind = if p.removal { "removal" } else { "update" };

            if p.expiry < now {
                match p.state {
                    State::Expiring => {
                        return false;
                    }
                    State::Deduping => {
                        trace!("Sending {kind} for {path:?} and {:?}", p.sources);
                    }
                    State::Debounced => {
                        trace!("Sending debounced {kind} for {path:?} and {:?}", p.sources);
                    }
                }

                p.state = State::Expiring;
                p.expiry += DEBOUNCE_DURATION;

                if p.removal {
                    Self::send_removal(
                        &self.gui_sender,
                        path.clone(),
                        std::mem::take(&mut p.sources),
                    )
                } else {
                    Self::send_update(
                        &self.gui_sender,
                        path.clone(),
                        std::mem::take(&mut p.sources),
                    );
                }
            } else if p.expiry < maybe_tick && p.state != State::Expiring {
                maybe_tick = p.expiry;
            }

            true
        });

        if self.recent_mutations.is_empty() {
            self.next_tick = None;
        } else {
            self.next_tick = Some(maybe_tick + BATCH_GRACE);
        }
    }

    pub(super) fn flush_updates(
        &mut self,
        all_paths: &AHashSet<Arc<Path>>,
        mut unmatched_paths: AHashSet<Arc<Path>>,
    ) -> AHashSet<Arc<Path>> {
        // Grab any pending updates waiting in the channel.
        // A more robust option would be to wait for ~1ms up to ~5ms, but this is always going to
        // be best-effort.
        while let Ok((ev, id)) = self.notify_receiver.try_recv() {
            self.handle_event(ev, id);
        }

        for path in all_paths {
            let Some(p) = self.recent_mutations.get_mut(&**path) else {
                continue;
            };

            let old = unmatched_paths.take(path);

            if p.state == State::Expiring {
                continue;
            }

            let path = old.unwrap_or_else(|| path.clone());

            if p.removal {
                debug!("Flushing removal for {path:?} and {:?}", p.sources);
                Self::send_removal(&self.gui_sender, path, std::mem::take(&mut p.sources));
            } else {
                debug!("Flushing update for {path:?} and {:?}", p.sources);
                Self::send_update(&self.gui_sender, path, std::mem::take(&mut p.sources));
            }
        }

        unmatched_paths
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
                            error!("Path {parent:?} wasn't in search root {search_root:?}");
                            return;
                        }
                    }
                }
            }

            if let Err(e) = sender.send((res, Some(id.clone()))) {
                if !closing::closed() {
                    closing::fatal(format!("Error sending from notify watcher: {e}"));
                }
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
        let mut watch_fut = spawn_blocking(move || {
            let res = watcher.watch(&p, notify::RecursiveMode::Recursive);
            (watcher, res)
        });

        let watcher = select! {
            watcher = &mut watch_fut => watcher,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                let msg = format!(
                    "Search watcher in {path:?} took a long time to initialize. \
                    Updates may be missed");
                error!("{msg}");
                let _ignored = self.gui_sender.send(GuiAction::ConveyWarning(msg));

                let gui_sender = self.gui_sender.clone();
                let sender = self.slow_searches_sender.clone();
                spawn_local(async move {
                    let slow = watch_fut.await;

                    let (watcher, res) = match slow {
                        Ok((watcher, res)) => {
                            (watcher, res)
                        },
                        Err(e) => {
                            let msg = format!("Search watcher error: {e}");
                            error!("{msg}");
                            let _ignored = gui_sender.send(GuiAction::DirectoryError(path, msg));
                            return;
                        },
                    };

                    if let Err(e) = res {
                        let msg = format!("Search watcher error: {e}");
                        error!("{msg}");
                        let _ignored = gui_sender.send(GuiAction::DirectoryError(path, msg));
                        return;
                    }

                    let _ignored = sender.send((cancel, watcher));
                });
                return;
            }
        };

        if let Err(e) = watcher {
            let msg = format!("Search watcher error: {e}");
            error!("{msg}");
            self.send(GuiAction::DirectoryError(path, msg));
            return;
        }

        if let Ok((_watcher, Err(e))) = watcher {
            let msg = format!("Search watcher error: {e}");
            error!("{msg}");
            self.send(GuiAction::DirectoryError(path, msg));
            return;
        }

        self.open_searches.push((cancel, watcher.unwrap().0));
    }
}
