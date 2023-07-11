mod string_cell;

use std::time::Instant;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use gtk::glib::Object;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{
    gio, glib, ColumnView, ColumnViewColumn, ColumnViewSorter, CustomSorter, GridView, ListView,
    MultiSelection,
};

use self::string_cell::StringCell;
use crate::com::{DirMetadata, EntryKind, EntryObject};

// Does absolutely nothing, except exist
const fn dummy_sort_fn(a: &Object, b: &Object) -> gtk::Ordering {
    gtk::Ordering::Equal
}


pub(super) fn new(selection: &MultiSelection) -> ColumnView {
    let dummy_sorter = CustomSorter::new(dummy_sort_fn);

    let icon_factory = gtk::SignalListItemFactory::new();

    icon_factory.connect_setup(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let cell = StringCell::new();
        item.set_child(Some(&cell));
    });

    icon_factory.connect_bind(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();
        let child = item.child().unwrap().downcast::<StringCell>().unwrap();
        let text = entry.name.to_string_lossy();
        child.set_text("todo", None);
    });

    let icon_column = ColumnViewColumn::new(None, Some(icon_factory));
    icon_column.set_expand(true);

    let name_factory = gtk::SignalListItemFactory::new();

    name_factory.connect_setup(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let cell = StringCell::new();
        item.set_child(Some(&cell));
    });

    name_factory.connect_bind(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();
        let child = item.child().unwrap().downcast::<StringCell>().unwrap();
        let text = entry.name.to_string_lossy();
        child.set_text(&text, Some(&text));
    });

    let name_column = ColumnViewColumn::new(Some("Name"), Some(name_factory));
    name_column.set_expand(true);
    name_column.set_sorter(Some(&dummy_sorter));


    let size_factory = gtk::SignalListItemFactory::new();

    size_factory.connect_setup(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let cell = StringCell::new();
        cell.align_end(9);
        item.set_child(Some(&cell));
    });

    size_factory.connect_bind(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();
        let child = item.child().unwrap().downcast::<StringCell>().unwrap();
        let text = match entry.kind {
            EntryKind::File { size } => humansize::format_size(size, humansize::WINDOWS),
            EntryKind::Directory { contents } => format!("{contents}"),
            EntryKind::Uninitialized => unreachable!(),
        };
        child.set_text(&text, None);
    });

    let size_column = ColumnViewColumn::new(Some("Size"), Some(size_factory));
    size_column.set_sorter(Some(&dummy_sorter));
    size_column.set_fixed_width(110);


    let modified_factory = gtk::SignalListItemFactory::new();

    modified_factory.connect_setup(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let cell = StringCell::new();
        item.set_child(Some(&cell));
    });

    modified_factory.connect_bind(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();
        let child = item.child().unwrap().downcast::<StringCell>().unwrap();

        // Only use seconds for columns
        let localtime = Local.timestamp_opt(entry.mtime.sec as i64, 0).unwrap();
        let text = localtime.format("%Y-%m-%d %H:%M:%S");
        child.set_text(&format!("{text}"), None);
    });

    let modified_column = ColumnViewColumn::new(Some("Date Modified"), Some(modified_factory));
    modified_column.set_sorter(Some(&dummy_sorter));
    modified_column.set_fixed_width(200);
    // Icon -- combine into name column?
    // Name
    // Size
    // Modified date

    // let name_column = ColumnViewColumn::new(
    // column_view.append_column(column);


    // let grid = GridView::new(Some(selection.clone()), Some(factory));
    // let grid = ListView::new(Some(selection.clone()), Some(factory));
    // grid.set_min_columns(2);
    // grid.set_max_columns(2);
    // grid.set_enable_rubberband(true);
    // grid.set_vexpand(true);

    let column_view = ColumnView::new(Some(selection.clone()));
    column_view.set_show_column_separators(true);
    column_view.set_enable_rubberband(true);
    column_view.set_vexpand(true);
    column_view.set_reorderable(false);

    column_view.append_column(&icon_column);
    column_view.append_column(&name_column);
    column_view.append_column(&size_column);
    column_view.append_column(&modified_column);

    let sorter = column_view.sorter().unwrap().downcast::<ColumnViewSorter>().unwrap();
    sorter.connect_changed(|a, b| {
        println!("{:?}", a.n_sort_columns());
        println!("{:?}", a.nth_sort_column(0));
        println!("Sorter changed {a:?}");
    });
    let cview = column_view.clone();
    column_view.connect_sorter_notify(|a| {
        println!("Sorter notify {a:?}");
    });


    column_view
}
