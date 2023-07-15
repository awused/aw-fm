use std::cell::RefCell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ahash::{AHashMap, AHashSet};
use gtk::glib;
use notify::{Event, RecommendedWatcher};
use tempfile::TempDir;
use tokio::select;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::LocalSet;
use tokio::time::{sleep_until, timeout, Instant};

use crate::com::{CommandResponder, GuiAction, GuiActionContext, MAWithResponse, ManagerAction};
use crate::config::{CONFIG, OPTIONS};
use crate::{closing, spawn_thread};

mod actions;
mod monitor;
mod read_dir;
mod watcher;

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
enum ManagerWork {
    Current,
    Finalize,
    Downscale,
    Load,
    Upscale,
    Scan,
}


// Manages I/O work on files and directories.
// Compared to the manager in aw-man, this one is much dumber and all of the driving logic lives in
// the gui thread. This thread just manages reading directory contents and sqlite and feeding data
// to the gui without blocking it.
#[derive(Debug)]
struct Manager {
    temp_dir: TempDir,
    gui_sender: glib::Sender<GuiAction>,

    action_context: GuiActionContext,

    // If there are pending mutations, we wait to clear and process them.
    // If the boolean is true, there was a second event we debounced.
    recent_mutations: AHashMap<PathBuf, (Instant, bool)>,
    next_tick: Option<Instant>,

    watcher: RecommendedWatcher,
    notify_receiver: UnboundedReceiver<Event>,
}

pub fn run(
    manager_receiver: UnboundedReceiver<MAWithResponse>,
    gui_sender: glib::Sender<GuiAction>,
) -> JoinHandle<()> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("aw-fm");
    let tmp_dir = CONFIG
        .temp_directory
        .as_ref()
        .map_or_else(|| builder.tempdir(), |d| builder.tempdir_in(d))
        .expect("Error creating temporary directory");

    spawn_thread("manager", move || {
        let _cod = closing::CloseOnDrop::default();
        let m = Manager::new(gui_sender, tmp_dir);
        run_local(m.run(manager_receiver));
        trace!("Exited IO manager thread");
    })
}

#[tokio::main(flavor = "current_thread")]
async fn run_local(f: impl Future<Output = TempDir>) {
    // Set up a LocalSet so that spawn_local can be used for cleanup tasks.
    let local = LocalSet::new();
    let tdir = local.run_until(f).await;

    // Unlike with aw-man, tasks being left in the LocalSet aren't an error.
    // Most tasks do not write to any temporary directories at all.

    if let Err(e) = timeout(Duration::from_secs(600), local).await {
        error!("Unable to finishing cleaning up in {e}, something is stuck.");
    }

    // By now, all archive joins, even those spawned in separate tasks, are done.
    tdir.close()
        .unwrap_or_else(|e| error!("Error dropping manager temp dir: {:?}", e));
}

impl Manager {
    fn new(gui_sender: glib::Sender<GuiAction>, temp_dir: TempDir) -> Self {
        let (sender, notify_receiver) = tokio::sync::mpsc::unbounded_channel();

        let watcher = notify::recommended_watcher(move |res| {
            let event = match res {
                Ok(event) => event,
                Err(e) => todo!(),
            };

            if let Err(e) = sender.send(event) {
                if !closing::closed() {
                    error!("Error sending from notify watcher: {e}");
                }
                closing::close();
            }
        })
        .unwrap();

        Self {
            temp_dir,
            gui_sender,

            action_context: GuiActionContext::default(),

            recent_mutations: AHashMap::new(),
            next_tick: None,

            watcher,
            notify_receiver,
        }
    }

