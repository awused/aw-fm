use std::fmt::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use gtk::gio::ListStore;
use gtk::glib::SignalHandlerId;
use gtk::prelude::{Cast, ListModelExt, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{AdjustmentExt, BoxExt, EditableExt, SelectionModelExt, WidgetExt};
use gtk::{
    Bitset, ColumnView, GridView, ListView, MultiSelection, Orientation, ScrolledWindow, Widget,
};

use self::details::DetailsView;
use self::element::{PaneElement, PaneSignals};
use self::icon_view::IconView;
use super::id::TabId;
use super::{Contents, SavedViewState};
use crate::com::{DirSettings, Disconnector, DisplayMode, EntryObject, SortSettings};
use crate::gui::{applications, tabs_run};

mod details;
mod element;
mod icon_view;

#[derive(Debug)]
enum View {
    Icons(IconView),
    Details(DetailsView),
}

impl View {
    const fn matches(&self, mode: DisplayMode) -> bool {
        match (self, mode) {
            (Self::Icons(_), DisplayMode::Icons) | (Self::Details(_), DisplayMode::List) => true,
            (Self::Icons(_), DisplayMode::List) | (Self::Details(_), DisplayMode::Icons) => false,
        }
    }

    fn update_settings(&self, settings: DirSettings) {
        match self {
            Self::Icons(_) => (),
            Self::Details(details) => details.update_sort(settings.sort),
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
    fn update_settings(&mut self, settings: DirSettings);

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
    // entry_signalsn: EntrySignals...
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
        //     split.remove_child(self);
        // }
    }
}

impl Pane {
    // new_search(tab_id, settings, Selection)
    fn create(tab: TabId, path: &Path, settings: DirSettings, contents: &Contents) -> Self {
        debug!("Creating {:?} pane for {:?}: {:?}", settings.display_mode, tab, path);
        let (element, signals) = PaneElement::new(tab, contents);

        let view = match settings.display_mode {
            DisplayMode::Icons => {
                View::Icons(IconView::new(&element.imp().scroller, tab, &contents.selection))
            }
            DisplayMode::List => View::Details(DetailsView::new(
                &element.imp().scroller,
                tab,
                settings,
                &contents.selection,
            )),
        };

        element.imp().location.set_text(&path.to_string_lossy());


        Self {
            view,

            element,
            contents_signals: signals,

            tab,
            selection: contents.selection.clone(),
        }
    }

    // pub(super) fn new(tab: &Tab, parent: &gtk::Box) -> Self {
    pub(super) fn new(
        tab: TabId,
        path: &Path,
        settings: DirSettings,
        contents: &Contents,
        parent: &gtk::Box,
    ) -> Self {
        let pane = Self::create(tab, path, settings, contents);

        // New tabs are always to the right/bottom, so always blindly append.
        parent.append(&pane.element);

        pane
    }

    pub(super) fn replace(
        tab: TabId,
        path: &Path,
        settings: DirSettings,
        contents: &Contents,
        other: Self,
    ) -> Self {
        let pane = Self::create(tab, path, settings, contents);

        let parent = other.element.parent().unwrap();
        let parent = parent.downcast::<gtk::Box>().unwrap();

        parent.insert_child_after(&pane.element, Some(&other.element));
        parent.remove(&other.element);

        pane
    }

    pub(super) fn update_settings(&mut self, settings: DirSettings) {
        if self.view.matches(settings.display_mode) {
            self.view.update_settings(settings);
            return;
        }

        self.view = match settings.display_mode {
            DisplayMode::Icons => {
                View::Icons(IconView::new(&self.element.imp().scroller, self.tab, &self.selection))
            }
            DisplayMode::List => View::Details(DetailsView::new(
                &self.element.imp().scroller,
                self.tab,
                settings,
                &self.selection,
            )),
        };
    }

    pub(super) fn update_location(&mut self, path: &Path, settings: DirSettings) {
        self.update_settings(settings);
        self.element.imp().location.set_text(&path.to_string_lossy());
    }

    pub(super) fn get_view_state(&self, list: &Contents) -> SavedViewState {
        let eo = match &self.view {
            View::Icons(ic) => ic.get_first_visible(),
            View::Details(_) => todo!(),
        };

        let scroll_pos =
            eo.and_then(|obj| list.position_by_sorted_entry(&obj.get())).unwrap_or_default();


        SavedViewState { scroll_pos, search: None }
    }

    // Most view state code should be moved here.
    pub(super) fn workaround_scroller(&self) -> &ScrolledWindow {
        &self.element.imp().scroller
    }
}

impl PaneExt for Pane {
    fn update_settings(&mut self, settings: DirSettings) {
        if self.view.matches(settings.display_mode) {
            self.view.update_settings(settings);
            return;
        }

        self.view = match settings.display_mode {
            DisplayMode::Icons => {
                View::Icons(IconView::new(&self.element.imp().scroller, self.tab, &self.selection))
            }
            DisplayMode::List => View::Details(DetailsView::new(
                &self.element.imp().scroller,
                self.tab,
                settings,
                &self.selection,
            )),
        };
    }

    fn get_view_state(&self, list: &Contents) -> SavedViewState {
        let eo = match &self.view {
            View::Icons(ic) => ic.get_first_visible(),
            View::Details(_) => todo!(),
        };

        let scroll_pos =
            eo.and_then(|obj| list.position_by_sorted_entry(&obj.get())).unwrap_or_default();


        SavedViewState { scroll_pos, search: None }
    }

    fn apply_view_state(&mut self, state: SavedViewState) {
        match &self.view {
            View::Icons(icons) => icons.scroll_to(state.scroll_pos),
            View::Details(details) => details.scroll_to(state.scroll_pos),
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
