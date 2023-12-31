#![cfg_attr(not(feature = "windows-console"), windows_subsystem = "windows")]

#[macro_use]
extern crate log;

// The tikv fork may not be easily buildable for Windows
#[cfg(all(not(target_env = "msvc"), not(debug_assertions)))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread::{self, JoinHandle};

use gtk::{glib, Settings};

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
        .unwrap_or_else(|e| panic!("Error spawning thread {name}: {e}"))
}

fn main() {
    elapsedlogger::init_logging();
    config::init();

    gtk::init().expect("GTK could not be initialized");

    let (manager_sender, manager_receiver) = tokio::sync::mpsc::unbounded_channel();
    // PRIORITY_LOW prioritize GTK redrawing events.
    let (gui_sender, gui_receiver) = glib::MainContext::channel(glib::Priority::HIGH_IDLE);

    closing::init(gui_sender.clone());

    let man_handle = manager::run(manager_receiver, gui_sender);

    // No one should ever have this disabled
    Settings::default().unwrap().set_gtk_hint_font_metrics(true);

    if let Err(e) = catch_unwind(AssertUnwindSafe(|| gui::run(manager_sender, gui_receiver))) {
        // This will only happen on programmer error, but we want to make sure the manager thread
        // has time to exit and clean up temporary files.
        // The only things we do after this are cleanup.
        error!("gui::run panicked unexpectedly: {e:?}");

        // This should _always_ be a no-op since it should have already been closed by a
        // CloseOnDrop.
        closing::close();
    }

    // These should never panic on their own, but they may if they're interacting with the gui
    // thread and it panics.
    if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
        drop(man_handle.join());
    })) {
        error!("Joining manager thread panicked unexpectedly: {e:?}");

        closing::close();
    }
}
