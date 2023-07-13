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
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let row = IconTile::new();
        item.set_child(Some(&row));
    });

    // the bind stage is used for "binding" the data to the created widgets on the "setup" stage
    factory.connect_bind(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();
        println!("bind {entry:?}");
        let child = item.child().unwrap().downcast::<IconTile>().unwrap();

        // entry.connect_notify_local(name, f)
        child.bind(&entry);
        // info!("bind {:?}", start.elapsed());
    });

    factory.connect_unbind(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();
        println!("unbind {item:?} {entry:?}");
        let child = item.child().unwrap().downcast::<IconTile>().unwrap();

        child.unbind(&entry);
    });

    let grid = GridView::new(Some(selection.clone()), Some(factory));
    // let grid = ListView::new(Some(selection.clone()), Some(factory));
    // grid.set_min_columns(2);
    // grid.set_max_columns(2);
    // We want this to grow as necessary, but setting this too high (>32) absolutely tanks
    // performance.
    grid.set_max_columns(16);
    grid.set_enable_rubberband(true);
    grid.set_vexpand(true);


    grid
}