    async fn run(mut self, mut receiver: UnboundedReceiver<MAWithResponse>) -> TempDir {
        // let path: PathBuf = "/storage/usr/desuwa/Hentai/Images".into();
        // let path: PathBuf = "/storage/cache/fm-test".into();
        // self.start_read_dir(Arc::from(path));
        // self.start_read_dir("/storage/media/youtube/vtubers".into());
        // self.start_read_dir("/home/desuwa".into());
        // let mut watcher = notify::poll::PollWatcher::new(
        //     |res| match res {
        //         Ok(event) => println!("event: {event:?}"),
        //         Err(e) => println!("watch error: {e:?}"),
        //     },
        //     notify::Config::default().with_poll_interval(Duration::from_secs(5)),
        // )
        // .unwrap();
        // Add a path to be watched. All files and directories at that path and
        // below will be monitored for changes.
        // let start = Instant::now();
        // self.watcher
        //     // .watch(Path::new("/storage/usr/desuwa/Hentai/Images"),
        // RecursiveMode::NonRecursive)     .watch(Path::new("/storage/cache/fm-test"),
        // RecursiveMode::NonRecursive)     .unwrap();
        // println!("watch {:?}", start.elapsed());
        //
        // self.watcher
        //     // .watch(Path::new("/storage/usr/desuwa/Hentai/Images"),
        // RecursiveMode::NonRecursive)     .watch(Path::new("/storage/cache/fm-test"),
        // RecursiveMode::NonRecursive)     .unwrap();
        // self.watcher
        //     // .watch(Path::new("/storage/usr/desuwa/Hentai/Images"),
        // RecursiveMode::NonRecursive)     .watch(Path::new("/storage/cache/fm-test"),
        // RecursiveMode::NonRecursive)     .unwrap();
        'main: loop {
            // self.find_next_work();

            // let current_work = !delay_downscale && self.has_work(Current);
            // let final_work = !delay_downscale && self.has_work(Finalize);
            // let downscale_work = !delay_downscale && self.has_work(Downscale);
            // let load_work = self.has_work(Load);
            // let upscale_work = self.has_work(Upscale);
            // let scan_work = self.has_work(Scan);

            let no_work = true;
            // !(current_work
            //     || final_work
            //     || downscale_work
            //     || load_work
            //     || upscale_work
            //     || scan_work
            //     || delay_downscale);

            let mut idle = false;

            'idle: loop {
                select! {
                    biased;
                    _ = closing::closed_fut() => break 'main,
                    mtg = receiver.recv() => {
                        let Some((mtg, context, r)) = mtg else {
                            error!("Received nothing from gui thread. This should never happen");
                            closing::close();
                            break 'main;
                        };
                        self.action_context = context;
                        self.handle_action(mtg, r);
                    }
                    ev = self.notify_receiver.recv() => {
                        let Some(ev) = ev else {
                            error!("Received nothing from notify watcher. This should never happen");
                            closing::close();
                            break 'main;
                        };
                        self.handle_event(ev);
                    }
                    _ = async { sleep_until(self.next_tick.unwrap()).await },
                            if self.next_tick.is_some() => {
                        self.handle_pending_updates();
                    }
                    // _ = self.do_work(Current, true), if current_work => {},
                    // comp = self.do_work(Finalize, current_work), if final_work =>
                    //     self.handle_completion(comp, self.finalize.clone().unwrap()),
                    // comp = self.do_work(Downscale, current_work), if downscale_work =>
                    //     self.handle_completion(comp, self.downscale.clone().unwrap()),
                    // comp = self.do_work(Load, current_work), if load_work =>
                    //     self.handle_completion(comp, self.load.clone().unwrap()),
                    // comp = self.do_work(Upscale, current_work), if upscale_work =>
                    //     self.handle_completion(comp, self.upscale.clone().unwrap()),
                    // _ = self.do_work(Scan, current_work), if scan_work => {},
                    // _ = self.downscale_delay.wait_delay(), if delay_downscale => {
                    //     self.downscale_delay.clear();
                    // },
                    // TODO -- move this to the GUI thread and have it drive unloading stuff
                    _ = idle_sleep(), if no_work && !idle && CONFIG.idle_timeout.is_some() => {
                        idle = true;
                        debug!("Entering idle mode.");
                        self.idle_unload();
                        continue 'idle;
                    }
                };

                if idle {
                    error!("todo -- idle end")
                }

                break 'idle;
            }
        }

        closing::close();
        if let Err(e) = timeout(Duration::from_secs(600), self.join()).await {
            error!("Failed to exit cleanly in {e}, something is probably stuck.");
        }
        self.temp_dir
    }

    fn handle_action(&mut self, ma: ManagerAction, resp: Option<CommandResponder>) {
        use ManagerAction::*;

        match ma {
            Open(path) => {
                self.watch_dir(&path);
                self.start_read_dir(path);
            }
            Close(path) => self.unwatch_dir(&path),
        }
        // Execute(s, env) => self.execute(s, env, resp),
    }

    fn send_gui(gui_sender: &glib::Sender<GuiAction>, action: GuiAction) {
        if let Err(e) = gui_sender.send(action) {
            error!("Sending to gui thread unexpectedly failed, {:?}", e);
            closing::close();
        }
    }

    async fn join(&mut self) {
        error!("TODO join")
    }

    fn idle_unload(&self) {
        error!("TODO -- idle unload");

        // Self::send_gui(&self.gui_sender, GuiAction::IdleUnload);
    }
}

async fn idle_sleep() {
    tokio::time::sleep(Duration::from_secs(CONFIG.idle_timeout.unwrap().get())).await
}
