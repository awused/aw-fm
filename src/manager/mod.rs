use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ahash::AHashMap;
use gtk::glib;
use notify::{Event, RecommendedWatcher};
use tokio::select;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::LocalSet;
use tokio::time::{sleep_until, timeout, Instant};

use self::watcher::PendingUpdates;
use crate::com::{GuiAction, ManagerAction};
use crate::{closing, spawn_thread};

mod actions;
mod read_dir;
mod watcher;


// Manages I/O work on files and directories.
// Compared to the manager in aw-man, this one is much dumber and all of the driving logic lives in
// the gui thread. This thread just manages reading directory contents and sqlite and feeding data
// to the gui without blocking it.
#[derive(Debug)]
struct Manager {
    gui_sender: glib::Sender<GuiAction>,

    // If there are pending mutations, we wait to clear and process them.
    // If the boolean is true, there was a second event we debounced.
    recent_mutations: BTreeMap<Arc<Path>, PendingUpdates>,
    next_tick: Option<Instant>,

    watcher: RecommendedWatcher,

    open_searches: Vec<(Arc<AtomicBool>, notify::RecommendedWatcher)>,

    notify_receiver: UnboundedReceiver<notify::Result<Event>>,
}

pub fn run(
    manager_receiver: UnboundedReceiver<ManagerAction>,
    gui_sender: glib::Sender<GuiAction>,
) -> JoinHandle<()> {
    spawn_thread("manager", move || {
        let _cod = closing::CloseOnDrop::default();
        let m = Manager::new(gui_sender);
        run_local(m.run(manager_receiver));
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
    fn new(gui_sender: glib::Sender<GuiAction>) -> Self {
        let (sender, notify_receiver) = tokio::sync::mpsc::unbounded_channel();

        let watcher = notify::recommended_watcher(move |res| {
            if let Err(e) = sender.send(res) {
                if !closing::closed() {
                    error!("Error sending from notify watcher: {e}");
                }
                closing::close();
            }
        })
        .unwrap();

        Self {
            gui_sender,

            recent_mutations: BTreeMap::new(),
            next_tick: None,

            watcher,
            open_searches: Vec::new(),

            notify_receiver,
        }
    }

    // async fn run(mut self, mut receiver: UnboundedReceiver<ManagerAction>) -> TempDir {
    async fn run(mut self, mut receiver: UnboundedReceiver<ManagerAction>) {
        'main: loop {
            select! {
                biased;
                _ = closing::closed_fut() => break 'main,
                mtg = receiver.recv() => {
                    let Some(ma) = mtg else {
                        error!("Received nothing from gui thread. This should never happen");
                        closing::close();
                        break 'main;
                    };
                    self.handle_action(ma);
                }
                ev = self.notify_receiver.recv() => {
                    let Some(ev) = ev else {
                        error!("Received nothing from notify watcher. This should never happen");
                        closing::close();
                        break 'main;
                    };
                    self.handle_event(ev, None);
                }
                _ = async { sleep_until(self.next_tick.unwrap()).await },
                        if self.next_tick.is_some() => {
                    self.handle_pending_updates();
                }
            };
        }

        closing::close();
        // if let Err(e) = timeout(Duration::from_secs(600), self.join()).await {
        //     error!("Failed to exit cleanly in {e}, something is probably stuck.");
        // }
    }

    fn handle_action(&mut self, ma: ManagerAction) {
        use ManagerAction::*;

        match ma {
            Open(path, cancel) => {
                if self.watch_dir(&path) {
                    self.start_read_dir(path, cancel);
                }
            }
            Refresh(path, cancel) => self.start_read_dir(path, cancel),
            Unwatch(path) => self.unwatch_dir(&path),

            Search(path, cancel) => {
                self.watch_search(path.clone(), cancel.clone());
                self.recurse_dir(path, cancel);
            }
            EndSearch(cancel) => self.unwatch_search(cancel),

            Execute(s, env) => self.execute(s, env),
            Script(s, env) => self.script(s, env),
        }
    }

    fn send(&self, action: GuiAction) {
        if let Err(e) = self.gui_sender.send(action) {
            error!("Sending to gui thread unexpectedly failed, {:?}", e);
            closing::close();
        }
    }
}
