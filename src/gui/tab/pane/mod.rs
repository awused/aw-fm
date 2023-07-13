use gtk::prelude::Cast;
use gtk::traits::{BoxExt, WidgetExt};
use gtk::{ColumnView, GridView, ListView, Orientation, ScrolledWindow, Widget};

use super::Tab;
use crate::com::DisplayMode;

mod column_view;
mod icon_view;

#[derive(Debug)]
enum Contents {
    Icons(GridView),
    // Icons(ListView),
    Details(ColumnView),
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
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.scroller
            .connect_destroy(|_| error!("TODO -- remove me: confirmed destroyed"));
        // TODO -- confirm everything else gets destroyed as expected.
        if let Some(parent) = self.pane.parent() {
            let parent = parent.downcast_ref::<gtk::Box>().unwrap();
            parent.remove(&self.pane);
        }
    }
}

impl Pane {
    pub(super) fn new(tab: &Tab) -> Self {
        debug!("Creating pane for {:?}: {:?}", tab.id, tab.path);
        let pane = gtk::Box::new(Orientation::Vertical, 0);
        pane.set_hexpand(true);
        pane.set_vexpand(true);

        let location_bar = gtk::Entry::new();
        let scroller = ScrolledWindow::new();

        scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroller.set_overlay_scrolling(false);

        let contents = match tab.settings.display_mode {
            DisplayMode::Icons => {
                let grid = icon_view::new(&tab.contents.selection);
                scroller.set_child(Some(&grid));
                Contents::Icons(grid)
            }
            DisplayMode::List => {
                let column_view = column_view::new(&tab.contents.selection);
                scroller.set_child(Some(&column_view));
                Contents::Details(column_view)
            }
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

    pub(super) fn hide(&self) {
        if let Some(parent) = self.pane.parent() {
            let parent = parent.downcast_ref::<gtk::Box>().unwrap();
            parent.remove(&self.pane);
        } else {
            error!("Called hide() on pane that wasn't visible");
        }
    }
}
