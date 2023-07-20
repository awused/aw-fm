mod icon_tile;

use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib, GridView, ListView, MultiSelection, ScrolledWindow};

use self::icon_tile::IconTile;
use super::get_first_visible_child;
use crate::com::{DisplayMode, EntryObject};
use crate::gui::tabs::{Tab, TabId};
use crate::gui::{applications, GUI};

#[derive(Debug)]
pub(super) struct IconView {
    grid: GridView,
}

impl IconView {
    pub(super) fn new(
        scroller: &ScrolledWindow,
        tab_id: TabId,
        selection: &MultiSelection,
    ) -> Self {
        let factory = gtk::SignalListItemFactory::new();

        factory.connect_setup(move |_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = IconTile::default();
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

        grid.connect_destroy(|_| error!("TODO -- remove me: grid confirmed destroyed"));
        scroller.set_child(Some(&grid));


        grid.connect_activate(move |gv, a| {
            let display = gv.display();
            let model = gv.model().and_downcast::<MultiSelection>().unwrap();

            applications::activate(tab_id, &display, &model)
        });

        Self { grid }
    }

    pub(super) fn scroll_to(&self, pos: u32) {
        self.grid.activate_action("list.scroll-to-item", Some(&pos.to_variant()));
    }

    pub(super) fn get_first_visible(&self) -> Option<EntryObject> {
        let model = self.grid.model().unwrap();
        if model.n_items() == 0 {
            return None;
        }

        get_first_visible_child(self.grid.upcast_ref::<gtk::Widget>())
            .and_then(|w| w.first_child())
            .and_downcast::<IconTile>()
            .and_then(|it: IconTile| it.bound_object())
    }
}
