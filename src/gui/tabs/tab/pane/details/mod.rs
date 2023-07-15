mod string_cell;

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use gtk::glib::Object;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{
    gio, glib, ColumnView, ColumnViewColumn, ColumnViewSorter, CustomSorter, GridView, ListView,
    MultiSelection, ScrolledWindow, SignalListItemFactory,
};

use self::string_cell::{EntryString, StringCell};
use crate::com::{
    DirSettings, DisplayMode, Entry, EntryKind, EntryObject, SortDir, SortMode, SortSettings,
};
use crate::gui::tabs::tab::Tab;
use crate::gui::tabs::TabId;
use crate::gui::GUI;

const NAME: &str = "Name";
const SIZE: &str = "Size";
const DATE_MODIFIED: &str = "Date Modified";


// Does absolutely nothing, except exist
const fn dummy_sort_fn(a: &Object, b: &Object) -> gtk::Ordering {
    gtk::Ordering::Equal
}

fn unwrap_item(obj: &Object) -> (StringCell, EntryObject) {
    let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
    let child = item.child().unwrap().downcast::<StringCell>().unwrap();
    let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();
    (child, entry)
}

fn setup_string_binds(factory: &SignalListItemFactory) {
    factory.connect_bind(move |_factory, item| {
        let (child, entry) = unwrap_item(item);
        child.bind(&entry);
    });

    factory.connect_unbind(move |_factory, item| {
        let (child, entry) = unwrap_item(item);
        child.unbind(&entry);
    });
}

fn set_sort(column_view: &ColumnView, sort: SortSettings) {
    let column_name = match sort.mode {
        SortMode::Name => Some(NAME),
        SortMode::MTime => Some(DATE_MODIFIED),
        SortMode::Size => Some(SIZE),
        SortMode::BTime => None,
    };
    let binding = column_name.and_then(|name| {
        column_view
            .columns()
            .iter()
            .flatten()
            .filter(|col: &ColumnViewColumn| col.title().is_some())
            .find(|col: &ColumnViewColumn| col.title().unwrap().as_str() == name)
    });
    let column = binding.as_ref();

    column_view.sort_by_column(column, sort.direction.into());
}

#[derive(Debug)]
pub(super) struct DetailsView {
    column_view: ColumnView,
    current_sort: Rc<Cell<SortSettings>>,
}


impl DetailsView {
    pub(super) fn new(
        scroller: &ScrolledWindow,
        tab_id: TabId,
        settings: DirSettings,
        selection: &MultiSelection,
    ) -> Self {
        let dummy_sorter = CustomSorter::new(dummy_sort_fn);

        let icon_factory = SignalListItemFactory::new();
        icon_factory.connect_setup(move |_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let cell = StringCell::new(EntryString::Name);
            item.set_child(Some(&cell));
        });
        setup_string_binds(&icon_factory);

        let icon_column = ColumnViewColumn::new(None, Some(icon_factory));
        icon_column.set_expand(true);


        let name_factory = SignalListItemFactory::new();
        name_factory.connect_setup(move |_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let cell = StringCell::new(EntryString::Name);
            item.set_child(Some(&cell));
        });
        setup_string_binds(&name_factory);

        let name_column = ColumnViewColumn::new(Some(NAME), Some(name_factory));
        name_column.set_expand(true);
        name_column.set_sorter(Some(&dummy_sorter));


        let size_factory = SignalListItemFactory::new();
        size_factory.connect_setup(move |_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let cell = StringCell::new(EntryString::Size);
            cell.align_end(9);
            item.set_child(Some(&cell));
        });
        setup_string_binds(&size_factory);

        let size_column = ColumnViewColumn::new(Some(SIZE), Some(size_factory));
        size_column.set_sorter(Some(&dummy_sorter));
        size_column.set_fixed_width(110);


        let modified_factory = SignalListItemFactory::new();
        modified_factory.connect_setup(move |_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let cell = StringCell::new(EntryString::Modified);
            item.set_child(Some(&cell));
        });
        setup_string_binds(&modified_factory);

        let modified_column = ColumnViewColumn::new(Some(DATE_MODIFIED), Some(modified_factory));
        modified_column.set_sorter(Some(&dummy_sorter));
        modified_column.set_fixed_width(200);


        let column_view = ColumnView::new(Some(selection.clone()));
        column_view.set_show_column_separators(true);
        column_view.set_enable_rubberband(true);
        column_view.set_vexpand(true);
        column_view.set_reorderable(false);

        column_view.append_column(&icon_column);
        column_view.append_column(&name_column);
        column_view.append_column(&size_column);
        column_view.append_column(&modified_column);

        set_sort(&column_view, settings.sort);

        let current_sort = Rc::new(Cell::new(settings.sort));

        let sorter = column_view.sorter().unwrap().downcast::<ColumnViewSorter>().unwrap();
        let cur_sort = current_sort.clone();
        sorter.connect_changed(move |sorter, b| {
            let (col, direction) = sorter.nth_sort_column(0);
            println!("{col:?} {direction:?}");
            let Some(col) = col else {
                return;
            };
            let col = col.title().unwrap();

            let mode = match col.as_str() {
                NAME => SortMode::Name,
                SIZE => SortMode::Size,
                DATE_MODIFIED => SortMode::MTime,
                _ => unreachable!(),
            };

            let direction: SortDir = direction.into();
            let new_sort = SortSettings { mode, direction };

            if cur_sort.get() != new_sort {
                cur_sort.set(new_sort);
                trace!("Sorter changed: {col:?} {direction:?}");
                GUI.with(|g| g.get().unwrap().tabs.borrow_mut().update_sort(tab_id, new_sort))
            }
        });

        column_view.connect_destroy(|_| error!("TODO -- remove me: details confirmed destroyed"));


        scroller.set_child(Some(&column_view));

        Self { column_view, current_sort }
    }

    pub(super) fn update_sort(&self, sort: SortSettings) {
        if self.current_sort.get() == sort {
            return;
        }

        self.current_sort.set(sort);

        set_sort(&self.column_view, sort);
    }
}
