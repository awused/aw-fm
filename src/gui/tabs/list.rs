use std::env::current_dir;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gtk::gdk::RGBA;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{BoxExt, WidgetExt};
use gtk::Orientation;
use path_clean::PathClean;

use super::{Tab, TabId};
use crate::com::{DirSnapshot, DisplayMode, EntryObjectSnapshot, SortSettings, Update};
use crate::config::OPTIONS;
use crate::gui::main_window::MainWindow;
use crate::gui::tabs::id::next_id;

// mod tab_element;

// Maximum number of panes per window
const MAX_PANES: usize = 3;

// Unique highlights for each selected tab per pane.
const PANE_COLOURS: [RGBA; MAX_PANES] = [RGBA::BLUE, RGBA::GREEN, RGBA::RED];


// TODO -- to support multiple panes, move active out of this
// This is tightly couples the Tab implementation, right now.
#[derive(Debug)]
pub struct TabsList {
    tabs: Vec<Tab>,

    //tabs_store: gio::ListStore,
    tabs_container: gtk::ListView,
    pane_container: gtk::Box,
}

impl TabsList {
    pub fn new(window: &MainWindow) -> Self {
        Self {
            tabs: Vec::new(),

            tabs_container: window.imp().tabs.clone(),
            pane_container: window.imp().panes.clone(),
        }
    }

    pub fn setup(&mut self) {
        assert!(self.tabs.is_empty());

        let mut path = OPTIONS
            .file_name
            .clone()
            .unwrap_or_else(|| current_dir().unwrap_or_else(|_| "/".into()))
            .clean();

        if path.is_relative() {
            // prepending "/" is likely to be wrong, but eh.
            let mut abs = current_dir().unwrap_or_else(|_| "/".into());
            abs.push(path);
            path = abs.clean();
        }

        let first_tab_element = ();

        self.tabs.push(Tab::new(next_id(), path, first_tab_element, &[]));
        self.tabs[0].load(&mut [], &mut []);
        self.tabs[0].display(&self.pane_container);
    }

    pub fn update_sort(&mut self, id: TabId, settings: SortSettings) {
        // Assume we can't update a tab that doesn't exist -- this should always be called
        // synchronously
        self.find_mut(id).unwrap().update_sort(settings);
    }

    fn find_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id() == id)
    }

    fn position(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id() == id)
    }

    fn update_display_mode(&mut self, id: TabId, mode: DisplayMode) {
        // Assume we can't update a tab that doesn't exist -- this should always be called
        // synchronously
        self.find_mut(id).unwrap().update_display_mode(mode);
    }

    fn split_around_mut(&mut self, index: usize) -> (&mut [Tab], &mut Tab, &mut [Tab]) {
        let (left, rest) = self.tabs.split_at_mut(index);
        let ([center], right) = rest.split_at_mut(1) else {
            unreachable!();
        };

        (left, center, right)
    }

    pub fn apply_snapshot(&mut self, snap: DirSnapshot) {
        let first_match = self.tabs.iter().position(|t| t.matches_snapshot(&snap.id));
        if let Some(i) = first_match {
            let (_, first_tab, right_tabs) = self.split_around_mut(i);
            first_tab.apply_snapshot(right_tabs, snap);
        }
    }

    pub fn update(&mut self, update: Update) {
        // Handle the case where a tab's directory is deleted out from underneath it.
        // This is handled immediately, even if the directory is still being loaded.
        if let Update::Removed(path) = &update {
            let mut i = 0;
            while let Some(j) =
                self.tabs.iter().skip(i).position(|t| t.check_directory_deleted(path))
            {
                i += j;
                let (left, tab, right) = self.split_around_mut(i);
                tab.handle_directory_deleted(left, right);
            }
        }

        let Some(index) = self.tabs.iter().position(|t| t.matches_update(&update)) else {
            return;
        };

        let (_, tab, right_tabs) = self.split_around_mut(index);
        tab.apply_update(right_tabs, update);
    }

    // Unlike with update() above, we know this is going to be the same Arc<>
    pub fn directory_failure(&mut self, path: Arc<Path>) {
        let mut i = 0;
        while let Some(j) = self.tabs.iter().skip(i).position(|t| t.matches_arc(&path)) {
            i += j;
            let (left, tab, right) = self.split_around_mut(i);
            tab.handle_directory_deleted(left, right);
        }
    }

    fn navigate(&mut self, id: TabId) {
        let index = self.position(id).unwrap();
        let (left, tab, right) = self.split_around_mut(index);
        todo!()
    }

    fn clone_tab(&mut self, index: usize) {
        let mut new_tab = Tab::cloned(next_id(), &self.tabs[index], ());

        self.tabs.insert(index + 1, new_tab);
    }

    // For now, tabs always open right of the active tab
    fn open_tab(&mut self, path: PathBuf, active_tab: TabId) {
        let mut new_tab = Tab::new(next_id(), path, (), &self.tabs);

        self.tabs.insert(self.position(active_tab).unwrap() + 1, new_tab);
    }

    pub fn close_tab(&mut self, id: TabId) {
        let index = self.position(id).unwrap();
        // TODO -- If all tabs are active
        if self.tabs.len() == 1 {
            self.open_tab(".".into(), id);
        }

        let tab = &self.tabs[index];
        let pane_index = Some(0); // ???

        // let active = self.tabs[index].is_active()
        self.tabs.remove(index);

        // if let Some(pane_index) = pane_index {
        //     // TODO -- handle case where multiple tabs are active in panes
        //     // Should grab the next
        //     let (left, new_active, right) = self.split_around_mut(self.active);
        //     new_active.load(left, right);
        //     self.tabs[self.active].display(&self.pane_container);
        // }
    }

    // This is necessary to attempt to avoid a gtk crash
    pub fn finish_apply_view_state(&mut self, id: TabId) {
        let Some(index) = self.position(id) else {
            return;
        };

        self.tabs[index].finish_apply_view_state();
    }
}
