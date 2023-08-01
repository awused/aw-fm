use std::cell::RefCell;

use ahash::AHashMap;
use gtk::gdk::Display;
use gtk::gio::{AppInfo, File};
use gtk::prelude::{AppInfoExt, Cast, DisplayExt, GdkAppLaunchContextExt, ListModelExt};
use gtk::traits::SelectionModelExt;
use gtk::{glib, MultiSelection};

use super::tabs::id::TabId;
use super::{show_error, show_warning, tabs_run};
use crate::com::EntryObject;


static DIR_OPEN_LIMIT: usize = 10;

thread_local! {
    // Avoid repeat lookups, even if nothing was found.
    static DEFAULT_CACHE: RefCell<AHashMap<String, Option<AppInfo>>> = RefCell::default()
}

fn cached_lookup(mime: &str) -> Option<AppInfo> {
    DEFAULT_CACHE.with(|c| {
        let mut m = c.borrow_mut();

        if let Some(ai) = m.get(mime) {
            return ai.clone();
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
            if !sent_error {
                show_warning(&format!("Couldn't find application for mimetype: {}", entry.mime));
                sent_error = true;
            }
            continue;
        };

        let file = File::for_path(&entry.abs_path);

        if let Some((_app, v)) = apps.iter_mut().find(|(a, _)| a == &app) {
            v.push(file)
        } else {
            apps.push((app, vec![file]));
        }
    }

    let context = display.app_launch_context();
    context.set_timestamp(gtk::gdk::CURRENT_TIME);

    for (app, files) in apps {
        if let Err(e) = app.launch(&files, Some(&context)) {
            show_error(&format!("Application launch error: {app:?} {e:?}"));
        }
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
            return show_warning(BOTH_ERROR);
        }
    }

    if !files.is_empty() {
        return partition_and_launch(display, &files);
    }

    if directories.len() > DIR_OPEN_LIMIT {
        return show_warning(&format!("Can't load more than {DIR_OPEN_LIMIT} directories at once"));
    }

    // Can be called while the TabsList lock is held.
    // This is less than perfectly efficient but it doesn't matter.
    glib::idle_add_local_once(move || {
        tabs_run(|t| {
            if directories.len() == 1 {
                t.navigate(tab, &directories[0]);
                return;
            }

            // Open tabs in reverse order.
            // directories.reverse();
            for d in directories.into_iter().rev() {
                t.open_tab(&d, false);
            }
        })
    });
}
