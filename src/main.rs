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

use std::any::Any;
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

mod elapsedlogger;

mod closing;
mod com;
mod config;
mod database;
mod gui;
mod manager;
mod natsort;

fn handle_panic(_e: Box<dyn Any + Send>) {
    error!("Unexpected panic in thread {}", thread::current().name().unwrap_or("unnamed"));
    closing::close();
}

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

fn main() {
    elapsedlogger::init_logging();
    config::init();

    gtk::init().expect("GTK could not be initialized");

    let (manager_sender, manager_receiver) = tokio::sync::mpsc::unbounded_channel();
    // PRIORITY_LOW prioritize GTK redrawing events.
    let (gui_sender, gui_receiver) = glib::MainContext::channel(glib::PRIORITY_LOW);

    closing::init(gui_sender.clone());

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
