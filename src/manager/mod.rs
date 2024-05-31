use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use completion::complete;
use notify::{Event, RecommendedWatcher};
use tokio::select;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot::Receiver;
use tokio::task::LocalSet;
use tokio::time::{sleep_until, timeout, Instant};

use self::watcher::PendingUpdates;
use crate::com::{CompletionResult, GuiAction, ManagerAction};
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
    // If the boolean is true, there was a second event we debounced.
    recent_mutations: BTreeMap<Arc<Path>, PendingUpdates>,
    next_tick: Option<Instant>,

    watcher: RecommendedWatcher,

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
            if let Err(e) = sender.send((res, None)) {
                if !closing::closed() {
                    closing::fatal(format!("Error sending from notify watcher: {e}"));
                }
            }
        })
        .unwrap();

        let (slow_searches_sender, slow_searches_receiver) = tokio::sync::mpsc::unbounded_channel();

        Self {
            gui_sender,

            recent_mutations: BTreeMap::new(),
            next_tick: None,

            watcher,
            open_searches: Vec::new(),
            slow_searches_sender,
            slow_searches_receiver,

            notify_sender,
            notify_receiver,

            completion: None,
        }
    }

    async fn run(mut self, mut receiver: UnboundedReceiver<ManagerAction>) {
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
                        println!("TODO -- handle successful completion {completed:?}");
                    }
                }
                _ = async { sleep_until(self.next_tick.unwrap()).await },
                        if self.next_tick.is_some() => {
                    self.handle_pending_updates();
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
                self.flush_updates(Vec::new());

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

            Flush(paths, resp) => {
                let remainder = self.flush_updates(paths);

                // We, most likely, just wrote these files, so reading them should be very fast and
                // very few of them should not have pending notifications.
                for p in remainder {
                    info!(
                        "Synchronously reading {p:?} for completed operation with no notification."
                    );
                    Self::send_update(&self.gui_sender, p.into(), Sources::new_flat());
                }

                let _ignored = resp.send(());
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
