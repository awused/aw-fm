use std::path::Path;
use std::sync::Arc;

use dirs::home_dir;
use gtk::gio::ListStore;
use gtk::prelude::{Cast, CastNone, ListModelExt, StaticType};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::BoxExt;
use gtk::{NoSelection, Orientation, SignalListItemFactory};

use super::element::TabElement;
use super::id::TabId;
use super::tab::Tab;
use crate::com::{DirSnapshot, DisplayMode, SortSettings, Update};
use crate::gui::main_window::MainWindow;
use crate::gui::tabs::id::next_id;
use crate::gui::tabs::NavTarget;
use crate::gui::{gui_run, tabs_run};

// For event handlers which cannot be run with the tabs lock being held.
// Assumes the tab still exists since GTK notifies are run synchronously.
pub(super) fn event_run_tab<T, F: FnOnce(&mut Tab, &[Tab], &[Tab]) -> T>(id: TabId, f: F) -> T {
    tabs_run(|t| t.must_run_tab(id, f))
}


// This is tightly coupled the Tab implementation, right now.
#[derive(Debug)]
pub struct TabsList {
    // Assume the number of tabs is small enough and sane.
    // We iterate over all tabs a lot, and we assume a linear scan is going to be fast enough.
    // If that stops being the case it's annoying but not infeasible to use a map.
    // TODO -- limit number of tabs to 255 or so?
    tabs: Vec<Tab>,

    // There is always one active tab or no visible tabs.
    // There may be tabs that are not visible.
    active: Option<TabId>,

    tab_elements: ListStore,
    _tabs_container: gtk::ListView,
    pane_container: gtk::Box,
}

