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
use super::{SavedViewState, Tab};
use crate::com::{DirSettings, Disconnector, DisplayMode, EntryObject, SortSettings};
use crate::gui::tabs_run;

mod details;
mod element;
mod icon_view;

#[derive(Debug)]
enum Contents {
    Icons(IconView),
    Details(DetailsView),
}

impl Contents {
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

#[derive(Debug)]
pub(super) struct Pane {
    contents: Contents,

    element: PaneElement,
    signals: PaneSignals,

    tab: super::TabId,
    selection: MultiSelection,
}

impl Drop for Pane {
    fn drop(&mut self) {
        // TODO -- if panes are always displayed, this can be unwrap()
        if let Some(parent) = self.element.parent() {
            let parent = parent.downcast_ref::<gtk::Box>().unwrap();
            parent.remove(&self.element);
        }
    }
}

impl Pane {
    pub(super) fn new(tab: &Tab) -> Self {
        debug!("Creating {:?} pane for {:?}: {:?}", tab.settings.display_mode, tab.id, tab.path);

        let (element, signals) = PaneElement::new(tab);

        let contents = match tab.settings.display_mode {
            DisplayMode::Icons => Contents::Icons(IconView::new(
                &element.imp().scroller,
                tab.id.copy(),
                &tab.contents.selection,
            )),
            DisplayMode::List => Contents::Details(DetailsView::new(
                &element.imp().scroller,
                tab.id.copy(),
                tab.settings,
                &tab.contents.selection,
            )),
        };


        Self {
            contents,

            element,
            signals,

            tab: tab.id.copy(),
            selection: tab.contents.selection.clone(),
        }
    }

    // TODO -- maybe just fold this up into new() and assume it is always visible
    pub(super) fn display(&self, parent: &gtk::Box) {
        if let Some(parent) = self.element.parent() {
            error!("Called display() on pane that was already visible");
        } else {
            parent.append(&self.element);
        }
    }

    pub(super) fn update_settings(&mut self, settings: DirSettings) {
        if self.contents.matches(settings.display_mode) {
            self.contents.update_settings(settings);
            return;
        }

        self.contents = match settings.display_mode {
            DisplayMode::Icons => Contents::Icons(IconView::new(
                &self.element.imp().scroller,
                self.tab,
                &self.selection,
            )),
            DisplayMode::List => Contents::Details(DetailsView::new(
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

    pub(super) fn apply_view_state(&mut self, state: SavedViewState) {
        match &self.contents {
            Contents::Icons(icons) => icons.scroll_to(state.scroll_pos),
            Contents::Details(details) => details.scroll_to(state.scroll_pos),
        }
    }

    pub(super) fn get_view_state(&self, list: &super::Contents) -> SavedViewState {
        let eo = match &self.contents {
            Contents::Icons(ic) => ic.get_first_visible(),
            Contents::Details(_) => todo!(),
        };

        let scroll_pos =
            eo.and_then(|obj| list.position_by_sorted_entry(&obj.get())).unwrap_or_default();


        SavedViewState { scroll_pos }
    }

    // Most view state code should be moved here.
    pub(super) fn workaround_scroller(&self) -> &ScrolledWindow {
        &self.element.imp().scroller
    }
}
