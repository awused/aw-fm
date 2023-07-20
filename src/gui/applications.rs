use std::cell::RefCell;

use ahash::AHashMap;
use gtk::gdk::traits::AppLaunchContextExt;
use gtk::gdk::{AppLaunchContext, Display};
use gtk::gio::{AppInfo, File};
use gtk::prelude::{AppInfoExt, Cast, DisplayExt, ListModelExt};
use gtk::traits::SelectionModelExt;
use gtk::{glib, MultiSelection};

use super::tabs::TabId;
use super::tabs_run;
use crate::com::EntryObject;
use crate::gui::gui_run;


thread_local! {
    // Avoid repeat lookups, even if nothing was found.
    static DEFAULT_CACHE: RefCell<AHashMap<String, Option<AppInfo>>> = RefCell::default()
}

fn cached_lookup(mime: &str) -> Option<AppInfo> {
    DEFAULT_CACHE.with(|c| {
        let mut m = c.borrow_mut();

        if let Some(Some(ai)) = m.get(mime) {
            return Some(ai.clone());
        }

        let ai = AppInfo::default_for_type(mime, false);

        m.insert(mime.to_string(), ai.clone());
        ai
    })
}

fn partition_and_launch(display: &Display, entries: &[EntryObject]) {
    // Only error on the first one
    let mut sent_error = false;

    let mut apps: Vec<(AppInfo, Vec<File>)> = Vec::new();

    for entry in entries {
        let entry = entry.get();

        let Some(app) = cached_lookup(&entry.mime) else {
            continue;
        };

        let file = File::for_path(&entry.abs_path);

        if let Some((app, v)) = apps.iter_mut().find(|(a, _)| a == &app) {
            v.push(file)
        } else {
            apps.push((app, vec![file]));
        }
    }

    let context = display.app_launch_context();
    context.set_timestamp(gtk::gdk::CURRENT_TIME);

    for (app, files) in apps {
        app.launch(&files, Some(&context));
    }
}

static BOTH_ERROR: &str = "Can't launch directories and files together";

pub fn activate(tab: TabId, display: &Display, selection: &MultiSelection) {
    let selected = selection.selection();
    if selected.size() == 0 {
        warn!("Activated with no items");
    }

    // Don't allow both at once.
    let mut directories = Vec::new();
    let mut files = Vec::new();

    for i in 0..selected.size() {
        let eo = selection.item(selected.nth(i as u32)).unwrap();
        let eo = eo.downcast::<EntryObject>().unwrap();

        if eo.get().dir() {
            directories.push(eo.get().abs_path.clone());
        } else {
            files.push(eo);
        }

        if !files.is_empty() && !directories.is_empty() {
            error!("{BOTH_ERROR}");
            gui_run(|g| g.convey_error(BOTH_ERROR));
            return;
        }
    }

    if !files.is_empty() {
        partition_and_launch(display, &files);
        return;
    }

    // Can be called while the TabsList lock is held.
    // This is less than perfectly efficient but it doesn't matter.
    glib::idle_add_local_once(move || {
        tabs_run(|t| {
            if directories.len() == 1 {
                t.navigate(tab, &directories[0]);
                return;
            }

            // Open tabs in this order so they're
            directories.reverse();
            error!("todo open tabs");
        })
    });
}
