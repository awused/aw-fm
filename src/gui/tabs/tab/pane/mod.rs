use std::fmt::Write;
use std::time::Instant;

use gtk::gio::ListStore;
use gtk::glib::SignalHandlerId;
use gtk::prelude::{Cast, ListModelExt, ObjectExt};
use gtk::traits::{BoxExt, SelectionModelExt, WidgetExt};
use gtk::{
    Bitset, ColumnView, GridView, ListView, MultiSelection, Orientation, ScrolledWindow, Widget,
};

use self::details::DetailsView;
use self::icon_view::IconView;
use super::Tab;
use crate::com::{DirSettings, Disconnector, DisplayMode, EntryObject, SortSettings};

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

    bottom_bar: gtk::Box,

    // count_label: gtk::Label,
    // selection_label: gtk::Label,
    count_signal: Disconnector<ListStore>,
    selection_signal: Disconnector<MultiSelection>,

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


        let bottom_bar = gtk::Box::new(Orientation::Horizontal, 0);
        let (count_signal, selection_signal) = Self::setup_bottom_bar(tab, &bottom_bar);


        pane.append(&location_bar);
        pane.append(&scroller);
        pane.append(&bottom_bar);

        Self {
            pane,
            location_bar,
            scroller,
            contents,
            bottom_bar,

            count_signal,
            selection_signal,

            tab: tab.id,
            selection: tab.contents.selection.clone(),
        }
    }

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

    // byte size, folder items, num_folders
    fn selected_string(selection: &MultiSelection, set: &Bitset) -> String {
        let len = set.size();
        let mut dirs = 0;
        let mut i = 0;
        let mut bytes = 0;
        let mut contents = 0;
        while i < len {
            let idx = set.nth(i as u32);
            let obj = selection.item(idx).unwrap().downcast::<EntryObject>().unwrap();
            let entry = obj.get();

            if entry.dir() {
                dirs += 1;
                contents += entry.raw_size();
            } else {
                bytes += entry.raw_size();
            }

            i += 1;
        }

        let mut label = String::new();
        if dirs > 0 {
            write!(
                &mut label,
                "{dirs} folder{} selected (containing {contents} items)",
                if dirs > 1 { "s" } else { "" }
            );
            if dirs < len {
                write!(&mut label, ", ");
            }
        }

        if dirs < len {
            write!(
                &mut label,
                "{} file{} selected ({})",
                len - dirs,
                if len - dirs > 1 { "s" } else { "" },
                humansize::format_size(bytes, humansize::WINDOWS)
            );
        }
        label
    }

    fn setup_bottom_bar(
        tab: &Tab,
        bottom_bar: &gtk::Box,
    ) -> (Disconnector<ListStore>, Disconnector<MultiSelection>) {
        let count_label = gtk::Label::new(Some(&format!("{} items", tab.contents.list.n_items())));
        let selection_label = gtk::Label::new(None);
        selection_label.set_visible(false);
        selection_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

        bottom_bar.append(&count_label);
        bottom_bar.append(&selection_label);

        // let count = count_label.downgrade();
        let count = count_label.clone();
        let count_signal = tab.contents.list.connect_items_changed(move |list, _p, _a, _r| {
            count.set_text(&format!("{} items", list.n_items()));
        });
        let count_signal = Disconnector::new(&tab.contents.list, count_signal);

        let count = count_label.clone();
        let selected = selection_label.clone();
        let update_selected = move |selection: &MultiSelection, _p: u32, _n: u32| {
            let set = selection.selection();
            let len = set.size();
            if len == 0 {
                selected.set_visible(false);
                count.set_visible(true);
                return;
            }
            selected.set_visible(true);
            count.set_visible(false);


            if len == 1 {
                let obj = selection.item(set.nth(0)).unwrap().downcast::<EntryObject>().unwrap();
                let entry = obj.get();

                selected.set_text(&format!(
                    "\"{}\" selected ({}{})",
                    entry.name.to_string_lossy(),
                    if entry.dir() { "containing " } else { "" },
                    entry.long_size_string()
                ));
                return;
            }

            // Costly, but not unbearably slow at <20ms for 100k items.
            selected.set_text(&Self::selected_string(selection, &set));
        };
        update_selected(&tab.contents.selection, 0, 0);
        let selection_signal = tab.contents.selection.connect_selection_changed(update_selected);

        let selection_signal = Disconnector::new(&tab.contents.selection, selection_signal);

        count_label.connect_destroy(|_| error!("Remove me: confirmed count label destroyed"));
        selection_label
            .connect_destroy(|_| error!("Remove me: confirmed selection label destroyed"));

        (count_signal, selection_signal)
    }
}
