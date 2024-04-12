use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use dirs::home_dir;
use gtk::gio::ListStore;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{NoSelection, Orientation, SignalListItemFactory};

use super::element::TabElement;
use super::id::TabId;
use super::tab::{ClosedTab, Tab};
use crate::com::{
    DirSnapshot, DisplayMode, EntryObject, SearchSnapshot, SearchUpdate, SortDir, SortMode,
    SortSettings, Update,
};
use crate::database::{SavedSplit, Session, SplitChild};
use crate::gui::clipboard::ClipboardOp;
use crate::gui::main_window::MainWindow;
use crate::gui::tabs::id::next_id;
use crate::gui::tabs::NavTarget;
use crate::gui::{operations, show_error, show_warning, tabs_run};

// For event handlers which cannot be run with the tabs lock being held.
// Assumes the tab still exists since GTK notifies are run synchronously.
pub(super) fn event_run_tab<T, F: FnOnce(&mut Tab, &[Tab], &[Tab]) -> T>(id: TabId, f: F) -> T {
    tabs_run(|t| t.must_run_tab(id, f))
}

#[derive(Debug)]
pub(super) struct Group {
    // Only needed for restoring from closed tabs, for later
    // id: GroupUid,
    // The current parent, subject to change
    pub parent: TabId,
    // _NOT_ including the parent
    pub children: Vec<TabId>,
}

impl Group {
    pub fn size(&self) -> u32 {
        self.children.len() as u32 + 1
    }
}


#[derive(Debug)]
pub enum TabPosition {
    AfterActive,
    After(TabId),
    End,
}


// This is tightly coupled the Tab implementation, right now.
#[derive(Debug)]
pub struct TabsList {
    // Assume the number of tabs is small enough and sane.
    // We iterate over all tabs a lot, and we assume a linear scan is going to be fast enough.
    // If that stops being the case it's annoying but not infeasible to use a map.
    // TODO -- limit number of tabs to 255 or so?
    tabs: Vec<Tab>,

    closed: Vec<ClosedTab>,

    // There is always one active tab or no visible tabs.
    // There may be tabs that are not visible.
    active: Option<TabId>,

    tab_elements: ListStore,
    pane_container: gtk::Box,
}

