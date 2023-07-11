use std::path::PathBuf;

use gtk::gdk::gio::FileMonitor;

pub enum MonitorChange {
    Start(PathBuf),
    Stop(PathBuf),
}

pub enum UpdateEvent {}

fn start() {
    todo!();
}

fn changed_event() {}
