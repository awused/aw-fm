use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use gtk::gdk::RGBA;
use gtk::traits::{BoxExt, WidgetExt};
use gtk::Orientation;

use self::tab::Tab;
use crate::com::{DirSnapshot, DisplayMode, EntryObjectSnapshot, SortSettings, Update};

mod tab;

// Maximum number of panes per window
const MAX_PANES: usize = 3;

// Unique highlights for each selected tab per pane.
const PANE_COLOURS: [RGBA; MAX_PANES] = [RGBA::BLUE, RGBA::GREEN, RGBA::RED];


// A unique identifier for tabs.
// Options considered:
//   Incrementing u64:
//      + Easy implementation
//      + Fast, no allocations
//      - Can theoretically overflow
//      - Uniqueness isn't statically guaranteed
//      - Linear searching for tabs
//   Rc<()>:
//      + Easy implementation
//      + Rc::ptr_eq is as fast as comparing u64
//      + Tabs can create their own
//      + Uniqueness is guaranteed provided tabs always construct their own
//      - Wasted heap allocations
//      - Linear searching for tabs
//  Rc<Cell<index>>:
//      + No need for linear searching to find tabs
//      + Rc::ptr_eq is as fast as comparing u64
//      + Uniqueness is guaranteed
//      - Most complicated implementation. Must be manually kept up-to-date.
//      - If the index is ever wrong, weird bugs can happen
//      - Heap allocation
//  UUIDs:
//      - Not really better than a bare u64
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct TabId(u64);

// TODO -- to support multiple panes, move active out of this
// This is tightly couples the Tab implementation, right now.
#[derive(Debug)]
pub(super) struct TabsList {
    tabs: Vec<Tab>,
    active: usize,
    next_id: NonZeroU64,
    tabs_container: gtk::Box,
    pane_container: gtk::Box,
}

impl Deref for TabsList {
    type Target = [Tab];

    fn deref(&self) -> &Self::Target {
        &self.tabs
    }
}

impl DerefMut for TabsList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tabs
    }
}


impl TabsList {
    pub(super) fn new_uninit() -> Self {
        let tabs_container = gtk::Box::new(Orientation::Horizontal, 0);
        tabs_container.set_hexpand(true);


        let pane_container = gtk::Box::new(Orientation::Horizontal, 0);
        pane_container.set_hexpand(true);
        pane_container.set_vexpand(true);

        Self {
            tabs: Vec::new(),
            active: 0, // Temporary for handling a single active tab
            next_id: NonZeroU64::new(1).unwrap(),
            tabs_container,
            pane_container,
        }
    }

    pub(super) fn initialize(&mut self, path: PathBuf) {
        assert!(self.tabs.is_empty());

        let first_tab_element = ();

        self.tabs.push(Tab::new(TabId(0), path, first_tab_element, &[]));
    }

    pub(super) fn layout(&mut self, parent: &gtk::Box) {
        parent.append(&self.tabs_container);
        parent.append(&self.pane_container);

        // Open and activate first and only tab
        self.tabs[0].load(&mut [], &mut []);
        self.tabs[0].display(&self.pane_container);

        // self.clone_tab(0);
        // glib::timeout_add_local_once(Duration::from_secs(5), || {
        //     GUI.with(|g| {
        //         let tabs = g.get().unwrap().tabs.borrow_mut();
        //         let it = tabs.tabs[1].contents.list.item(0).unwrap();
        //         let it = it.downcast::<EntryObject>().unwrap();
        //         let mut e = it.get().clone();
        //         e.name = ParsedString::from(OsString::from("faq"));
        //         it.update(e, SortSettings::default());
        // })
        // });
    }

    fn find_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id() == id)
    }

    fn position(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id() == id)
    }

    fn update_sort(&mut self, id: TabId, settings: SortSettings) {
        // Assume we can't update a tab that doesn't exist -- this should always be called
        // synchronously
        self.find_mut(id).unwrap().update_sort(settings);
    }

    fn update_display_mode(&mut self, id: TabId, mode: DisplayMode) {
        // Assume we can't update a tab that doesn't exist -- this should always be called
        // synchronously
        self.find_mut(id).unwrap().update_display_mode(mode);
    }

    fn split_around_mut(&mut self, index: usize) -> (&mut [Tab], &mut Tab, &mut [Tab]) {
        let (left, rest) = self.split_at_mut(index);
        let ([center], right) = rest.split_at_mut(1) else {
            unreachable!();
        };

        (left, center, right)
    }

    pub(super) fn apply_snapshot(&mut self, snap: DirSnapshot) {
        let first_match = self.iter().position(|t| t.matches_snapshot(&snap.id));
        if let Some(i) = first_match {
            let (_, first_tab, right_tabs) = self.split_around_mut(i);
            first_tab.apply_snapshot(right_tabs, snap);
        }
    }

    pub(super) fn handle_update(&mut self, update: Update) {
        // Handle the case where a tab's directory is deleted out from underneath it.
        // This is handled immediately, even if the directory is still being loaded.
        if let Update::Removed(path) = &update {
            let last_index = None;

            #[allow(clippy::never_loop)]
            // Technically could do better, but
            while let Some(index) = self.tabs.iter().position(|t| t.check_directory_deleted(path)) {
                assert_ne!(Some(index), last_index); // Should never happen
                error!("TODO -- handle directory deletion");
                break;
            }
        }

        let Some(index) = self.tabs.iter().position(|t| t.matches_update(&update)) else {
            return;
        };

        let (_, tab, right_tabs) = self.split_around_mut(index);
        tab.apply_single_update(right_tabs, update);
    }

    fn navigate(&mut self, id: TabId) {
        let index = self.position(id).unwrap();
        let (left, tab, right) = self.split_around_mut(index);
        todo!()
    }

    fn clone_tab(&mut self, index: usize) {
        let id = TabId(self.next_id.get());
        self.next_id = self.next_id.checked_add(1).unwrap();
        let mut new_tab = Tab::cloned(id, &self.tabs[index], ());

        self.tabs.insert(index + 1, new_tab);

        if index < self.active {
            error!("Cloning inactive tab, this shouldn't happen (yet)");
            self.active += 1
        }
    }

    // For now, tabs always open right of the active tab
    fn open_tab(&mut self, path: PathBuf, active_tab: TabId) {
        let id = TabId(self.next_id.get());
        self.next_id = self.next_id.checked_add(1).unwrap();
        let mut new_tab = Tab::new(id, path, (), &self.tabs);

        self.tabs.insert(self.position(active_tab).unwrap() + 1, new_tab);
    }

    pub(super) fn close_tab(&mut self, id: TabId) {
        let index = self.position(id).unwrap();
        // TODO -- If all tabs are active
        if self.tabs.len() == 1 {
            self.open_tab(".".into(), id);
        }

        let tab = &self.tabs[index];
        let pane_index = Some(0); // ???

        self.tabs.remove(index);

        if let Some(pane_index) = pane_index {
            // TODO -- handle case where multiple tabs are active in panes
            // Should grab the next
            let (left, new_active, right) = self.split_around_mut(self.active);
            new_active.load(left, right);
            self.tabs[self.active].display(&self.pane_container);
        }
    }
}
