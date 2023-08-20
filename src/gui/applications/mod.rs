use std::cell::RefCell;
use std::rc::Rc;

use ahash::AHashMap;
use gtk::gdk::Display;
use gtk::gio::{AppInfo, File};
use gtk::glib;
use gtk::prelude::{AppInfoExt, DisplayExt, GdkAppLaunchContextExt};

use self::open_with::OpenWith;
use super::tabs::id::TabId;
use super::{show_error, show_warning, tabs_run, Gui, Selected};
use crate::com::{EntryKind, EntryObject};
use crate::gui::gui_run;

mod application;
mod open_with;

// Only open new tabs if the number of directories is below this number.
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

        if let Some((_app, v)) = apps.iter_mut().find(|(a, _)| a.equal(&app)) {
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

pub(super) fn open(tab: TabId, display: &Display, selected: Selected<'_>, execute: bool) {
    if selected.len() == 0 {
        warn!("Activated with no items");
    }

    // Don't allow both at once.
    let mut directories = Vec::new();
    let mut files = Vec::new();

    for eo in selected {
        if eo.get().dir() {
            directories.push(eo.get().abs_path.clone());
        } else {
            files.push(eo);
        }

        if !files.is_empty() && !directories.is_empty() {
            return show_warning(BOTH_ERROR);
        }
    }

    if execute && files.len() == 1 {
        let e = files[0].get();
        if let EntryKind::File { executable: true, .. } = &e.kind {
            // Could build some kind of whitelist/blacklist for trusted files.
            info!("Executing file {:?}", e.abs_path);
            drop(e);

            let f = files.pop().unwrap();

            // Can be called while the TabsList lock is held.
            glib::idle_add_local_once(move || {
                gui_run(|g| {
                    g.send_manager(crate::com::ManagerAction::Execute(
                        f.get().abs_path.clone(),
                        g.get_env(),
                    ))
                })
            });

            return;
        }
    }

    if !files.is_empty() {
        return partition_and_launch(display, &files);
    }

    if directories.len() > DIR_OPEN_LIMIT {
        return show_warning(format!("Can't load more than {DIR_OPEN_LIMIT} directories at once"));
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

impl Gui {
    pub(super) fn open_with(self: &Rc<Self>, selected: Selected<'_>) {
        OpenWith::open(self, selected);
    }
}
