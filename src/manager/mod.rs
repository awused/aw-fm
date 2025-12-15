use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use ahash::AHashSet;
use completion::complete;
use notify::{Config, Event, PollWatcher, RecommendedWatcher};
use tokio::select;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot::Receiver;
use tokio::task::LocalSet;
use tokio::time::{Instant, sleep_until, timeout};

use self::watcher::PendingUpdates;
use crate::com::{CompletionResult, GuiAction, ManagerAction};
use crate::config::{CONFIG, NfsPolling};
use crate::manager::watcher::Sources;
use crate::{closing, spawn_thread};

mod actions;
mod completion;
mod read_dir;
mod watcher;

type RecurseId = Arc<AtomicBool>;

// Manages I/O work on files and directories.
// Compared to the manager in aw-man, this one is much dumber and all of the driving logic lives in
// the gui thread. This thread just manages reading directory contents and feeding data
// to the gui without blocking it.
#[derive(Debug)]
struct Manager {
    gui_sender: UnboundedSender<GuiAction>,

    // If there are pending mutations, we wait to clear and process them.
    // A vector of tuples would usually perform better here.
    recent_mutations: BTreeMap<Arc<Path>, PendingUpdates>,
    next_tick: Option<Instant>,

    watcher: RecommendedWatcher,
    poll_watcher: Option<PollWatcher>,
    nfs_keepalives: BTreeSet<Arc<Path>>,

    open_searches: Vec<(Arc<AtomicBool>, notify::RecommendedWatcher)>,

    slow_searches_sender: UnboundedSender<(Arc<AtomicBool>, notify::RecommendedWatcher)>,
    slow_searches_receiver: UnboundedReceiver<(Arc<AtomicBool>, notify::RecommendedWatcher)>,

    notify_sender: UnboundedSender<(notify::Result<Event>, Option<RecurseId>)>,
    notify_receiver: UnboundedReceiver<(notify::Result<Event>, Option<RecurseId>)>,

    completion: Option<(Receiver<CompletionResult>, Arc<AtomicBool>)>,
}

pub fn run(
    manager_receiver: UnboundedReceiver<ManagerAction>,
    gui_sender: UnboundedSender<GuiAction>,
) -> JoinHandle<()> {
    spawn_thread("manager", move || {
        let _cod = closing::CloseOnDrop::default();
        if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
            let m = Manager::new(gui_sender);
            run_local(m.run(manager_receiver));
        })) {
            closing::fatal(format!("Manager thread panicked unexpectedly: {e:?}"));
        }
        trace!("Exited IO manager thread");
    })
}

#[tokio::main(flavor = "current_thread")]
async fn run_local(f: impl Future<Output = ()>) {
    // Set up a LocalSet so that spawn_local can be used for cleanup tasks.
    let local = LocalSet::new();
    local.run_until(f).await;

    // Unlike with aw-man, tasks being left in the LocalSet aren't an error.
    // Most tasks do not write to any temporary directories at all.

    if let Err(e) = timeout(Duration::from_secs(600), local).await {
        error!("Unable to finishing cleaning up in {e}, something is stuck.");
    }
}

impl Manager {
    fn new(gui_sender: UnboundedSender<GuiAction>) -> Self {
        let (notify_sender, notify_receiver) = tokio::sync::mpsc::unbounded_channel();

        let sender = notify_sender.clone();
        let watcher = notify::recommended_watcher(move |res| {
            if let Err(e) = sender.send((res, None))
                && !closing::closed()
            {
                closing::fatal(format!("Error sending from notify watcher: {e}"));
            }
        })
        .unwrap();

        let poll_watcher = match CONFIG.nfs_polling {
            NfsPolling::Off => None,
            NfsPolling::On | NfsPolling::Both => {
                let sender = notify_sender.clone();
                Some(
                    notify::PollWatcher::new(
                        move |res| {
                            if let Err(e) = sender.send((res, None))
                                && !closing::closed()
                            {
                                closing::fatal(format!("Error sending from poll watcher: {e}"));
                            }
                        },
                        // 30s default polling, but all options are very bad
                        Config::default(),
                    )
                    .unwrap(),
                )
            }
        };

        let (slow_searches_sender, slow_searches_receiver) = tokio::sync::mpsc::unbounded_channel();

        Self {
            gui_sender,

            recent_mutations: BTreeMap::new(),
            next_tick: None,

            watcher,
            poll_watcher,
            nfs_keepalives: BTreeSet::new(),

            open_searches: Vec::new(),
            slow_searches_sender,
            slow_searches_receiver,

            notify_sender,
            notify_receiver,

            completion: None,
        }
    }

