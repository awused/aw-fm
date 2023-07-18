use std::fmt::Write;
use std::time::Instant;

use gtk::gio::ListStore;
use gtk::glib::SignalHandlerId;
use gtk::prelude::{Cast, ListModelExt, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{AdjustmentExt, BoxExt, SelectionModelExt, WidgetExt};
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
    // Icons(ListView),
    Details(DetailsView),
}


#[derive(Debug)]
pub(super) struct Pane {
    // TODO -- change to "GOTO" when edited, hit escape -> reset
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
        let scroller = ScrolledWindow::new();


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

    pub(super) fn update_sort(&self, sort: SortSettings) {
        match &self.contents {
            Contents::Icons(_) => {}
            Contents::Details(details) => details.update_sort(sort),
        }
    }

    pub(super) fn update_mode(&mut self, settings: DirSettings) {
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
}
