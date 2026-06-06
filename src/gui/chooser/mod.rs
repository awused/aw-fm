use std::convert::Into;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use gtk::gio::prelude::FileExt;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::glib::{self, GString};
use gtk::prelude::{BoxExt, EditableExt};
use gtk::{SelectionModel, gio};

use crate::closing;
use crate::config::{ChooserCommand, OPTIONS};
use crate::gui::chooser::bar::ChooserBar;
use crate::gui::{Gui, Selected, gui_run, show_error};

pub mod bar;

#[derive(Debug)]
pub struct Chooser {
    pub bar: ChooserBar,

    root: Option<Arc<Path>>,

    files: Vec<Arc<Path>>,
    last_text: String,
}

pub(super) fn chooser_run<R, F: FnOnce(&mut Chooser) -> R>(f: F) -> Option<R> {
    gui_run(|g| g.chooser.borrow_mut().as_mut().map(f))
}

impl ChooserCommand {
    pub const fn open_dir(&self) -> bool {
        if let Self::OpenFile { directory, .. } = self { *directory } else { false }
    }

    pub const fn open(&self) -> bool {
        matches!(self, Self::OpenFile { .. })
    }
}

impl Chooser {
    pub(super) fn setup(g: &Rc<Gui>) -> Option<Self> {
        let mode = OPTIONS.chooser_mode.as_ref()?;

        let bar = bar::ChooserBar::new(mode);

        g.window.imp().main_wrapper.append(&bar);

        Some(Self {
            last_text: bar.imp().text_entry.text().into(),
            bar,
            root: None,
            files: Vec::new(),
        })
    }

    pub(super) fn selection(&mut self, selection: &SelectionModel) {
        let cmd = OPTIONS.chooser_mode.as_ref().unwrap();

        let files = Selected::from(selection);


        if files.len() == 1 {
            let f = files.get(0);
            let e = f.get();
            if cmd.open_dir() && e.dir() || !cmd.open_dir() && !e.dir() {
                // TODO[path] -- correctly round-trip non-utf8 paths
                let rel_path = self
                    .root
                    .as_ref()
                    .and_then(|root| e.abs_path.strip_prefix(root).ok())
                    .map_or_else(|| e.abs_path.clone(), Into::into);

                self.last_text = rel_path.to_string_lossy().to_string();

                let text = self.last_text.clone();
                let bar = self.bar.clone();
                glib::idle_add_local_once(move || {
                    bar.imp().text_entry.set_text(&text);
                });
            }
        } else if files.len() > 1 && cmd.open() {
            self.last_text = String::new();
            let bar = self.bar.clone();
            glib::idle_add_local_once(move || {
                bar.imp().text_entry.set_text("");
            });
        }

        self.files = files
            .filter(|eo| (cmd.open_dir() && eo.get().dir()) || !cmd.open_dir() && !eo.get().dir())
            .map(|eo| eo.get().abs_path.clone())
            .collect();
    }

    // If the root changes, clear any multi-selection
    pub(super) fn root(&mut self, root: &Arc<Path>) {
        self.files.clear();
        self.root = Some(root.clone());
    }

    pub(super) fn accept(&mut self) {
        let cmd = OPTIONS.chooser_mode.as_ref().unwrap();

        let files = if !self.files.is_empty() {
            // We can assume these exist for opening, at least.
            self.files.clone()
        } else if let Some(root) = &self.root {
            let path: Arc<Path> = root.join(Path::new(&self.last_text)).into();

            if path.is_dir() && !cmd.open_dir() {
                return warn!("Can't open directory {path:?}");
            } else if !path.exists() && cmd.open() {
                return warn!("Can't open file that doesn't exist {path:?}");
            }

            vec![path]
        } else {
            return show_error("Tried to accept empty choice, this shouldn't happen");
        };

        if cmd.open() {
            info!("Selected {} files", files.len());
            for f in files {
                println!("{}", gio::File::for_path(f).uri());
            }
            closing::close();
            return;
        }

        info!("TODO save things");
        println!("cancelled");
        closing::close();
    }

    fn text(&mut self, text: GString) {
        if self.last_text == text {
            return;
        }

        self.last_text = text.into();
        self.files.clear();
    }
}
