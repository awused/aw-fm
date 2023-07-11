mod icon_tile;

use std::time::Instant;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib, GridView, ListView, MultiSelection};

use self::icon_tile::IconTile;
use crate::com::EntryObject;

// pub(super) fn new(selection: &MultiSelection) -> ListView {
pub(super) fn new(selection: &MultiSelection) -> GridView {
    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(move |_factory, item| {
        // In gtk4 < 4.8, you don't need the following line
        // as gtk used to pass GtkListItem directly. In order to make that API
        // generic for potentially future new APIs, it was switched to taking a GObject in 4.8
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let row = IconTile::new();
        item.set_child(Some(&row));
    });

    // the bind stage is used for "binding" the data to the created widgets on the "setup" stage
    factory.connect_bind(move |_factory, item| {
        let start = Instant::now();
        // In gtk4 < 4.8, you don't need the following line
        // as gtk used to pass GtkListItem directly. In order to make that API
        // generic for potentially future new APIs, it was switched to taking a GObject in 4.8
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();

        let child = item.child().unwrap().downcast::<IconTile>().unwrap();
        child.set_entry(&entry);
        // info!("bind {:?}", start.elapsed());
    });

    let grid = GridView::new(Some(selection.clone()), Some(factory));
    // let grid = ListView::new(Some(selection.clone()), Some(factory));
    // grid.set_min_columns(2);
    // grid.set_max_columns(2);
    grid.set_max_columns(128);
    grid.set_enable_rubberband(true);
    grid.set_vexpand(true);


    grid
}