impl TabsList {
    pub fn new(window: &MainWindow) -> Self {
        let tabs_container = window.imp().tabs.clone();
        let tab_elements = ListStore::new(TabElement::static_type());


        let factory = SignalListItemFactory::new();
        factory.connect_bind(|_factory, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let tab = item.item().unwrap().downcast::<TabElement>().unwrap();
            item.set_child(Some(&tab));
        });
        factory.connect_unbind(|_factory, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            item.set_child(None::<&TabElement>);
        });

        let selection = NoSelection::new(Some(tab_elements.clone()));

        tabs_container.set_factory(Some(&factory));
        tabs_container.set_model(Some(&selection));

        tabs_container.set_single_click_activate(true);
        tabs_container.connect_activate(|c, a| {
            let model = c.model().unwrap();
            let element = model.item(a).and_downcast::<TabElement>().unwrap();

            let id = *element.imp().tab.get().unwrap();
            tabs_run(|ts| ts.switch_active_tab(id));
        });

        Self {
            tabs: Vec::new(),
            active: None,

            tab_elements,
            _tabs_container: tabs_container,
            pane_container: window.imp().panes.clone(),
        }
    }

    pub fn open_initial_tab(&mut self) {
        assert!(self.tabs.is_empty());


        let Some(target) = NavTarget::initial(self) else {
            return;
        };

        let (tab, element) = Tab::new(next_id(), target, &[]);

        self.tabs.push(tab);
        self.tab_elements.append(&element);
        self.tabs[0].attach_pane(&[], &[], |w| self.pane_container.append(w));
        self.set_active(self.tabs[0].id());
    }

    fn find_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id() == id)
    }

    fn update_display_mode(&mut self, id: TabId, mode: DisplayMode) {
        if let Some(t) = self.find_mut(id) {
            t.update_display_mode(mode)
        }
    }

    pub fn update_sort(&mut self, id: TabId, settings: SortSettings) {
        if let Some(t) = self.find_mut(id) {
            t.update_sort(settings)
        }
    }

    pub fn set_active(&mut self, id: TabId) {
        // this should be synchronous so should never fail.
        let index = self.position(id).unwrap();

        if let Some(old) = self.active {
            let old_index = self.position(old).unwrap();
            self.tabs[old_index].set_inactive();
        }

        self.active = Some(id);
        self.tabs[index].set_active();
    }

    fn position(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id() == id)
    }

    fn element_position(&self, id: TabId) -> Option<u32> {
        (&self.tab_elements)
            .into_iter()
            .position(|e| {
                let t = e.unwrap().downcast::<TabElement>().unwrap();
                t.imp().tab.get().unwrap() == &id
            })
            .map(|u| u as u32)
    }

    fn active_element(&self) -> Option<u32> {
        let id = self.active?;

        // Assert it exists.
        Some(self.element_position(id).unwrap())
    }

    fn element(&self, i: u32) -> TabElement {
        self.tab_elements.item(i).and_downcast().unwrap()
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
                if !tab.handle_directory_deleted(left, right) {
                    error!("Loading {path:?} and all parent directories failed.");
                    gui_run(|g| {
                        g.error(&format!(
                            "Unexpected failure loading {path:?} and all parent directories"
                        ))
                    });
                    let id = tab.id();
                    self.close_tab(id);
                    continue;
                }
                i += 1
            }
        }

        if let Some(index) = self.tabs.iter().position(|t| t.matches_flat_update(&update)) {
            let (left_tabs, tab, right_tabs) = self.split_around_mut(index);
            tab.matched_flat_update(left_tabs, right_tabs, update);
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
            if !tab.handle_directory_deleted(left, right) {
                error!("Loading {path:?} and all parent directories failed.");
                gui_run(|g| {
                    g.error(&format!(
                        "Unexpected failure loading {path:?} and all parent directories"
                    ))
                });
                let id = tab.id();
                self.close_tab(id);
                continue;
            }
            i += 1
        }
    }

    pub fn navigate(&mut self, id: TabId, path: &Path) {
        let Some(index) = self.position(id) else {
            return;
        };

        let Some(target) = NavTarget::open_or_jump(path, self) else {
            return;
        };

        let (left, tab, right) = self.split_around_mut(index);

        tab.navigate(left, right, target)
    }

    pub fn get_active_dir(&self) -> Option<Arc<Path>> {
        let Some(active) = self.active else {
            return None;
        };

        let pos = self.position(active).unwrap();
        Some(self.tabs[pos].dir())
    }

    fn switch_active_tab(&mut self, id: TabId) {
        if Some(id) == self.active {
            return;
        }

        let index = self.position(id).unwrap();

        if self.tabs[index].visible() {
            debug!("Switching to visible tab {id:?}");
            self.set_active(id);
            return;
        }

        let Some(active) = self.active else {
            info!("Opening new pane on switch");
            let pane_container = self.pane_container.clone();
            let (left, tab, right) = self.split_around_mut(index);
            tab.attach_pane(left, right, |w| pane_container.append(w));
            self.set_active(id);
            return;
        };

        debug!("Switching to non-visible tab {id:?} from {:?}", active);
        let active_index = self.position(active).unwrap();

        assert_ne!(index, active_index);
        if index < active_index {
            let (left, right) = self.tabs.split_at_mut(active_index);
            left[index].replace_pane(&mut right[0]);
        } else {
            let (left, right) = self.tabs.split_at_mut(index);
            right[0].replace_pane(&mut left[active_index]);
        }

        self.set_active(id);
    }

    fn swap_panes(&mut self, visible: TabId, inactive: TabId) {
        debug_assert!(self.tabs[self.position(visible).unwrap()].visible());
        debug_assert!(!self.tabs[self.position(inactive).unwrap()].visible());

        if Some(visible) == self.active {
            self.switch_active_tab(inactive);
            return;
        }

        info!("Swapping pane from {visible:?} to {inactive:?}");
        error!("TODO -- haven't tested this");
        let old_index = self.position(visible).unwrap();
        let new_index = self.position(inactive).unwrap();

        if new_index < old_index {
            let (left, right) = self.tabs.split_at_mut(old_index);
            left[new_index].replace_pane(&mut right[0]);
        } else {
            let (left, right) = self.tabs.split_at_mut(new_index);
            right[0].replace_pane(&mut left[old_index]);
        }
    }

    fn clone_active(&mut self) -> Option<(TabId, usize)> {
        let index = self.position(self.active?).unwrap();
        let element_index = self.active_element().unwrap();

        let (new_tab, element) = Tab::cloned(next_id(), &self.tabs[index]);
        let id = new_tab.id();

        self.tabs.push(new_tab);
        self.tab_elements.insert(element_index + 1, &element);

        Some((id, self.tabs.len() - 1))
    }

    fn create_tab(&mut self, target: NavTarget, activate: bool) -> TabId {
        let (new_tab, element) = Tab::new(next_id(), target, &self.tabs);

        let id = new_tab.id();
        self.tabs.push(new_tab);

        if let Some(active_index) = self.active_element() {
            self.tab_elements.insert(active_index + 1, &element);
        } else {
            self.tab_elements.append(&element);
        }

        if activate {
            self.switch_active_tab(id)
        }
        id
    }

    // For now, tabs always open after the active tab
    // !activate -> background tab
    pub fn open_tab<P: AsRef<Path>>(&mut self, path: P, activate: bool) {
        let Some(target) = NavTarget::open_or_jump(path, self) else {
            return;
        };

        self.create_tab(target, activate);
    }

    // Clones the active tab or opens a new tab to the user's home directory.
    pub fn new_tab(&mut self, activate: bool) {
        if let Some((id, _)) = self.clone_active() {
            if activate {
                self.switch_active_tab(id)
            }
            return;
        }

        let Some(home) = home_dir() else {
            return;
        };

        let Some(target) = NavTarget::open_or_jump(home, self) else {
            return;
        };

        self.create_tab(target, activate);
    }

    // Splits based on index in self.tabs.
    // Used as an implementation detail for session loading
    // returns false if it fails, and session loading should stop if it happens.
    fn split_existing(&mut self, first: usize, second: usize, orient: Orientation) -> bool {
        if first >= self.tabs.len()
            || second >= self.tabs.len()
            || first == second
            || !self.tabs[first].visible()
            || self.tabs[second].visible()
        {
            error!(
                "Invalid or corrupt saved session. Split {first}:{} / {second}:{} is not valid. \
                 {} total tabs.",
                self.tabs.get(first).map(Tab::visible).unwrap_or_default(),
                self.tabs.get(second).map(Tab::visible).unwrap_or_default(),
                self.tabs.len()
            );
            gui_run(|g| g.error("Invalid or corrupt saved session. Check the logs"));
            return false;
        }

        let Some(paned) = self.tabs[first].split(orient) else {
            warn!("Called split {orient} but pane was too small to split");
            gui_run(|g| g.warning("Could not restore session: window was too small"));
            return false;
        };

        let (left, tab, right) = self.split_around_mut(second);
        tab.attach_pane(left, right, |w| paned.set_end_child(Some(w)));

        true
    }

    pub fn active_split(&mut self, orient: Orientation) {
        let Some(active) = self.active else {
            warn!("Called split {orient} with no panes to split, opening new tab instead");
            gui_run(|g| g.warning("Split called with no panes to split"));
            self.new_tab(true);
            return;
        };

        let active_pos = self.position(active).unwrap();

        let Some(paned) = self.tabs[active_pos].split(orient) else {
            warn!("Called split {orient} but pane was too small to split");
            gui_run(|g| g.warning("Pane is too small to split"));
            return;
        };

        let (new_id, new_index) = self.clone_active().unwrap();
        let (left, tab, right) = self.split_around_mut(new_index);
        tab.attach_pane(left, right, |w| paned.set_end_child(Some(w)));

        self.set_active(new_id);
    }

    pub(super) fn close_tab(&mut self, id: TabId) {
        let index = self.position(id).unwrap();
        let eindex = self.element_position(id).unwrap();


        let tab = &self.tabs[index];
        if tab.visible() {
            // If there's another tab that isn't visible, use it.
            // We want to prioritize the first tab to the left/above in the visible list.
            // If there is no tab at all, we'll also be closing the pane.
            //
            // The number of visible tabs is bounded to a small number by practicality.
            let visible_tabs: Vec<_> =
                self.tabs.iter().filter(|t| t.visible()).map(Tab::id).collect();
            let prioritized = (0..eindex).rev().chain(eindex + 1..self.tab_elements.n_items());

            for e in prioritized {
                let other_id = *self.element(e).imp().tab.get().unwrap();
                if !visible_tabs.contains(&other_id) {
                    self.swap_panes(id, other_id);
                    break;
                }
            }
        }

        // swap_panes will have changed self.active if there was another inactive tab to swap in.
        if Some(id) == self.active {
            if let Some(next) = self.tabs[index].next_of_kin_by_pane() {
                self.set_active(next);
            } else {
                self.active = None;
            }
        }

        self.tabs.swap_remove(index);
        self.tab_elements.remove(eindex);
    }

    // Helper function for common cases.
    fn must_run_tab<T, F: FnOnce(&mut Tab, &[Tab], &[Tab]) -> T>(&mut self, id: TabId, f: F) -> T {
        let Some(index) = self.position(id) else {
            unreachable!("Couldn't find tab for {id:?}");
        };

        let (left, tab, right) = self.split_around_mut(index);
        f(tab, left, right)
    }

    // Tries to run f() against the active tab, if one exists
    fn try_active<T, F: FnOnce(&mut Tab, &[Tab], &[Tab]) -> T>(&mut self, f: F) -> Option<T> {
        self.active.map(|active| self.must_run_tab(active, f))
    }

    // Don't want to expose the Tab methods to Gui, so annoying wrapper functions.
    pub fn active_navigate(&mut self, path: &Path) {
        if let Some(active) = self.active {
            self.navigate(active, path);
            return;
        }

        self.open_tab(path, true);
    }

    pub fn active_jump(&mut self, path: &Path) {
        let Some(jump) = NavTarget::jump(path, self) else {
            return;
        };

        if let Some(active) = self.active {
            self.must_run_tab(active, |t, l, r| {
                t.navigate(l, r, jump);
            });
            return;
        }

        self.create_tab(jump, true);
    }

    pub fn active_forward(&mut self) {
        self.try_active(Tab::forward);
    }

    pub fn active_back(&mut self) {
        self.try_active(Tab::back);
    }

    pub fn active_parent(&mut self) {
        self.try_active(Tab::parent);
    }

    pub fn active_child(&mut self) {
        self.try_active(Tab::child);
    }

    pub fn active_close_tab(&mut self) {
        let Some(active) = self.active else {
            warn!("CloseTab called with no active tab");
            return;
        };

        self.close_tab(active);
    }

    // This is explicitly closing the active pane
    // Doesn't open a new pane to replace it, but does find the next sibling to promote to being
    // active.
    pub fn active_close_pane(&mut self) {
        let Some(active) = self.active else {
            warn!("ClosePane called with no open panes");
            return;
        };

        let index = self.position(active).unwrap();

        if let Some(kin) = self.tabs[index].next_of_kin_by_pane() {
            self.set_active(kin);
        } else {
            self.active = None;
        }

        self.tabs[index].close_pane();
    }

    pub fn active_close_both(&mut self) {
        let Some(old_active) = self.active else {
            warn!("CloseActive called with no open panes");
            return;
        };

        self.active_close_pane();

        let index = self.position(old_active).unwrap();
        let eindex = self.element_position(old_active).unwrap();

        self.tabs.swap_remove(index);
        self.tab_elements.remove(eindex);
    }

    pub fn active_display_mode(&mut self, mode: DisplayMode) {
        let Some(active) = self.active else {
            warn!("Mode called with no open panes");
            return;
        };

        let index = self.position(active).unwrap();
        self.tabs[index].update_display_mode(mode);
    }
}
