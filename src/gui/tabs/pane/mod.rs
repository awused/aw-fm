use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gtk::gio::ListStore;
use gtk::glib::SignalHandlerId;
use gtk::prelude::{Cast, ListModelExt, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{AdjustmentExt, BoxExt, EditableExt, EntryExt, SelectionModelExt, WidgetExt};
use gtk::{
    Bitset, ColumnView, GridView, ListView, MultiSelection, Orientation, Paned, ScrolledWindow,
    Widget,
};

use self::details::DetailsView;
use self::element::{PaneElement, PaneSignals};
use self::icon_view::IconView;
use super::id::TabId;
use super::{Contents, SavedViewState};
use crate::com::{DirSettings, DisplayMode, EntryObject, SignalHolder, SortSettings};
use crate::gui::{applications, gui_run, tabs_run};

mod details;
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

fn get_last_visible_child(parent: &Widget) -> Option<Widget> {
    let parent_allocation = parent.allocation();

    let mut child = parent.last_child()?;
    loop {
        if child.is_visible() && child.is_mapped() {
            let allocation = child.allocation();
            // Get the last fully visible item so at least it stays stable.
            if allocation.y() + allocation.height() <= parent_allocation.height() {
                break Some(child);
            }
        }

        child = child.prev_sibling()?;
    }
}

pub(super) trait PaneExt {
    fn set_active(&mut self, active: bool);

    fn focus(&self);

    fn visible(&self) -> bool;

    // I don't like needing to pass list into this, but it needs to check that it's not stale.
    fn update_settings(&mut self, settings: DirSettings, list: &Contents);

    fn get_view_state(&self, list: &Contents) -> SavedViewState;

    fn apply_view_state(&mut self, state: SavedViewState, list: &Contents);

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
            trace!("Dropping detached pane");
            return;
        };

        if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
            info!("Promoting sibling pane of closed pane {:?}", self.tab);

            let start = paned.start_child().unwrap();
            let end = paned.end_child().unwrap();
            paned.set_start_child(Widget::NONE);
            paned.set_end_child(Widget::NONE);

            let start_tab = start.downcast_ref::<PaneElement>().unwrap().imp().tab.get().unwrap();
            let sibling = if start_tab == &self.tab { end } else { start };

            // A split will always have a parent.
            let grandparent = paned.parent().unwrap();
            if let Some(grandpane) = grandparent.downcast_ref::<gtk::Paned>() {
                let parent_is_start = grandpane
                    .start_child()
                    .unwrap()
                    .downcast_ref::<gtk::Paned>()
                    .map_or(false, |sc| sc.eq(paned));

                if parent_is_start {
                    grandpane.set_start_child(Some(&sibling));
                } else {
                    grandpane.set_end_child(Some(&sibling));
                }
            } else {
                let grandparent = grandparent.downcast_ref::<gtk::Box>().unwrap();
                grandparent.remove(paned);
                grandparent.append(&sibling);
            }
        } else {
            // Single pane, just remove it.
            let parent = parent.downcast_ref::<gtk::Box>().unwrap();
            parent.remove(&self.element);
        }

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
        // Needs deprecated GtkCompletion which seems buggy

        // imp.text_entry.connect_changed(|e| {
        //     println!("TODO -- changed {e:?}");
        // });
        //
        imp.text_entry.connect_activate(move |e| {
            let path: PathBuf = e.text().into();
            if path.is_file() {
                gui_run(|g| {
                    g.warning(&format!("TODO -- jump to file: {}", path.to_string_lossy()))
                });
            } else if path.is_dir() {
                tabs_run(|t| t.navigate(tab, &path));
            } else {
                // TODO -- jump to file instead if it is a file.
                gui_run(|g| g.warning(&format!("No such directory: {}", path.to_string_lossy())));
            }
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
        if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
            let old_is_start = paned
                .start_child()
                .unwrap()
                .downcast_ref::<PaneElement>()
                .map_or(false, |t| t.imp().tab.get().unwrap() == &other.tab);

            if old_is_start {
                paned.set_start_child(Some(&pane.element));
            } else {
                paned.set_end_child(Some(&pane.element));
            }
        } else {
            let parent = parent.downcast::<gtk::Box>().unwrap();
            parent.remove(&other.element);
            parent.append(&pane.element);
        }

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
    fn set_active(&mut self, active: bool) {
        if active {
            self.element.add_css_class("active-pane");
        } else {
            self.element.remove_css_class("active-pane");
        }
    }

    fn focus(&self) {
        self.element.imp().text_entry.grab_focus();
    }

    fn visible(&self) -> bool {
        // For now, if a flat pane exists it is always visible.
        // The only exception is the very brief period in replace_pane before we drop it.
        true
    }

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

        self.apply_view_state(vs, list);
    }

    fn get_view_state(&self, list: &Contents) -> SavedViewState {
        let scroll_pos = if self.element.imp().scroller.vadjustment().value() > 0.0 {
            let eo = match &self.view {
                View::Icons(ic) => ic.get_last_visible(),
                View::Columns(cv) => cv.get_last_visible(),
            };

            eo.map(|child| super::ScrollPosition {
                path: child.get().abs_path.clone(),
                index: list.position_by_sorted_entry(&child.get()).unwrap_or_default(),
            })
        } else {
            None
        };


        SavedViewState { scroll_pos, search: None }
    }

    fn apply_view_state(&mut self, state: SavedViewState, list: &Contents) {
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