impl TabsList {
    pub fn new(window: &MainWindow) -> Self {
        let tabs_container = &window.imp().tabs;
        let tab_elements = ListStore::new::<TabElement>();


        let factory = SignalListItemFactory::new();
        factory.connect_bind(|_factory, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let tab = item.item().unwrap().downcast::<TabElement>().unwrap();
            // println!("binding {:?}", tab.imp().tab.get().unwrap());

            if let Some(old_item) = tab.imp().list_item.take() {
                warn!(
                    "Workaround: unbinding already bound tab {:?} before setting parent",
                    tab.imp().tab.get().unwrap()
                );
                if let Some(old_tab) = old_item.item().and_downcast::<TabElement>() {
                    if old_tab.imp().tab.get() == tab.imp().tab.get() {
                        old_item.set_child(None::<&TabElement>);
                    }
                }
                tab.unparent();
            }

            tab.imp().list_item.set(Some(item.clone()));

            item.set_child(Some(&tab));
        });
        factory.connect_unbind(|_factory, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();

            let tab = item.item().unwrap().downcast::<TabElement>().unwrap();
            // println!("unbinding {:?}", tab.imp().tab.get().unwrap());
            if let Some(new_item) = tab.imp().list_item.take() {
                if &new_item != item {
                    warn!(
                        "Unbound tab {:?} from item it was no longer bound to",
                        tab.imp().tab.get()
                    );
                    tab.imp().list_item.set(Some(new_item));
                    return;
                }
            }

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
            closed: Vec::new(),
            active: None,

            tab_elements,
            pane_container: window.imp().panes.clone(),
        }
    }

    pub fn initial_setup(&mut self) {
        assert!(self.tabs.is_empty());

        let Some(target) = NavTarget::initial(self) else {
            return;
        };

        let (tab, element) = Tab::new(next_id(), target, &[], |w| self.pane_container.append(w));

        self.tabs.push(tab);
        self.tab_elements.append(&element);
        self.tabs[0].make_visible(&[], &[]);
        self.set_active(self.tabs[0].id());
    }

    fn position(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id() == id)
    }

    pub(super) fn find(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id() == id)
    }

    pub(super) fn find_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id() == id)
    }

    pub fn update_sort(&mut self, id: TabId, settings: SortSettings) {
        if let Some(t) = self.find_mut(id) {
            t.update_sort(settings)
        }
    }

    pub fn set_active(&mut self, id: TabId) {
        if Some(id) == self.active {
            return;
        }

        // this should be synchronous so should never fail.
        let index = self.position(id).unwrap();

        if let Some(old) = self.active {
            self.find_mut(old).unwrap().set_inactive();
        }

        self.active = Some(id);
        self.tabs[index].set_active();
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

    // Only returns None when after has already been closed
    fn element_insertion_index(&self, after: TabId) -> Option<u32> {
        let Some(tab) = self.find(after) else {
            warn!("Called element_insertion_index with dangling ID {after:?}");
            return None;
        };

        if let Some(g) = tab.multi_tab_group() {
            let b = g.borrow();
            Some(self.element_position(b.parent).unwrap() + b.size())
        } else {
            self.element_position(tab.id()).map(|i| i + 1)
        }
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

    pub fn mark_watching(&mut self, id: Arc<AtomicBool>) {
        if let Some(t) = self.tabs.iter().find(|t| t.matches_watch(&id)) {
            t.mark_watch_started(id);
        }
    }

    pub fn apply_snapshot(&mut self, snap: DirSnapshot) {
        let first_match = self.tabs.iter().position(|t| t.matches_watch(&snap.id.id));
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
                    show_error(&format!(
                        "Unexpected failure loading {path:?} and all parent directories"
                    ));
                    let id = tab.id();
                    self.close_tab(id);
                    continue;
                }
                i += 1
            }
        }

        if let Some(index) = self.tabs.iter().position(|t| t.matches_flat_update(&update)) {
            let (left_tabs, tab, right_tabs) = self.split_around_mut(index);
            tab.flat_update(left_tabs, right_tabs, update);
        }
    }

    // Search data cannot cause updates to other tabs, but it can fail to update EntryObjects
    // with the newest versions.
    // To prevent races, flat tab snapshots always take priority.
    pub fn search_update(&mut self, update: SearchUpdate) {
        if let Some(pos) = self.tabs.iter().position(|t| t.matches_search_update(&update)) {
            // If there are no other matching tabs we can apply mutations even in search tabs
            //
            // NOTE: This must be checked here, even if Sources was piped through from the watcher
            // code, it would allow for races with new searches opening.
            let overlapping_tabs = self
                .tabs
                .iter()
                .enumerate()
                .any(|(i, t)| i != pos && t.overlaps_other_search_update(&update));

            self.tabs[pos].apply_search_update(update, !overlapping_tabs);
        } else {
            warn!("Unmatched search update.");
        }
    }

    pub fn apply_search_snapshot(&mut self, snap: SearchSnapshot) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.matches_search_snapshot(&snap)) {
            t.apply_search_snapshot(snap);
        } else {
            warn!("Unmatched search snapshot.");
        }
    }

    // Unlike with update() above, we know this is going to be the same Arc<>
    pub fn directory_failure(&mut self, path: Arc<Path>) {
        let mut i = 0;
        while let Some(j) = self.tabs.iter().skip(i).position(|t| t.matches_arc(&path)) {
            i += j;
            let (left, tab, right) = self.split_around_mut(i);
            if !tab.handle_directory_deleted(left, right) {
                show_error(&format!(
                    "Unexpected failure loading {path:?} and all parent directories"
                ));
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

    pub fn scroll_to_completed(&mut self, op: &Rc<operations::Operation>) {
        let Some(tab) = self.find_mut(op.tab) else {
            return info!("Not scrolling to completed operation in closed tab {:?}", op.tab);
        };

        tab.scroll_to_completed(op);
    }

    pub fn get_active_dir(&self) -> Option<Arc<Path>> {
        Some(self.find(self.active?).unwrap().dir())
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

        self.active_hide();


        if let Some(group) = self.tabs[index].multi_tab_group() {
            debug!("Switching to non-visible tab {id:?} in group");

            for c in &group.borrow().children {
                self.must_run_tab(*c, Tab::make_visible);
            }

            self.must_run_tab(group.borrow().parent, Tab::make_visible);
        } else {
            debug!("Switching to non-visible tab {id:?}");
            let (left, tab, right) = self.split_around_mut(index);
            tab.make_visible(left, right);
        }

        self.set_active(id);
    }

    // New tab is always at the end of self.tabs
    fn clone_active(&mut self, for_split: bool) -> Option<TabId> {
        let active = self.active?;

        let active_index = self.position(active).unwrap();

        let element_index = if for_split {
            self.element_position(active).unwrap() + 1
        } else {
            // Bit inefficient, extra linear search
            self.element_insertion_index(active).unwrap()
        };

        let (new_tab, element) =
            Tab::cloned(next_id(), &self.tabs[active_index], |w| self.pane_container.append(w));
        let id = new_tab.id();

        self.tabs.push(new_tab);
        self.tab_elements.insert(element_index, &element);

        Some(id)
    }

    pub(super) fn create_tab(
        &mut self,
        position: TabPosition,
        target: NavTarget,
        activate: bool,
    ) -> TabId {
        let (new_tab, element) =
            Tab::new(next_id(), target, &self.tabs, |w| self.pane_container.append(w));

        let id = new_tab.id();
        self.tabs.push(new_tab);

        let after = match position {
            TabPosition::AfterActive => self.active,
            TabPosition::After(id) => Some(id),
            TabPosition::End => None,
        };

        let index = after
            .and_then(|a| self.element_insertion_index(a))
            .unwrap_or_else(|| self.tab_elements.n_items());

        self.tab_elements.insert(index, &element);

        if activate {
            self.switch_active_tab(id)
        }

        id
    }

    pub fn reopen(&mut self) {
        let Some(closed) = self.closed.pop() else {
            return debug!("No closed tab to reopen");
        };
        info!("Reopening closed tab");

        let index = closed.after.and_then(|a| self.element_insertion_index(a)).unwrap_or_default();

        let (new_tab, element) = Tab::reopen(closed, &self.tabs, |w| self.pane_container.append(w));

        let id = new_tab.id();
        self.tabs.push(new_tab);

        self.tab_elements.insert(index, &element);

        self.switch_active_tab(id)
    }

    // For now, tabs always open after the active tab
    // !activate -> background tab
    pub fn open_tab<P: AsRef<Path>>(&mut self, path: P, pos: TabPosition, activate: bool) {
        let Some(target) = NavTarget::open_or_jump(path, self) else {
            return;
        };

        self.create_tab(pos, target, activate);
    }

    // Clones the active tab or opens a new tab to the user's home directory.
    pub fn new_tab(&mut self, activate: bool) {
        if let Some(id) = self.clone_active(false) {
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

        self.create_tab(TabPosition::AfterActive, target, activate);
    }

    // Restores splits from saved groups in a session.
    // Splits are restored from the top down by splitting out the earliest descendent from each
    // branch of the tree.
    // Used as an implementation detail for session loading
    fn restore_split(&mut self, group: &Rc<RefCell<Group>>, split: &SavedSplit) {
        let first = split.start.first_child();
        let second = split.end.first_child();

        if first == second {
            return show_error("Invalid or corrupt session: tab split with itself");
        }

        if first as usize >= self.tabs.len() || second as usize >= self.tabs.len() {
            return show_error("Invalid or corrupt session: splitting non-existent tabs");
        }

        let first = *self.element(first).imp().tab.get().unwrap();
        let first_pos = self.position(first).unwrap();

        let second = *self.element(second).imp().tab.get().unwrap();
        let second_pos = self.position(second).unwrap();

        self.tabs[first_pos].force_group(group);
        self.tabs[second_pos].force_group(group);
        let orient = if split.horizontal { Orientation::Horizontal } else { Orientation::Vertical };

        let paned = self.tabs[first_pos].split(orient, true).unwrap().0;
        self.tabs[second_pos].force_end_child(paned);

        if let SplitChild::Split(split) = &split.start {
            self.restore_split(group, split);
        }

        if let SplitChild::Split(split) = &split.end {
            self.restore_split(group, split);
        }
    }

    pub fn active_split(&mut self, orient: Orientation, tab: Option<TabId>) {
        let Some(active) = self.active else {
            show_warning("Split called with no panes to split");
            return self.new_tab(true);
        };

        if let Some(id) = tab {
            if self.find(id).unwrap().visible() {
                return self.set_active(id);
            }
        }

        let active_pos = self.position(active).unwrap();

        let Some((paned, group)) = self.tabs[active_pos].split(orient, false) else {
            return show_warning("Pane is too small to split");
        };


        let existing = if let Some(tab) = tab {
            let index = self.position(tab).unwrap();
            assert!(!self.tabs[index].visible());

            if self.tabs[index].multi_tab_group().is_some() {
                // Since it's not visible, we know this is not in the same group
                self.remove_tab_from_group(tab);
            }

            let eindex = self.element_position(tab).unwrap();
            let moved = self.tab_elements.item(eindex).unwrap();
            self.tab_elements.remove(eindex);

            let dest = self.element_position(active).unwrap() + 1;
            self.tab_elements.insert(dest, &moved);

            Some((tab, index))
        } else {
            None
        };

        let (id, index) = existing.unwrap_or_else(|| {
            let new = self.clone_active(true).unwrap();
            (new, self.tabs.len() - 1)
        });

        let (left, tab, right) = self.split_around_mut(index);
        tab.add_to_visible_group(left, right, group, paned);

        self.set_active(id);
    }

    pub fn refresh(&mut self) {
        if self.active.is_none() {
            return show_warning("Refresh called without any visible tabs");
        };

        // TODO -- Should be (path, is_search)
        let mut unload_paths = BTreeSet::new();
        let mut unloaded = HashSet::new();

        for t in &mut self.tabs {
            if !t.visible() {
                continue;
            }

            debug!("Unloading visible tab {:?}", t.id());
            unloaded.insert(t.id());
            t.unload_unchecked();
            // TODO -- Should be (path, is_search)
            unload_paths.insert(t.dir());
        }

        'outer: loop {
            for t in &mut self.tabs {
                if unloaded.contains(&t.id()) {
                    continue;
                }

                for p in &unload_paths {
                    if t.overlaps(p) {
                        debug!("Unloading tab {:?} that overlaps with {p:?}", t.id());
                        unloaded.insert(t.id());
                        // TODO -- If this is a search tab, this can leave search Entries alive and
                        // stale in callbacks. Search contents should be dropped synchronously.
                        // t.unload_unchecked(purge_search = true)
                        t.unload_unchecked();

                        if unload_paths.insert(t.dir()) {
                            // Expanded the set of overlapping paths, reconsider earlier tabs.
                            continue 'outer;
                        }
                        break;
                    }
                }
            }

            break 'outer;
        }

        self.reload_visible();
    }

    pub fn refresh_all(&mut self) {
        info!("Refreshing all tabs");
        for t in &mut self.tabs {
            t.unload_unchecked();
        }

        // SAFETY: just unloaded every single tab.
        unsafe {
            // This serves two purposes:
            //
            // Old EntryObjects are dropped in idle callbacks, so they can survive long enough to
            // be resurrected by searches and serve stale values.
            //
            // Flat tabs that open fast enough could resurrect and update old objects. This won't
            // cause stale data to show up, unlike search, but can break scrolling because GTK.
            EntryObject::purge();
        }

        self.reload_visible();
    }

    fn reload_visible(&mut self) {
        for i in 0..self.tabs.len() {
            if !self.tabs[i].visible() {
                continue;
            }

            let (left, t, right) = self.split_around_mut(i);
            t.reload_visible(left, right);
        }
    }

    // Removes a tab from its group and moves its element out of the group.
    // Does not change the active tab directly, but does hide the pane
    fn remove_tab_from_group(&mut self, id: TabId) {
        debug!("Removing {id:?} from its group");

        let index = self.position(id).unwrap();
        let tab = &self.tabs[index];
        let Some(group) = tab.multi_tab_group() else {
            return trace!("Not removing {id:?} from group where it is the only member");
        };

        let mut b = group.borrow_mut();

        let eindex = self.element_position(id).unwrap();

        if b.parent == tab.id() {
            // We don't need to move it, but we do need to promote the next child
            // There must be at least one child.
            let next = self.element(eindex + 1);
            let new_parent = next.imp().tab.get().unwrap();


            let pos = b.children.iter().position(|c| c == new_parent).unwrap();
            b.children.swap_remove(pos);
            b.parent = *new_parent;
            drop(b);

            self.find(*new_parent).unwrap().become_parent();
        } else {
            let pos = b.children.iter().position(|c| *c == id).unwrap();
            b.children.swap_remove(pos);

            let dest = self.element_position(b.parent).unwrap() + b.size();
            drop(b);

            if dest != eindex {
                debug_assert!(dest > eindex);
                // Must be at least two elements
                let moved = self.tab_elements.item(eindex).unwrap();
                self.tab_elements.remove(eindex);
                self.tab_elements.insert(dest, &moved);
            }
        }

        self.tabs[index].reattach_and_hide_pane(&self.pane_container);
        self.tabs[index].leave_group();
    }

    fn hide_single_pane(&mut self, id: TabId) {
        let index = self.position(id).unwrap();

        if let Some(kin) = self.tabs[index].next_of_kin_by_pane() {
            if self.active == Some(id) {
                self.set_active(kin);
            }
            self.remove_tab_from_group(id);
        } else {
            if self.active == Some(id) {
                self.active = None;
            }
            self.tabs[index].hide_pane();
        }
    }

    pub(super) fn close_tab(&mut self, id: TabId) {
        let mut eindex = self.element_position(id).unwrap();
        let index = self.position(id).unwrap();

        let tab = &self.tabs[index];

        let after = if eindex > 0 {
            self.tab_elements
                .item(eindex - 1)
                .and_downcast::<TabElement>()
                .unwrap()
                .imp()
                .tab
                .get()
                .copied()
        } else {
            None
        };

        if let Some(_group) = tab.multi_tab_group() {
            // Real pain here.
            self.hide_single_pane(id);
            // Closing the pane can change this
            eindex = self.element_position(id).unwrap();
        } else if self.tabs.len() > 1 && tab.visible() {
            // ungrouped tab + visible -> must be the active tab
            debug_assert_eq!(Some(id), self.active);
            // If there's another tab that isn't visible, use it.
            // We want to prioritize the first tab to the left/above in the visible list.
            let next = if eindex > 0 { eindex - 1 } else { eindex + 1 };
            let other_id = *self.element(next).imp().tab.get().unwrap();

            let target_tab = self
                .find(other_id)
                .unwrap()
                .multi_tab_group()
                .map_or(other_id, |g| g.borrow().parent);

            self.switch_active_tab(target_tab);
        }

        if Some(id) == self.active {
            self.active = None;
        }

        let closed = self.tabs.swap_remove(index).close(after);
        self.tab_elements.remove(eindex);
        self.closed.push(closed);
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

    pub fn activate(&mut self) {
        let Some(active) = self.active else {
            return warn!("Activate called with no active tab");
        };

        self.find(active).unwrap().activate();
    }

    pub fn active_open_default(&mut self) {
        let Some(active) = self.active else {
            return warn!("OpenDefault called with no active tab");
        };

        self.find(active).unwrap().open_default();
    }

    pub fn active_open_with(&mut self) {
        let Some(active) = self.active else {
            return warn!("OpenWith called with no active tab");
        };

        self.find(active).unwrap().open_with();
    }

    pub fn active_copy(&self) {
        let Some(active) = self.active else {
            return;
        };

        self.find(active).unwrap().set_clipboard(ClipboardOp::Copy);
    }

    pub fn active_cut(&self) {
        let Some(active) = self.active else {
            return;
        };

        self.find(active).unwrap().set_clipboard(ClipboardOp::Cut);
    }

    pub fn active_paste(&self) {
        let Some(active) = self.active else {
            return;
        };

        self.find(active).unwrap().paste();
    }

    // Don't want to expose the Tab methods to Gui, so annoying wrapper functions.
    pub fn active_navigate(&mut self, path: &Path) {
        if let Some(active) = self.active {
            self.navigate(active, path);
            return;
        }

        self.open_tab(path, TabPosition::AfterActive, true);
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

        self.create_tab(TabPosition::AfterActive, jump, true);
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
            return warn!("ClosePane called with no open panes");
        };

        self.hide_single_pane(active);
    }

    pub fn active_hide(&mut self) {
        let Some(active) = self.active else {
            return debug!("HidePanes called with no open panes");
        };

        let active_tab = self.find_mut(active).unwrap();

        let Some(group) = active_tab.multi_tab_group() else {
            active_tab.start_hide();
            active_tab.finish_hide();
            return;
        };

        let mut children = group.borrow().children.clone();
        let mut to_finish = Vec::with_capacity(children.len());

        for (i, t) in self.tabs.iter_mut().enumerate() {
            let Some(pos) = children.iter().position(|id| *id == t.id()) else {
                continue;
            };

            children.swap_remove(pos);
            to_finish.push(i);
            t.start_hide();

            if children.is_empty() {
                break;
            };
        }

        let t = self.find_mut(group.borrow().parent).unwrap();
        t.start_hide();
        t.finish_hide();

        for i in to_finish {
            self.tabs[i].finish_hide();
        }
    }

    pub fn active_close_both(&mut self) {
        let Some(old_active) = self.active else {
            return warn!("CloseActive called with no open panes");
        };

        self.hide_single_pane(old_active);

        let index = self.position(old_active).unwrap();
        let eindex = self.element_position(old_active).unwrap();

        self.tabs.swap_remove(index);
        self.tab_elements.remove(eindex);
    }

    pub fn active_display_mode(&mut self, mode: DisplayMode) {
        let Some(active) = self.active else {
            return warn!("Display called with no open panes");
        };

        self.find_mut(active).unwrap().update_display_mode(mode);
    }

    pub fn active_sort_mode(&mut self, mode: SortMode) {
        let Some(active) = self.active else {
            return warn!("SortBy called with no open panes");
        };

        self.find_mut(active).unwrap().update_sort_mode(mode);
    }

    pub fn active_sort_dir(&mut self, dir: SortDir) {
        let Some(active) = self.active else {
            return warn!("SortDir called with no open panes");
        };

        self.find_mut(active).unwrap().update_sort_dir(dir);
    }

    pub fn active_search(&mut self, query: &str) {
        let Some(active) = self.active else {
            return warn!("Search called with no open panes");
        };

        self.find_mut(active).unwrap().search(query.to_owned());
    }

    pub fn active_trash(&self) {
        let Some(active) = self.active else {
            return warn!("Trash called with no open panes");
        };

        self.find(active).unwrap().trash();
    }

    pub fn active_delete(&self) {
        let Some(active) = self.active else {
            return warn!("Delete called with no open panes");
        };

        self.find(active).unwrap().delete();
    }

    pub fn active_rename(&self) {
        let Some(active) = self.active else {
            return warn!("Rename called with no open panes");
        };

        self.find(active).unwrap().rename();
    }

    pub fn active_properties(&self) {
        let Some(active) = self.active else {
            return warn!("Properties called with no open panes");
        };

        self.find(active).unwrap().properties();
    }

    pub fn active_create(&self, folder: bool) {
        let Some(active) = self.active else {
            return warn!(
                "New{} called with no open panes",
                if folder { "Folder" } else { "File" }
            );
        };

        self.find(active).unwrap().create(folder);
    }

    pub fn reorder(&mut self, source: TabId, dest: TabId, mut after: bool) {
        assert!(source != dest);
        let Some(src_idx) = self.position(source) else {
            return;
        };
        let Some(dest_idx) = self.position(dest) else {
            return;
        };

        let mut sg = self.tabs[src_idx].multi_tab_group();
        let dg = self.tabs[dest_idx].multi_tab_group();

        if sg.as_ref().is_some_and(|sg| dg.as_ref().is_some_and(|dg| Rc::ptr_eq(sg, dg))) {
            return error!("TODO -- decide how/if to handle moves inside the same group");
        }


        // We'll move everything before or after the group
        let mut dest_index = if let Some(dg) = dg {
            if dest != dg.borrow().parent {
                after = true;
            }

            let dest = dg.borrow().parent;
            if after {
                self.element_position(dest).unwrap() + dg.borrow().size()
            } else {
                self.element_position(dest).unwrap()
            }
        } else {
            self.element_position(dest).unwrap() + if after { 1 } else { 0 }
        };

        if sg.as_ref().is_some_and(|sg| sg.borrow().parent != source) {
            // Not the parent, remove from current group
            // This may change src_index, but we haven't read that that.
            // Will not change dest_index since they're in different groups and this will, at
            // worst, reorder tabs within source's group.
            if self.tabs[src_idx].visible() {
                self.hide_single_pane(source);
            } else {
                self.remove_tab_from_group(source);
            }
            sg = None;
        }

        let mut src_index = self.element_position(source).unwrap();

        if src_index < dest_index {
            dest_index -= 1;
        }

        // If there is still a group, we're moving the entire thing
        for _ in 0..sg.map_or(1, |sg| sg.borrow().size()) {
            let item = self.element(src_index);
            self.tab_elements.remove(src_index);
            self.tab_elements.insert(dest_index, &item);

            if src_index > dest_index {
                src_index += 1;
                dest_index += 1;
            }
        }
    }

    pub fn get_session(&self) -> Option<Session> {
        if self.tabs.is_empty() {
            info!("No tabs to save as session");
            return None;
        }

        let tabs: AHashMap<_, _> = self.tabs.iter().map(|t| (t.id(), t)).collect();

        let mut numbered_ids = AHashMap::new();

        let paths = self
            .tab_elements
            .iter::<TabElement>()
            .map(Result::unwrap)
            .enumerate()
            .map(|(n, el)| {
                let id = *el.imp().tab.get().unwrap();
                numbered_ids.insert(id, n as u32);
                tabs[&id].dir()
            })
            .collect();

        let mut groups = Vec::new();

        for t in &self.tabs {
            if t.multi_tab_group().is_some_and(|g| g.borrow().parent == t.id()) {
                groups.push(t.save_group(&numbered_ids));
            }
        }

        Some(Session { paths, groups })
    }

    pub fn load_session(&mut self, session: Session) {
        // Take advantage of existing data if we can.
        let old_tabs = self.tabs.len();
        for path in session.paths {
            let target = NavTarget::assume_dir(path);
            self.create_tab(TabPosition::End, target, false);
        }

        for n in 0..old_tabs {
            // We do swap_remove so this is fine.
            let tab = &mut self.tabs[old_tabs - n - 1];
            if tab.visible() {
                tab.hide_pane();
            }
            let id = tab.id();
            self.close_tab(id);
        }

        for saved in session.groups {
            if saved.parent as usize >= self.tabs.len() {
                show_error("Invalid or corrupt session: parent of group doesn't exist");
                continue;
            }

            let parent = self.element(saved.parent).tab();
            let group = self.find_mut(parent).unwrap().get_or_start_group();

            self.restore_split(&group, &saved.split);
        }
    }

    pub fn get_env(&self) -> Vec<(String, OsString)> {
        let mut env: Vec<(String, OsString)> = Vec::new();

        if let Some(active) = self.active {
            let tab = self.find(active).unwrap();
            tab.env_vars("AWFM_CURRENT_TAB", &mut env);
            env.push(("AWFM_SELECTION".to_owned(), tab.selection_env_str()));

            if let Some(index) = self.element_position(active) {
                if index > 0 {
                    let prev = self.element(index - 1);
                    let prev = *prev.imp().tab.get().unwrap();
                    self.find(prev).unwrap().env_vars("AWFM_PREV_TAB", &mut env);
                }
                if index + 1 < self.tab_elements.n_items() {
                    let next = self.element(index + 1);
                    let next = *next.imp().tab.get().unwrap();
                    self.find(next).unwrap().env_vars("AWFM_NEXT_TAB", &mut env);
                }
            }
        } else if self.tab_elements.n_items() > 0 {
            let next = self.element(0);
            let next = *next.imp().tab.get().unwrap();
            self.find(next).unwrap().env_vars("AWFM_NEXT_TAB", &mut env);
        }

        env
    }

    pub fn idle_unload(&mut self) {
        let mut visible = AHashSet::new();
        let mut unload = AHashSet::new();

        for t in &self.tabs {
            let d = t.dir();

            if t.visible() {
                unload.remove(&d);
                visible.insert(d);
            } else if !t.unloaded() && !visible.contains(&d) {
                unload.insert(d);
            }
        }

        for t in &mut self.tabs {
            if unload.contains(&t.dir()) {
                debug!("Unloading {:?}", t.id());
                // We're going to remove all tabs matching this directory, so no need to
                // coordinate.
                t.unload_unchecked();
            }
        }
    }
}
