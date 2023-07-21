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

use super::id::TabId;
use super::tab::Tab;
use crate::com::{DirSnapshot, DisplayMode, EntryObjectSnapshot, SortSettings, Update};
use crate::config::OPTIONS;
use crate::gui::main_window::MainWindow;
use crate::gui::tabs::id::next_id;


// Maximum number of panes per window
const MAX_PANES: usize = 3;

// Unique highlights for each selected tab per pane.
// TODO -- this or just brighter/darker?
const PANE_COLOURS: [RGBA; MAX_PANES] = [RGBA::BLUE, RGBA::GREEN, RGBA::RED];


// TODO -- to support multiple panes, move active out of this
// This is tightly couples the Tab implementation, right now.
#[derive(Debug)]
pub struct TabsList {
    tabs: Vec<Tab>,

    // There is always one active tab or no visible tabs.
    // There may be tabs that are not visible.
    active: Option<TabId>,

    //tabs_store: gio::ListStore,
    tabs_container: gtk::ListView,
    pane_container: gtk::Box,
}

impl TabsList {
    pub fn new(window: &MainWindow) -> Self {
        Self {
            tabs: Vec::new(),
            active: None,

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
        self.tabs[0].new_pane(&self.pane_container);
    }

    pub fn update_sort(&mut self, id: TabId, settings: SortSettings) {
        // Assume we can't update a tab that doesn't exist -- this should always be called
        // synchronously
        self.find_mut(id).unwrap().update_sort(settings);
    }

    pub fn set_active(&mut self, id: TabId) {
        debug_assert!(self.position(id).is_some());

        self.active = Some(id);
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
            let (left_tabs, first_tab, right_tabs) = self.split_around_mut(i);
            first_tab.apply_snapshot(left_tabs, right_tabs, snap);
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

        if let Some(index) = self.tabs.iter().position(|t| t.matches_update(&update)) {
            let (left_tabs, tab, right_tabs) = self.split_around_mut(index);
            tab.matched_update(left_tabs, right_tabs, update);
        } else {
            // TODO [search] handle search updates, which will be expensive but rare,
            // and cheap when there is no search.
            Tab::handle_unmatched_update(&mut self.tabs, update);
        };
    }

    // Applies a search snapshot to its matching tab.
    //
    // This cannot cause updates to other tabs, but it can fail to update some EntryObjects with
    // the newest versions if this snapshot has newer versions. To prevent races, flat tab
    // snapshots always take priority.
    // pub fn apply_search_snapshot(&mut self, snap: SearchSnapshot) {}

    // Unlike with update() above, we know this is going to be the same Arc<>
    pub fn directory_failure(&mut self, path: Arc<Path>) {
        let mut i = 0;
        while let Some(j) = self.tabs.iter().skip(i).position(|t| t.matches_arc(&path)) {
            i += j;
            let (left, tab, right) = self.split_around_mut(i);
            tab.handle_directory_deleted(left, right);
        }
    }

    pub fn navigate(&mut self, id: TabId, target: &Path) {
        let index = self.position(id).unwrap();
        let (left, tab, right) = self.split_around_mut(index);

        tab.navigate(left, right, target)
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

        let tab = &self.tabs[index];
        if tab.visible() {
            // If there's another tab that isn't visible, use it.
            // We want to prioritize the first tab to the left/above in the visible list.
            // If there is no tab at all, we'll also be closing the pane.
            // let next_tab =
        }

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

    pub fn active_close_tab(&mut self) {
        let Some(active) = self.active else {
            warn!("CloseTab called with no active tab");
            return;
        };

        self.close_tab(active);
    }

    pub fn active_close_pane(&mut self) {
        let Some(active) = self.active else {
            warn!("ClosePane called with no open panes");
            return;
        };

        // TODO -- maybe allow for empty views.
        // if self.tabs.len() == 1 {
        //     warn!("ClosePane called with only one tab");
        //     return;
        // }

        // TODO -- find parent ex-sibling pane, if any,
        // if let Some(sibling) = self.find_sibling_tab(active) {
        //     self.active = Some(sibling);
        //     // Move focus to that pane.
        //     self.move_focus_into(sibling);
        // } else {
        //     self.active = None;
        // }

        let index = self.position(active).unwrap();
        self.tabs[index].close_pane();
    }

    pub fn active_close_both(&mut self) {
        let Some(active) = self.active else {
            warn!("ClosePaneAndTab called with no open panes");
            return;
        };
    }

    pub fn active_display_mode(&mut self, mode: DisplayMode) {
        let Some(active) = self.active else {
            warn!("Mode called with no open panes");
            return;
        };

        let index = self.position(active).unwrap();
        self.tabs[index].update_display_mode(mode);
    }

    pub fn active_parent(&mut self) {
        let Some(active) = self.active else {
            warn!("Parent called with no open panes");
            return;
        };

        let index = self.position(active).unwrap();
        let (left, tab, right) = self.split_around_mut(index);
        tab.parent(left, right);
    }
}
