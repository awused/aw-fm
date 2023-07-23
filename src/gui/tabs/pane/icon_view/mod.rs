mod icon_tile;

use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib, GridView, ListView, MultiSelection, ScrolledWindow};

use self::icon_tile::IconTile;
use super::get_last_visible_child;
use crate::com::{DisplayMode, EntryObject, SignalHolder};
use crate::gui::tabs::id::TabId;
use crate::gui::tabs::ScrollPosition;
use crate::gui::{applications, GUI};

#[derive(Debug)]
pub(super) struct IconView {
    grid: GridView,
    selection: MultiSelection,

    workaround_rubberband: SignalHolder<MultiSelection>,
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

        // https://gitlab.gnome.org/GNOME/gtk/-/issues/5970
        grid.set_enable_rubberband(selection.n_items() != 0);
        let g = grid.clone();
        let signal = selection.connect_items_changed(move |sel, _, _, _| {
            g.set_enable_rubberband(sel.n_items() != 0);
        });
        let workaround_rubberband = SignalHolder::new(selection, signal);

        Self {
            grid,
            selection: selection.clone(),
            workaround_rubberband,
        }
    }

    pub(super) fn scroll_to(&self, pos: Option<ScrollPosition>) {
        let index = match pos {
            Some(pos) => pos.index,
            None => 0,
        };

        if self.selection.n_items() <= index {
            println!("nonono");
            return;
        }

        // This is very sensitive to when things are scrolled.
        let g = self.grid.clone();
        glib::idle_add_local_once(move || {
            g.activate_action("list.scroll-to-item", Some(&index.to_variant()));
        });
    }

    // https://gitlab.gnome.org/GNOME/gtk/-/issues/4688
    pub(super) fn get_last_visible(&self) -> Option<EntryObject> {
        let model = self.grid.model().unwrap();
        if model.n_items() == 0 {
            return None;
        }


        get_last_visible_child(self.grid.upcast_ref::<gtk::Widget>())
            .and_then(|c| c.first_child())
            .and_downcast::<IconTile>()
            .and_then(|c| c.bound_object())
    }
}
