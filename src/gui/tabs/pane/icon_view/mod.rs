use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{GestureClick, GridView, ListScrollFlags, MultiSelection, ScrolledWindow, Widget};

use self::icon_tile::IconTile;
use super::{Bound, get_last_visible_child, setup_item_controllers, setup_view_controllers};
use crate::com::EntryObject;
use crate::gui::tabs::id::TabId;
use crate::gui::tabs::pane::MIN_GRID_RES;
use crate::gui::{applications, tabs_run};

mod icon_tile;


#[derive(Debug)]
pub(super) struct IconView {
    grid: GridView,
}

impl Drop for IconView {
    fn drop(&mut self) {
        trace!("Working around GTK crash by setting model to null");
        self.grid.set_model(None::<&MultiSelection>);
    }
}

impl IconView {
    pub(super) fn new(
        scroller: &ScrolledWindow,
        tab: TabId,
        selection: &MultiSelection,
        deny_view_click: Rc<Cell<bool>>,
        initial_width: u32,
    ) -> Self {
        let factory = gtk::SignalListItemFactory::new();

        let deny = deny_view_click.clone();
        factory.connect_setup(move |_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let tile = IconTile::default();
            // Bind to the individual items, not the entire massive tile.
            let w = tile.downgrade();
            setup_item_controllers(tab, &*tile.imp().image, w.clone(), w.clone(), deny.clone());
            setup_item_controllers(tab, &*tile.imp().name, w.clone(), w.clone(), deny.clone());
            setup_item_controllers(tab, &*tile.imp().size, w.clone(), w, deny.clone());

            let deny_bg_click = GestureClick::new();
            deny_bg_click.set_button(1);
            let d = deny.clone();
            deny_bg_click.connect_pressed(move |c, _n, x, y| {
                // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
                let w = c.widget().unwrap();
                if !w.contains(x, y) {
                    warn!("Workaround -- ignoring junk mouse event in {tab:?} on item tile",);
                    return;
                }

                if !d.get() {
                    d.set(true);
                }
            });
            tile.add_controller(deny_bg_click);

            item.set_child(Some(&tile));
        });

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
        grid.set_enable_rubberband(true);
        grid.set_vexpand(true);

        // We want this to grow as necessary, but setting this too high (>32) absolutely tanks
        // performance if not necessary. Setting it low massively improves performance.
        // Compared to the old 16, using 6 improves minimum navigation time from ~200ms to ~60ms.
        // This dynamic value feels fairly comfortable. Cap it at 32.
        let col_width = MIN_GRID_RES.get().0;
        if initial_width == 0 || col_width <= 0 {
            grid.set_min_columns(1);
            grid.set_max_columns(8);
        } else {
            let columns = ((initial_width as f32 / col_width as f32).floor() as u32).clamp(1, 32);
            grid.set_min_columns(columns);
            grid.set_max_columns(columns);
        }

        debug!("Created grid view with an initial {} columns", grid.max_columns());

        grid.connect_activate(move |gv, _a| {
            let display = gv.display();
            let model = &gv.model().and_downcast::<MultiSelection>().unwrap();

            // It should be impossible for this to be missing.
            let path = tabs_run(|list| list.find(tab).unwrap().dir());

            applications::open(tab, &path, &display, model.into(), true)
        });


        setup_view_controllers(tab, &grid, deny_view_click);

        scroller.set_child(Some(&grid));

        Self { grid }
    }

    pub(super) fn scroll_to(&self, pos: u32, flags: ListScrollFlags) {
        let model = self.grid.model().unwrap();
        if model.n_items() <= pos {
            return;
        }

        self.grid.scroll_to(pos, flags, None);
    }

    // https://gitlab.gnome.org/GNOME/gtk/-/issues/4688
    pub(super) fn get_scroll_target(&self) -> Option<EntryObject> {
        let model = self.grid.model().unwrap();
        if model.n_items() == 0 {
            return None;
        }

        get_last_visible_child(self.grid.upcast_ref::<gtk::Widget>())
            .and_then(|c| c.first_child())
            .and_downcast::<IconTile>()
            .and_then(|c| c.bound_object())
    }

    pub(super) fn change_model(&mut self, selection: &MultiSelection) {
        self.grid.set_model(Some(selection));
    }

    pub(super) fn grab_focus(&self) {
        self.grid.grab_focus();
    }

    pub(super) fn focus_child(&self) -> Option<(Widget, EntryObject)> {
        let child = self.grid.focus_child()?;

        child
            .first_child()
            .and_downcast::<IconTile>()
            .and_then(|c| c.bound_object())
            .map(|eo| (child, eo))
    }

    pub(super) fn fix_focus_before_delete(&self, eo: &EntryObject) {
        let Some((child, focused)) = self.focus_child() else {
            return;
        };

        if !child.has_focus() {
            return;
        }

        if *focused.get() != *eo.get() {
            return;
        }

        if let Some(next) = child.next_sibling().or_else(|| child.prev_sibling()) {
            warn!("Workaround - Fixing broken focus on deletion in grid view");
            next.grab_focus();
        }
    }

    pub(super) fn workaround_enable_rubberband(&self) {
        self.grid.set_enable_rubberband(true);
    }

    pub(super) fn workaround_disable_rubberband(&self) {
        self.grid.set_enable_rubberband(false);
    }
}
