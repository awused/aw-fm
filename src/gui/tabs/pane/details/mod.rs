mod icon_cell;
mod string_cell;

use std::cell::Cell;
use std::rc::Rc;

use gtk::glib::Object;
use gtk::prelude::*;
use gtk::{
    glib, ColumnView, ColumnViewColumn, ColumnViewSorter, CustomSorter, MultiSelection,
    ScrolledWindow, SignalListItemFactory, Widget,
};

use self::icon_cell::IconCell;
use self::string_cell::{EntryString, StringCell};
use super::get_last_visible_child;
use crate::com::{DirSettings, EntryObject, SignalHolder, SortDir, SortMode, SortSettings};
use crate::gui::tabs::id::TabId;
use crate::gui::{applications, tabs_run};

const NAME: &str = "Name";
const SIZE: &str = "Size";
const DATE_MODIFIED: &str = "Date Modified";


#[derive(Debug)]
pub(super) struct DetailsView {
    column_view: ColumnView,
    selection: MultiSelection,
    current_sort: Rc<Cell<SortSettings>>,

    _workaround_rubber: SignalHolder<MultiSelection>,
}


impl DetailsView {
    pub(super) fn new(
        scroller: &ScrolledWindow,
        tab_id: TabId,
        settings: DirSettings,
        selection: &MultiSelection,
    ) -> Self {
        let column_view = ColumnView::new(Some(selection.clone()));

        setup_columns(&column_view);
        set_sort(&column_view, settings.sort);

        let current_sort = Rc::new(Cell::new(settings.sort));


        let sorter = column_view.sorter().unwrap().downcast::<ColumnViewSorter>().unwrap();
        let cur_sort = current_sort.clone();
        sorter.connect_changed(move |sorter, _b| {
            let (col, direction) = sorter.nth_sort_column(0);
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
                tabs_run(|t| t.update_sort(tab_id, new_sort))
            }
        });

        column_view.connect_destroy(|_| error!("TODO -- remove me: details confirmed destroyed"));

        column_view.connect_activate(move |cv, _a| {
            let display = cv.display();
            let model = cv.model().and_downcast::<MultiSelection>().unwrap();

            applications::activate(tab_id, &display, &model)
        });

        scroller.set_child(Some(&column_view));

        // https://gitlab.gnome.org/GNOME/gtk/-/issues/5970
        column_view.set_enable_rubberband(selection.n_items() != 0);
        let cv = column_view.clone();
        let signal = selection.connect_items_changed(move |sel, _, _, _| {
            cv.set_enable_rubberband(sel.n_items() != 0);
        });
        let workaround_rubberband = SignalHolder::new(selection, signal);

        Self {
            column_view,
            current_sort,
            selection: selection.clone(),

            _workaround_rubber: workaround_rubberband,
        }
    }

    pub(super) fn update_sort(&self, sort: SortSettings) {
        if self.current_sort.get() == sort {
            return;
        }

        self.current_sort.set(sort);

        set_sort(&self.column_view, sort);
    }

    pub(super) fn scroll_to(&self, pos: u32) {
        if self.selection.n_items() <= pos {
            return;
        }

        let w = self.column_view.first_child().and_then(|c| c.next_sibling());
        if let Some(w) = w {
            glib::idle_add_local_once(move || {
                drop(w.activate_action("list.scroll-to-item", Some(&pos.to_variant())));
            });
        } else {
            error!("Couldn't find ListView to scroll in details view");
        }
    }

    // https://gitlab.gnome.org/GNOME/gtk/-/issues/4688
    pub(super) fn get_last_visible(&self) -> Option<EntryObject> {
        let model = self.column_view.model().unwrap();
        if model.n_items() == 0 {
            return None;
        }

        // This is seems like it'll be fragile.
        let obj = self
            .column_view
            .first_child()
            .and_then(|c| c.next_sibling())
            .as_ref()
            .and_then(get_last_visible_child)
            .and_then(|c| c.first_child())
            .and_then(|c| c.first_child())
            .and_downcast::<IconCell>()
            .and_then(|c| c.bound_object());

        if obj.is_none() && self.selection.n_items() != 0 {
            error!("Failed to find visible item in list with at least one item");
        }

        obj
    }
}


fn setup_columns(column_view: &ColumnView) {
    let dummy_sorter = CustomSorter::new(dummy_sort_fn);

    let icon_factory = SignalListItemFactory::new();
    icon_factory.connect_setup(move |_factory, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let cell = IconCell::default();
        item.set_child(Some(&cell));
    });
    icon_factory.connect_bind(move |_factory, item| {
        let (child, entry) = unwrap_item::<IconCell>(item);
        child.bind(&entry);
    });
    icon_factory.connect_unbind(move |_factory, item| {
        let (child, entry) = unwrap_item::<IconCell>(item);
        child.unbind(&entry);
    });

    let icon_column = ColumnViewColumn::new(None, Some(icon_factory));


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

    column_view.set_show_column_separators(true);
    column_view.set_enable_rubberband(true);
    column_view.set_vexpand(true);
    column_view.set_reorderable(false);


    column_view.append_column(&icon_column);
    column_view.append_column(&name_column);
    column_view.append_column(&size_column);
    column_view.append_column(&modified_column);
}

// Does absolutely nothing, except exist
const fn dummy_sort_fn(_a: &Object, _b: &Object) -> gtk::Ordering {
    gtk::Ordering::Equal
}

fn unwrap_item<T: IsA<Widget>>(obj: &Object) -> (T, EntryObject) {
    let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
    let child = item.child().unwrap().downcast::<T>().unwrap();
    let entry = item.item().unwrap().downcast::<EntryObject>().unwrap();
    (child, entry)
}

fn setup_string_binds(factory: &SignalListItemFactory) {
    factory.connect_bind(move |_factory, item| {
        let (child, entry) = unwrap_item::<StringCell>(item);
        child.bind(&entry);
    });

    factory.connect_unbind(move |_factory, item| {
        let (child, entry) = unwrap_item::<StringCell>(item);
        child.unbind(&entry);
    });
}

fn set_sort(column_view: &ColumnView, sort: SortSettings) {
    let column_name = match sort.mode {
        SortMode::Name => NAME,
        SortMode::MTime => DATE_MODIFIED,
        SortMode::Size => SIZE,
    };

    let binding = column_view
        .columns()
        .iter()
        .flatten()
        .filter(|col: &ColumnViewColumn| col.title().is_some())
        .find(|col: &ColumnViewColumn| col.title().unwrap().as_str() == column_name);
    let column = binding.as_ref();

    column_view.sort_by_column(column, sort.direction.into());
}
