use std::cmp::Ordering;
use std::time::Instant;

use gtk::gio::{ListModel, ListStore};
use gtk::glib::{self, ControlFlow, Object, Priority};
use gtk::prelude::*;
use gtk::{CustomFilter, FilterListModel, MultiSelection};

use super::PartiallyAppliedUpdate;
use crate::com::{Entry, EntryObject, EntryObjectSnapshot, SearchSnapshot, SortSettings};

pub struct Contents {
    list: ListStore,
    // TODO -- non-optional once we support display hidden/true/false
    pub filtered: Option<FilterListModel>,
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

impl Drop for Contents {
    fn drop(&mut self) {
        // Drops things in small batches in callbacks
        self.clear(self.sort);
    }
}

pub struct TotalPos(u32);

impl Contents {
    pub fn new(sort: SortSettings) -> Self {
        let list = ListStore::new::<EntryObject>();
        let selection = MultiSelection::new(Some(list.clone()));
        Self {
            list,
            filtered: None,
            sort,
            stale: false,
            selection,
        }
    }

    pub fn search_from(flat: &Self) -> (Self, CustomFilter) {
        let list = ListStore::new::<EntryObject>();
        let filter = CustomFilter::new(|_eo| false);
        let filtered = FilterListModel::new(Some(list.clone()), Some(filter.clone()));
        let selection = MultiSelection::new(Some(filtered.clone()));

        // Causes annoying flickering, but as an optimization incremental mode is used occasionally
        // elsewhere.
        //
        // No, no incremental anywhere: https://gitlab.gnome.org/GNOME/gtk/-/issues/5989
        // filtered.set_incremental(false);

        let mut s = Self {
            list,
            filtered: Some(filtered),
            sort: flat.sort,
            stale: false,
            selection,
        };

        s.clone_from(flat, flat.sort);

        (s, filter)
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

    pub fn apply_snapshot(&mut self, snap: EntryObjectSnapshot) {
        if snap.id.kind.initial() {
            if self.stale {
                debug!("Clearing out stale items");
            }
            self.clear(self.sort);
            debug_assert!(self.list.n_items() == 0);
        }
        assert!(!self.stale);


        let start = Instant::now();
        if self.list.n_items() == 0 {
            let mut entries = snap.entries;
            // This is extremely fast if sort settings haven't changed.
            entries.sort_by(|a, b| a.get().cmp(&b.get(), self.sort));
            self.list.extend(entries);
        } else {
            self.list.extend(snap.entries);
            self.list.sort(self.sort.comparator());
        }

        // It can be marginally faster to detach, sort, and reattach the list when there is no
        // filter, but selection gets clobbered.
        trace!("Inserted and sorted {:?} items in {:?}", self.list.n_items(), start.elapsed());
    }

    pub fn re_sort_for_search(&mut self) {
        let start = Instant::now();
        self.list.sort(self.sort.comparator());
        trace!(
            "Re-sorted {} items in search pane in {:?}",
            self.list.n_items(),
            start.elapsed()
        );
    }

    pub fn add_flat_elements_for_search(&mut self, snap: EntryObjectSnapshot) {
        let start = Instant::now();
        if self.list.n_items() == 0 {
            let mut entries = snap.entries;
            entries.sort_by(|a, b| a.get().cmp(&b.get(), self.sort));
            self.list.extend(entries);
        } else {
            self.list.extend(snap.entries);
            self.list.sort(self.sort.comparator());
        }
        trace!("Sorted {:?} search items in {:?}", self.list.n_items(), start.elapsed());
    }

    pub fn apply_search_snapshot(&mut self, snap: SearchSnapshot) {
        let start = Instant::now();
        self.list.extend(snap.into_entries());
        self.list.sort(self.sort.comparator());
        trace!(
            "Sorted {:?} items for search snapshot in {:?}",
            self.list.n_items(),
            start.elapsed()
        );
    }

    // ListStore isn't even a flat array, so binary searching isn't much worse even at small sizes.
    fn bsearch<L: IsA<ListModel>>(entry: &Entry, sort: SortSettings, list: &L) -> Option<u32> {
        let mut start = 0;
        let mut end = list.n_items();

        if end == 0 {
            return None;
        }

        while start < end {
            let mid = start + (end - start) / 2;

            let obj = list.item(mid).unwrap().downcast::<EntryObject>().unwrap();

            let inner = obj.get();
            if inner.abs_path == entry.abs_path {
                // The equality check below may fail even with abs_path being equal due to updates.
                return Some(mid);
            }

            match entry.cmp(&inner, sort) {
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

    // Look for the location in the unfiltered list.
    // The list may no longer be sorted because of an update, but except for the single update to
    // "entry's corresponding EntryObject" the list will be sorted.
    // So using the old version of the entry we can search for the old location.
    pub fn total_position_by_sorted(&self, entry: &Entry) -> Option<TotalPos> {
        assert!(!self.stale);

        Self::bsearch(entry, self.sort, &self.list).map(TotalPos)
    }

    // Look for the location in the filtered list.
    // The list may no longer be sorted because of an update, but except for the single update to
    // "entry's corresponding EntryObject" the list will be sorted.
    // So using the old version of the entry we can search for the old location.
    pub fn filtered_position_by_sorted(&self, entry: &Entry) -> Option<u32> {
        assert!(!self.stale);
        if let Some(filtered) = &self.filtered {
            // https://gitlab.gnome.org/GNOME/gtk/-/issues/5989
            // TODO [incremental]
            // if filtered.is_incremental() {
            //     // Must unset incremental to get an accurate position.
            //     filtered.set_incremental(false);
            // }
            Self::bsearch(entry, self.sort, filtered)
        } else {
            Self::bsearch(entry, self.sort, &self.list)
        }
    }

    pub fn reinsert_updated(&mut self, pos: TotalPos, new: &EntryObject, old: &Entry) {
        let t_pos = pos.0;
        assert!(!self.stale);
        let comp = self.sort.comparator();

        if (t_pos == 0
            || comp(&self.list.item(t_pos - 1).unwrap(), new.upcast_ref::<Object>()).is_lt())
            && (t_pos + 1 == self.list.n_items()
                || comp(&self.list.item(t_pos + 1).unwrap(), new.upcast_ref::<Object>()).is_gt())
        {
            trace!("Did not reinsert item as it was already in the correct position");
            return;
        }

        let was_selected = if self.filtered.is_some() {
            if let Some(f_pos) = self.filtered_position_by_sorted(old) {
                self.selection.is_selected(f_pos)
            } else {
                false
            }
        } else {
            self.selection.is_selected(t_pos)
        };

        // After removing the lone updated item, the list is guaranteed to be sorted.
        // So we can reinsert it much more cheaply than sorting the entire list again.
        self.list.remove(t_pos);
        let new_t_pos = self.list.insert_sorted(new, comp);


        if was_selected {
            if self.filtered.is_some() {
                if let Some(f_pos) = self.filtered_position_by_sorted(old) {
                    self.selection.select_item(f_pos, false);
                }
            } else {
                self.selection.select_item(new_t_pos, false);
            }
        }
    }

    pub fn insert(&mut self, new: &EntryObject) {
        assert!(!self.stale);
        debug_assert!(self.total_position_by_sorted(&new.get()).is_none());
        self.list.insert_sorted(new, self.sort.comparator());
    }

    pub(super) fn finish_update(&mut self, update: &PartiallyAppliedUpdate) {
        assert!(!self.stale);

        match update {
            PartiallyAppliedUpdate::Mutate(old, new) => {
                let old_position = self.total_position_by_sorted(old).unwrap();
                self.reinsert_updated(old_position, new, old);
            }
            PartiallyAppliedUpdate::Insert(new) => {
                self.list.insert_sorted(new, self.sort.comparator());
            }
            PartiallyAppliedUpdate::Delete(old) => {
                let i = self.total_position_by_sorted(&old.get()).unwrap();
                self.remove(i);
            }
        }
    }

    pub fn remove(&mut self, pos: TotalPos) {
        self.list.remove(pos.0);
    }

    pub fn sort(&mut self, sort: SortSettings) {
        if self.stale {
            return self.clear(sort);
        }

        if self.sort == sort {
            return;
        }

        self.sort = sort;
        let start = Instant::now();
        self.list.sort(sort.comparator());
        trace!("Sorted {:?} items in {:?}", self.list.n_items(), start.elapsed());
    }

    pub fn clear(&mut self, sort: SortSettings) {
        self.stale = false;
        self.sort = sort;

        if self.list.n_items() == 0 {
            return;
        }

        let new_list = ListStore::new::<EntryObject>();

        if let Some(filtered) = &self.filtered {
            filtered.set_model(Some(&new_list));
        } else {
            self.selection.set_model(Some(&new_list));
        }

        let old_list = std::mem::replace(&mut self.list, new_list);

        // Dropping in one go can be glacially slow due to callbacks and notifications.
        // Especially if we're cleaning up the final references to a lot of items with thumbnails.
        // 130ms for ~40k items
        // 160ms for 50k
        // More with thumbnails
        let start = Instant::now();
        let total = old_list.n_items();
        glib::idle_add_local_full(Priority::LOW, move || {
            if old_list.n_items() <= 1000 {
                trace!("Finished dropping {total} items in {:?}", start.elapsed());
                return ControlFlow::Break;
            }
            old_list.splice(0, 1000, &[] as &[EntryObject]);
            ControlFlow::Continue
        });
    }

    pub fn mark_stale(&mut self) {
        debug!("Marking {self:?} as stale");
        self.stale = true;
    }
}
