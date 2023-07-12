#![cfg_attr(not(feature = "windows-console"), windows_subsystem = "windows")]
// TEMPORARY
#![allow(dead_code)]
#![allow(unused)]

#[macro_use]
extern crate log;

// The tikv fork may not be easily buildable for Windows
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::pin::Pin;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gtk::gio::{
    Cancellable, FileQueryInfoFlags, Icon, FILE_ATTRIBUTE_STANDARD_ICON,
    FILE_ATTRIBUTE_STANDARD_IS_SYMLINK, FILE_ATTRIBUTE_STANDARD_SYMBOLIC_ICON,
};
use gtk::glib::GStr;
use gtk::prelude::{FileExt, IconExt};
use gtk::{gio, glib, IconLookupFlags, IconTheme, Settings};
use once_cell::sync::Lazy;
use rayon::prelude::{ParallelBridge, ParallelIterator};

// use self::com::MAWithResponse;

mod elapsedlogger;

mod closing;
mod com;
mod config;
mod database;
mod gui;
mod manager;
mod natsort;
// mod pools;
// mod socket;
// mod state_cache;
//


fn spawn_thread<F, T>(name: &str, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .unwrap_or_else(|_| panic!("Error spawning thread {name}"))
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Fut<T> = Pin<Box<dyn Future<Output = T>>>;

fn main() {
    elapsedlogger::init_logging();

    let mut i = 0;

    // iter.par_bridge().for_each(|x| {
    //     if !x.unwrap().metadata().unwrap().is_file() {
    //         println!("Not file");
    //     }
    // });

    #[cfg(target_family = "unix")]
    unsafe {
        // This sets a restrictive umask to prevent other users from reading anything written by
        // this program. Images can be private and sockets can be used to run arbitrary
        // executables.
        // libc::umask(0o077);
        // Tune memory trimming, otherwise the resident memory set tends to explode in size. The
        // default behaviour is dynamic and seems very poorly tuned for applications like an
        // image viewer.
        // #[cfg(target_env = "gnu")]
        // libc::mallopt(libc::M_TRIM_THRESHOLD, 128 * 1024);
    }


    config::init();

    // All GTK calls that could possibly be reached before this completes (pixbuf, channel sends)
    // are safe to call off the main thread and before GTK is initialzed.
    gtk::init().expect("GTK could not be initialized");

    let (manager_sender, manager_receiver) = tokio::sync::mpsc::unbounded_channel();
    // PRIORITY_DEFAULT is enough to be higher priority than GTK redrawing events.
    let (gui_sender, gui_receiver) = glib::MainContext::channel(glib::PRIORITY_LOW);

    closing::init(gui_sender.clone());

    // let sock_handle = socket::init(&gui_sender);
    let man_handle = manager::run(manager_receiver, gui_sender);

    // No one should ever have this disabled
    Settings::default().unwrap().set_gtk_hint_font_metrics(true);

    if let Err(e) = catch_unwind(AssertUnwindSafe(|| gui::run(manager_sender, gui_receiver))) {
        // This will only happen on programmer error, but we want to make sure the manager thread
        // has time to exit and clean up temporary files.
        // The only things we do after this are cleanup.
        error!("gui::run panicked unexpectedly: {:?}", e);

        // This should _always_ be a no-op since it should have already been closed by a
        // CloseOnDrop.
        closing::close();
    }

    // These should never panic on their own, but they may if they're interacting with the gui
    // thread and it panics.
    if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
        drop(man_handle.join());
    })) {
        error!("Joining manager thread panicked unexpectedly: {:?}", e);

        closing::close();
    }
}
