use std::cell::RefCell;
use std::env::{current_dir, set_current_dir};
use std::path::Path;
use std::rc::Rc;

use ahash::AHashMap;
use gtk::gdk::Display;
use gtk::gio::{AppInfo, File};
use gtk::glib;
use gtk::prelude::{AppInfoExt, DisplayExt, GdkAppLaunchContextExt};

use self::open_with::OpenWith;
use super::tabs::id::TabId;
use super::tabs::list::TabPosition;
use super::{show_error, show_warning, tabs_run, ActionTarget, Gui, Selected};
use crate::com::{EntryKind, EntryObject, ManagerAction};
use crate::gui::gui_run;

mod application;
mod open_with;

// Only open new tabs if the number of directories is below this number.
static DIR_OPEN_LIMIT: usize = 10;

thread_local! {
    // Avoid repeat lookups, even if nothing was found.
    static DEFAULT_CACHE: RefCell<AHashMap<&'static str, Option<AppInfo>>> = RefCell::default()
}

fn cached_lookup(mime: &'static str) -> Option<AppInfo> {
    DEFAULT_CACHE.with_borrow_mut(|m| {
        if let Some(ai) = m.get(mime) {
            return ai.clone();
        }

        let ai = AppInfo::default_for_type(mime, false);

        m.insert(mime, ai.clone());
        ai
    })
}

fn partition_and_launch(tab_dir: &Path, display: &Display, entries: &[EntryObject]) {
    // Only error on the first one
    let mut sent_error = false;

    let mut apps: Vec<(AppInfo, Vec<File>)> = Vec::new();

    for entry in entries {
        let entry = entry.get();

        let Some(app) = cached_lookup(entry.mime) else {
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

    if !current_dir().is_ok_and(|d| d == tab_dir) {
        debug!("Changing working directory to {tab_dir:?}");

        if let Err(e) = set_current_dir(tab_dir) {
            show_warning(format!("Could not change to directory for application launch: {e}"));
        }
    }

    for (app, files) in apps {
        if let Err(e) = app.launch(&files, Some(&context)) {
            show_error(&format!("Application launch error: {app:?} {e:?}"));
        }
    }
}

static BOTH_ERROR: &str = "Can't launch directories and files together";

pub(super) fn open(
    tab: TabId,
    tab_dir: &Path,
    display: &Display,
    selected: Selected<'_>,
    execute: bool,
) {
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
                    g.send_manager(ManagerAction::Launch(f.get().abs_path.clone(), g.get_env()))
                })
            });

            return;
        }
    }

    if !files.is_empty() {
        return partition_and_launch(tab_dir, display, &files);
    }

    if directories.len() > DIR_OPEN_LIMIT {
        return show_warning(format!("Can't load more than {DIR_OPEN_LIMIT} directories at once"));
    }

    // Can be called while the TabsList lock is held.
    // This is less than perfectly efficient but it doesn't matter.
    glib::idle_add_local_once(move || {
        tabs_run(|t| {
            if directories.len() == 1 {
                t.navigate_open_tab(tab, &directories[0]);
                return;
            }

            // Open tabs in reverse order.
            for d in directories.into_iter().rev() {
                t.open_tab(&d, TabPosition::After(ActionTarget::Tab(tab)), false);
            }
        })
    });
}

impl Gui {
    pub(super) fn open_with(self: &Rc<Self>, selected: Selected<'_>) {
        OpenWith::open(self, selected);
    }
}
