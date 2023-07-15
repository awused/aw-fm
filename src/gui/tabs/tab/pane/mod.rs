use gtk::prelude::Cast;
use gtk::traits::{BoxExt, WidgetExt};
use gtk::{ColumnView, GridView, ListView, MultiSelection, Orientation, ScrolledWindow, Widget};

use self::details::DetailsView;
use self::icon_view::IconView;
use super::Tab;
use crate::com::{DirSettings, DisplayMode, SortSettings};

mod details;
mod icon_view;

#[derive(Debug)]
enum Contents {
    Icons(IconView),
    // Icons(ListView),
    Details(DetailsView),
}


#[derive(Debug)]
pub(super) struct Pane {
    pane: gtk::Box,
    // TODO -- change to "GOTO" when edited, hit escape -> reset
    location_bar: gtk::Entry,
    pub(super) scroller: ScrolledWindow,
    contents: Contents,
    bottom_bar: (),

    tab: super::TabId,
    selection: MultiSelection,
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.scroller
            .connect_destroy(|_| error!("TODO -- remove me: confirmed pane destroyed"));
        // TODO -- confirm everything else gets destroyed as expected.
        if let Some(parent) = self.pane.parent() {
            let parent = parent.downcast_ref::<gtk::Box>().unwrap();
            parent.remove(&self.pane);
        }
    }
}

impl Pane {
    pub(super) fn new(tab: &Tab) -> Self {
        debug!("Creating {:?} pane for {:?}: {:?}", tab.settings.display_mode, tab.id, tab.path);
        let pane = gtk::Box::new(Orientation::Vertical, 0);
        pane.set_hexpand(true);
        pane.set_vexpand(true);

        let location_bar = gtk::Entry::new();
        let scroller = ScrolledWindow::new();

        scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroller.set_overlay_scrolling(false);

        let contents = match tab.settings.display_mode {
            DisplayMode::Icons => {
                Contents::Icons(IconView::new(&scroller, tab.id, &tab.contents.selection))
            }
            DisplayMode::List => Contents::Details(DetailsView::new(
                &scroller,
                tab.id,
                tab.settings,
                &tab.contents.selection,
            )),
        };

        let bottom_bar = ();

        pane.append(&location_bar);
        pane.append(&scroller);
        //pane.append(&bottom_bar);

        Self {
            pane,
            location_bar,
            scroller,
            contents,
            bottom_bar,
            tab: tab.id,
            selection: tab.contents.selection.clone(),
        }
    }

    // TODO -- include index so multi-pane views can
    pub(super) fn display(&self, parent: &gtk::Box) {
        if let Some(parent) = self.pane.parent() {
            error!("Called display() on pane that was already visible");
        } else {
            parent.append(&self.pane);
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
            DisplayMode::Icons => {
                Contents::Icons(IconView::new(&self.scroller, self.tab, &self.selection))
            }
            DisplayMode::List => Contents::Details(DetailsView::new(
                &self.scroller,
                self.tab,
                settings,
                &self.selection,
            )),
        };
    }
}
