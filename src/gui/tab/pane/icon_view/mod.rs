mod icon_tile;

use std::time::Instant;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib, GridView, ListView, MultiSelection};

use self::icon_tile::IconTile;
use crate::com::EntryObject;

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
        let child = item.child().unwrap().downcast::<IconTile>().unwrap();

        child.bind(&entry);
    });

    factory.connect_unbind(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();
        let child = item.child().unwrap().downcast::<IconTile>().unwrap();

        child.unbind(&entry);
    });

    let grid = GridView::new(Some(selection.clone()), Some(factory));
    // We want this to grow as necessary, but setting this too high (>32) absolutely tanks
    // performance. 16 is enough for a big 4K monitor and doesn't seem to ruin performance.
    grid.set_max_columns(16);
    grid.set_enable_rubberband(true);
    grid.set_vexpand(true);


    grid
}
