use std::fmt::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use gtk::gio::ListStore;
use gtk::glib::SignalHandlerId;
use gtk::prelude::{Cast, ListModelExt, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{AdjustmentExt, BoxExt, EditableExt, EntryExt, SelectionModelExt, WidgetExt};
use gtk::{
    Bitset, ColumnView, GridView, ListView, MultiSelection, Orientation, ScrolledWindow, Widget,
};

use self::columns::DetailsView;
use self::element::{PaneElement, PaneSignals};
use self::icon_view::IconView;
use super::id::TabId;
use super::{Contents, SavedViewState};
use crate::com::{DirSettings, DisplayMode, EntryObject, SignalHolder, SortSettings};
use crate::gui::{applications, tabs_run};

mod columns;
mod element;
mod icon_view;

#[derive(Debug)]
enum View {
    Icons(IconView),
    Columns(DetailsView),
}

impl View {
    const fn matches(&self, mode: DisplayMode) -> bool {
        match (self, mode) {
            (Self::Icons(_), DisplayMode::Icons) | (Self::Columns(_), DisplayMode::Columns) => true,
            (Self::Icons(_), DisplayMode::Columns) | (Self::Columns(_), DisplayMode::Icons) => {
                false
            }
        }
    }

    fn update_settings(&self, settings: DirSettings) {
        match self {
            Self::Icons(_) => (),
            Self::Columns(details) => details.update_sort(settings.sort),
        }
    }
}


fn get_first_visible_child(parent: &Widget) -> Option<Widget> {
    let mut child = parent.first_child()?;
    loop {
        let allocation = child.allocation();
        // Assume we're dealing with a vertical list.
        if allocation.y() + allocation.height() > 0 {
            break Some(child);
        }

        child = child.next_sibling()?;
    }
}

pub(super) trait PaneExt {
    // I don't like needing to pass list into this, but it needs to check that it's not stale.
    fn update_settings(&mut self, settings: DirSettings, list: &Contents);

    fn get_view_state(&self, list: &Contents) -> SavedViewState;

    fn apply_view_state(&mut self, state: SavedViewState);

    fn workaround_scroller(&self) -> &ScrolledWindow;

    fn activate(&self);
}

#[derive(Debug)]
pub(super) struct Pane {
    view: View,

    element: PaneElement,
    contents_signals: PaneSignals,
    tab: TabId,
    selection: MultiSelection,
}

impl Drop for Pane {
    fn drop(&mut self) {
        let Some(parent) = self.element.parent() else {
            // If parent is None here, we've explicitly detached it to replace it with another
            // pane.
            return;
        };

        let parent = parent.downcast_ref::<gtk::Box>().unwrap();
        parent.remove(&self.element);

        // TODO [split]
        // if let Some(split) = parent.downcast_ref::<Split>() {
        //     let sibling = split.get_child
        //     split.remove_child(self);
        //     let grandparent = split.parent().unwrap();
        //     grandparent.insert_child_after(sibling.element, Some(&split));
        //     grandparent.remove(&split)
        // }
    }
}

impl Pane {
    // new_search(tab_id, settings, Selection)
    fn create(tab: TabId, settings: DirSettings, selection: &MultiSelection) -> Self {
        let (element, signals) = PaneElement::new(tab, selection);

        let view = match settings.display_mode {
            DisplayMode::Icons => {
                View::Icons(IconView::new(&element.imp().scroller, tab, selection))
            }
            DisplayMode::Columns => {
                View::Columns(DetailsView::new(&element.imp().scroller, tab, settings, selection))
            }
        };


        Self {
            view,

            element,
            contents_signals: signals,

            tab,
            selection: selection.clone(),
        }
    }

    fn setup_flat(mut self, tab: TabId, path: &Path) -> Self {
        debug!("Creating new flat pane for {:?}: {:?}", tab, path);

        let location = path.to_string_lossy().to_string();

        let imp = self.element.imp();
        imp.text_entry.set_text(&location);
        imp.original_text.replace(location);


        // TODO -- autocomplete for directories only (should be fast since they're always first)

        // imp.text_entry.connect_changed(|e| {
        //     println!("TODO -- changed {e:?}");
        // });
        //
        imp.text_entry.connect_activate(|e| {
            println!("TODO -- activated {e:?}");
        });


        self
    }

    pub(super) fn new_flat(
        tab: TabId,
        path: &Path,
        settings: DirSettings,
        selection: &MultiSelection,
        parent: &gtk::Box,
    ) -> Self {
        let pane = Self::create(tab, settings, selection).setup_flat(tab, path);

        // New tabs are always to the right/bottom
        parent.append(&pane.element);

        pane
    }

    pub(super) fn replace_flat(
        tab: TabId,
        path: &Path,
        settings: DirSettings,
        selection: &MultiSelection,
        other: Self,
    ) -> Self {
        let pane = Self::create(tab, settings, selection).setup_flat(tab, path);

        let parent = other.element.parent().unwrap();
        let parent = parent.downcast::<gtk::Box>().unwrap();

        parent.insert_child_after(&pane.element, Some(&other.element));
        parent.remove(&other.element);

        pane
    }

    pub(super) fn update_location(&mut self, path: &Path, settings: DirSettings, list: &Contents) {
        self.update_settings(settings, list);

        let location = path.to_string_lossy().to_string();
        self.element.imp().text_entry.set_text(&location);
        self.element.imp().original_text.replace(location);
    }

    // Most view state code should be moved here.
    pub(super) fn workaround_scroller(&self) -> &ScrolledWindow {
        &self.element.imp().scroller
    }
}

impl PaneExt for Pane {
    fn update_settings(&mut self, settings: DirSettings, list: &Contents) {
        if self.view.matches(settings.display_mode) {
            self.view.update_settings(settings);
            return;
        }

        let vs = self.get_view_state(list);

        self.view = match settings.display_mode {
            DisplayMode::Icons => {
                View::Icons(IconView::new(&self.element.imp().scroller, self.tab, &self.selection))
            }
            DisplayMode::Columns => View::Columns(DetailsView::new(
                &self.element.imp().scroller,
                self.tab,
                settings,
                &self.selection,
            )),
        };

        self.apply_view_state(vs);
    }

    fn get_view_state(&self, list: &Contents) -> SavedViewState {
        let eo = match &self.view {
            View::Icons(ic) => ic.get_first_visible(),
            View::Columns(cv) => cv.get_first_visible(),
        };

        let scroll_pos =
            eo.and_then(|obj| list.position_by_sorted_entry(&obj.get())).unwrap_or_default();


        SavedViewState { scroll_pos, search: None }
    }

    fn apply_view_state(&mut self, state: SavedViewState) {
        match &self.view {
            View::Icons(icons) => icons.scroll_to(state.scroll_pos),
            View::Columns(details) => details.scroll_to(state.scroll_pos),
        }
    }

    // Most view state code should be moved here.
    fn workaround_scroller(&self) -> &ScrolledWindow {
        &self.element.imp().scroller
    }

    fn activate(&self) {
        let display = self.element.display();
        applications::activate(self.tab, &display, &self.selection);
    }
}
