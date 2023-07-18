use std::fmt::Write;
use std::path::Path;
use std::time::Instant;

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

mod details;
mod element;
mod icon_view;

#[derive(Debug)]
enum Contents {
    Icons(IconView),
    Details(DetailsView),
}

impl Contents {
    fn matches(&self, mode: DisplayMode) -> bool {
        match (self, mode) {
            (Contents::Icons(_), DisplayMode::Icons)
            | (Contents::Details(_), DisplayMode::List) => true,
            (Contents::Icons(_), DisplayMode::List)
            | (Contents::Details(_), DisplayMode::Icons) => false,
        }
    }

    fn update_settings(&self, settings: DirSettings) {
        match self {
            Contents::Icons(_) => (),
            Contents::Details(details) => details.update_sort(settings.sort),
        }
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
                tab.id,
                &tab.contents.selection,
            )),
            DisplayMode::List => Contents::Details(DetailsView::new(
                &element.imp().scroller,
                tab.id,
                tab.settings,
                &tab.contents.selection,
            )),
        };


        Self {
            contents,

            element,
            signals,

            tab: tab.id,
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

    pub(super) fn apply_view_state(&mut self, state: SavedViewState) {
        self.element.imp().scroller.vadjustment().set_value(state.scroll_pos);
    }

    // Most view state code should be moved here.
    pub(super) fn workaround_scroller(&self) -> &ScrolledWindow {
        &self.element.imp().scroller
    }

    pub(super) fn update_location(&mut self, path: &Path, settings: DirSettings) {
        self.update_settings(settings);
        self.element.imp().location.set_text(&path.to_string_lossy());
    }
}
