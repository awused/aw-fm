use std::cmp::Ordering;
use std::time::Instant;

use gtk::gio::ListStore;
use gtk::glib::Object;
use gtk::prelude::{Cast, ListModelExt, ListModelExtManual, StaticType};
use gtk::traits::SelectionModelExt;
use gtk::MultiSelection;

use super::PartiallyAppliedUpdate;
use crate::com::{DirSettings, Entry, EntryObject, EntryObjectSnapshot, SortSettings};

pub struct Contents {
    list: ListStore,
    sort: SortSettings,
    // Stale means the entries in this list are for the previous directory.
    // They remain visible for up to the ~1s it takes for a snapshot of a slow directory to arrive.
    stale: bool,
    pub selection: MultiSelection,
}

impl std::fmt::Debug for Contents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[Contents: {}]", self.list.n_items())
    }
}

impl Contents {
    pub fn new(sort: SortSettings) -> Self {
        let list = ListStore::new(EntryObject::static_type());
        let selection = MultiSelection::new(Some(list.clone()));
        Self { list, sort, stale: false, selection }
    }

    pub fn clone_from(&mut self, source: &Self, sort: SortSettings) {
        self.clear(sort);

        if !source.stale {
            self.list.extend(source.list.iter::<EntryObject>().flatten());
        } else {
            warn!("Cloning from stale tab Contents. This is unusual but not necessarily wrong.");
        }

        if sort != source.sort {
            self.list.sort(sort.comparator());
        }
    }

    pub fn apply_snapshot(&mut self, snap: EntryObjectSnapshot, sort: SortSettings) {
        if snap.id.kind.initial() {
            if self.stale {
                debug!("Clearing out stale items");
            }
            self.clear(sort);
            debug_assert!(self.list.n_items() == 0);
        }
        assert!(!self.stale);
        debug_assert_eq!(self.sort, sort);

        self.list.extend(snap.entries.into_iter());
        let start = Instant::now();
        self.list.sort(self.sort.comparator());
        trace!("Sorted {:?} items in {:?}", self.list.n_items(), start.elapsed());

        // let a = self.contents.list.item(0).unwrap();
        // let settings = self.settings;
        // glib::timeout_add_local_once(Duration::from_secs(5), move || {
        //     let b = a.downcast::<EntryObject>().unwrap();
        //     let mut c = b.get().clone();
        //     c.name = ParsedString::from(OsString::from("asdf"));
        //     error!("Updating file for no reason");
        //     b.update(c, settings.sort);
        // });
    }

    // Look for the location in the list.
    // The list may no longer be sorted because of an update, but except for the single update to
    // "entry's corresponding EntryObject" the list will be sorted.
    // So using the old version of the entry we can search for the old location.
    pub fn position_by_sorted_entry(&self, entry: &Entry) -> Option<u32> {
        assert!(!self.stale);
        let mut start = 0;
        let mut end = self.list.n_items();

        if end == 0 {
            return None;
        }

        while start < end {
            let mid = start + (end - start) / 2;

            let obj = self.list.item(mid).unwrap().downcast::<EntryObject>().unwrap();

            let inner = obj.get();
            if inner.abs_path == entry.abs_path {
                // The equality check below may fail even with abs_path being equal due to updates.
                return Some(mid);
            }

            match entry.cmp(&inner, self.sort) {
                Ordering::Equal => unreachable!(),
                Ordering::Less => end = mid,
                Ordering::Greater => start = mid + 1,
            }
        }

        // All list stores must always be sorted modulo individual updates by the time updates are
        // being handled.
        //
        // The item is not present.
        None
    }

    pub fn reinsert_updated(&mut self, i: u32, new: &EntryObject) {
        assert!(!self.stale);
        let comp = self.sort.comparator();

        if (i == 0 || comp(&self.list.item(i - 1).unwrap(), new.upcast_ref::<Object>()).is_lt())
            && (i == self.list.n_items() - 1
                || comp(&self.list.item(i + 1).unwrap(), new.upcast_ref::<Object>()).is_gt())
        {
            debug!("Did not reinsert item as it was already in the correct position");
            return;
        }

        let was_selected = self.selection.is_selected(i);

        // After removing the lone updated item, the list is guaranteed to be sorted.
        // So we can reinsert it much more cheaply than sorting the entire list again.
        self.list.remove(i);
        let new_idx = self.list.insert_sorted(new, comp);

        if was_selected {
            self.selection.select_item(new_idx, false);
        }
    }

    pub fn insert(&mut self, new: &EntryObject, sort: SortSettings) {
        assert!(!self.stale);
        debug_assert!(self.position_by_sorted_entry(&new.get()).is_none());
        self.list.insert_sorted(new, self.sort.comparator());
    }

    pub(super) fn finish_update(&mut self, update: &PartiallyAppliedUpdate) {
        assert!(!self.stale);

        match update {
            PartiallyAppliedUpdate::Mutate(old, new) => {
                let old_position = self.position_by_sorted_entry(old).unwrap();
                self.reinsert_updated(old_position, new);
            }
            PartiallyAppliedUpdate::Insert(new) => {
                self.list.insert_sorted(new, self.sort.comparator());
            }
            PartiallyAppliedUpdate::Delete(old) => {
                let i = self.position_by_sorted_entry(&old.get()).unwrap();
                self.list.remove(i);
            }
        }
    }

    // Only remove this if it's really present.
    pub fn remove(&mut self, pos: u32) {
        self.list.remove(pos);
    }

    pub fn sort(&mut self, sort: SortSettings) {
        if self.stale {
            self.clear(sort);
        }

        self.sort = sort;
        let start = Instant::now();
        self.list.sort(sort.comparator());
        trace!("Sorted {:?} items in {:?}", self.list.n_items(), start.elapsed());
    }

    pub fn clear(&mut self, sort: SortSettings) {
        self.stale = false;
        self.list.remove_all();
        self.sort = sort;
    }

    pub fn mark_stale(&mut self) {
        debug!("Marking {self:?} as stale");
        self.stale = true;
    }
}