    async fn run(mut self, mut receiver: UnboundedReceiver<ManagerAction>) {
        // NFS times out after 5 minutes of idleness, so stat every 4.5 minutes
        const NFS_KEEPALIVE_PERIOD: Duration = Duration::from_secs(60 * 9 / 2);
        let mut next_keepalive = Instant::now() + NFS_KEEPALIVE_PERIOD;

        'main: loop {
            select! {
                biased;
                _ = closing::closed_fut() => break 'main,
                mtg = receiver.recv() => {
                    let Some(ma) = mtg else {
                        closing::fatal("Received nothing from gui thread. This should never happen");
                        break 'main;
                    };
                    self.handle_action(ma).await;
                }
                ev = self.notify_receiver.recv() => {
                    // Manager is holding both ends
                    let (ev, id) = ev.unwrap();
                    self.handle_event(ev, id);
                }
                slow = self.slow_searches_receiver.recv() => {
                    // Manager is holding both ends
                    let (cancel, watcher) = slow.unwrap();
                    if cancel.load(Ordering::Relaxed) {
                        info!("Got slow search watcher for cancelled search");
                        continue;
                    }

                    self.open_searches.push((cancel, watcher));
                }
                completed = async { (&mut self.completion.as_mut().unwrap().0).await }, if self.completion.is_some() => {
                    self.completion = None;

                    if let Ok(completed) = completed {
                        self.send(GuiAction::Completion(completed));
                    }
                }
                _ = async { sleep_until(self.next_tick.unwrap()).await },
                        if self.next_tick.is_some() => {
                    self.handle_pending_updates();
                }
                _ = sleep_until(next_keepalive), if !self.nfs_keepalives.is_empty() => {
                    next_keepalive = Instant::now() + NFS_KEEPALIVE_PERIOD;

                    let keepalives: Vec<_> = self.nfs_keepalives.iter().cloned().collect();

                    // Blocking in case NFS decides to hang, not sure it'll help though
                    tokio::task::spawn_blocking(move || {
                        trace!("Performing {} NFS keepalives", keepalives.len());

                        for d in keepalives {
                            if !d.is_dir() {
                                warn!("{d:?} was not a directory during NFS keepalive")
                            }
                        }
                    });
                }
            };
        }

        closing::close();
    }

    async fn handle_action(&mut self, ma: ManagerAction) {
        use ManagerAction::*;

        match ma {
            Open(path, sort, cancel) => {
                // This will process any pending removals immediately, but can't handle updates
                // that haven't yet reached this process. Those are rare enough in practice that it
                // is unlikely to be worth fixing.
                // TODO -- this is no longer true, since removals are no longer sent immediately
                self.flush_updates(&AHashSet::new(), AHashSet::new());

                if self.watch_dir(&path) {
                    self.send(GuiAction::Watching(cancel.clone()));
                    self.start_read_dir(path, sort, cancel);
                }
            }
            Unwatch(path) => self.unwatch_dir(&path),

            Search(path, cancel) => {
                self.watch_search(path.clone(), cancel.clone()).await;
                self.recurse_dir(path, cancel);
            }
            EndSearch(cancel) => self.unwatch_search(cancel),

            Execute(s, env) => self.execute(s, env),
            Script(s, target, env) => self.script(s, target, env),
            Launch(s, env) => self.launch(s, env),

            GetChildren(dirs, cancel) => self.get_children(dirs, cancel),

            Flush { all_paths, unmatched_paths, finished } => {
                let remainder = self.flush_updates(&all_paths, unmatched_paths);

                // We, most likely, just wrote these files, so reading them should be very fast and
                // very few of them should not have pending notifications.
                for p in remainder {
                    info!(
                        "Synchronously reading {p:?} for completed operation with no notification."
                    );
                    Self::send_update(&self.gui_sender, p, Sources::new_flat());
                }

                let _ignored = finished.send(all_paths);
            }

            Complete(path, initial, tab) => {
                if let Some((_, cancel)) = self.completion.replace(complete(path, initial, tab)) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }

            CancelCompletion => {
                if let Some((_, cancel)) = self.completion.take() {
                    trace!("Cancelling ongoing completion");
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    fn send(&self, action: GuiAction) {
        if let Err(e) = self.gui_sender.send(action) {
            closing::fatal(format!("Sending to gui thread unexpectedly failed, {e:?}"));
        }
    }
}
