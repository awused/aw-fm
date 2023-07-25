use std::path::{Path, PathBuf};

use gtk::gdk::Key;
use gtk::prelude::{Cast, CastNone, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{AdjustmentExt, BoxExt, EditableExt, EntryExt, EventControllerExt, WidgetExt};
use gtk::{EventControllerKey, MultiSelection, Orientation, ScrolledWindow, Widget};

use self::details::DetailsView;
use self::element::{PaneElement, PaneSignals};
use self::icon_view::IconView;
use super::id::TabId;
use super::{Contents, SavedPaneState};
use crate::com::{DirSettings, DisplayMode, EntryObject};
use crate::gui::{applications, tabs_run};

mod details;
mod element;
mod icon_view;

static MIN_PANE_RES: i32 = 400;

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

    fn visible(&self) -> bool;

    fn split(&self, orientation: Orientation) -> Option<gtk::Paned>;

    // I don't like needing to pass list into this, but it needs to check that it's not stale.
    fn update_settings(&mut self, settings: DirSettings, list: &Contents);

    fn get_state(&self, list: &Contents) -> SavedPaneState;

    fn apply_state(&mut self, state: SavedPaneState, list: &Contents);

    fn workaround_scroller(&self) -> &ScrolledWindow;

    fn activate(&self);

    fn next_of_kin(&self) -> Option<TabId>;
}

#[derive(Debug)]
pub(super) struct Pane {
    view: View,

    element: PaneElement,
    tab: TabId,
    selection: MultiSelection,

    _signals: PaneSignals,
    // _flat_sig: Option<SignalHolder<gtk::Entry>>,
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
            info!("Promoting sibling of closed pane {:?}", self.tab);

            let start = paned.start_child().unwrap();
            let end = paned.end_child().unwrap();
            paned.set_start_child(Widget::NONE);
            paned.set_end_child(Widget::NONE);

            let start_tab =
                start.downcast_ref::<PaneElement>().map(|te| *te.imp().tab.get().unwrap());
            let sibling = if start_tab == Some(self.tab) { end } else { start };

            // A split will always have a parent.
            let grandparent = paned.parent().unwrap();
            if let Some(grandpane) = grandparent.downcast_ref::<gtk::Paned>() {
                let pos = grandpane.position();
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
                grandpane.set_position(pos);
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

            tab,
            selection: selection.clone(),

            _signals: signals,
        }
    }

    fn setup_flat(self, path: &Path) -> Self {
        let tab = self.tab;
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
            tabs_run(|t| t.navigate(tab, &path));
        });

        // Reset on escape
        let key = EventControllerKey::new();
        let weak = self.element.downgrade();
        key.connect_key_pressed(move |c, k, _b, m| {
            if !m.is_empty() {
                return gtk::Inhibit(false);
            }

            if Key::Escape == k {
                let w = c.widget().downcast::<gtk::Entry>().unwrap();
                w.set_text(&weak.upgrade().unwrap().imp().original_text.borrow());
            }

            gtk::Inhibit(false)
        });
        imp.text_entry.add_controller(key);

        self
    }

    pub(super) fn new_flat<F: FnOnce(&Widget)>(
        tab: TabId,
        path: &Path,
        settings: DirSettings,
        selection: &MultiSelection,
        attach: F,
    ) -> Self {
        let pane = Self::create(tab, settings, selection).setup_flat(path);

        // Where panes are created is controlled in TabsList
        attach(pane.element.upcast_ref());

        pane
    }

    pub(super) fn replace(self, new: &Widget) {
        let parent = self.element.parent().unwrap();
        let Some(paned) = parent.downcast_ref::<gtk::Paned>() else {
            let parent = parent.downcast::<gtk::Box>().unwrap();
            parent.remove(&self.element);
            parent.append(new);
            return;
        };

        let old_is_start = paned
            .start_child()
            .unwrap()
            .downcast_ref::<PaneElement>()
            .map_or(false, |t| t.imp().tab.get().unwrap() == &self.tab);

        if old_is_start {
            paned.set_start_child(Some(new));
        } else {
            paned.set_end_child(Some(new));
        }
    }

    pub(super) fn update_location(&mut self, path: &Path, settings: DirSettings, list: &Contents) {
        self.update_settings(settings, list);

        let location = path.to_string_lossy().to_string();
        self.element.imp().text_entry.set_text(&location);
        self.element.imp().original_text.replace(location);
        self.element.imp().seek.set_text("");
        self.element.imp().stack.set_visible_child_name("count");
    }
}

impl PaneExt for Pane {
    fn set_active(&mut self, active: bool) {
        self.element.imp().active.set(active);
        if active {
            self.element.add_css_class("active-pane");
            self.element.imp().text_entry.grab_focus_without_selecting();
        } else {
            self.element.remove_css_class("active-pane");
        }
    }

    fn visible(&self) -> bool {
        // If a flat pane exists it is always visible.
        // The only exception is the very brief period in replace_pane before it is dropped.
        true
    }

    fn update_settings(&mut self, settings: DirSettings, list: &Contents) {
        if self.view.matches(settings.display_mode) {
            self.view.update_settings(settings);
            return;
        }

        let vs = self.get_state(list);

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

        self.apply_state(vs, list);
    }

    fn get_state(&self, list: &Contents) -> SavedPaneState {
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


        SavedPaneState { scroll_pos, search: None }
    }

    fn apply_state(&mut self, state: SavedPaneState, list: &Contents) {
        let pos = state
            .scroll_pos
            .and_then(|sp| {
                if let Some(eo) = EntryObject::lookup(&sp.path) {
                    let pos = list.position_by_sorted_entry(&eo.get());
                    debug!("Scrolling to position from element {pos:?}");
                    pos
                } else {
                    Some(sp.index)
                }
            })
            .unwrap_or(0);

        match &self.view {
            View::Icons(icons) => icons.scroll_to(pos),
            View::Columns(details) => details.scroll_to(pos),
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

    fn split(&self, orient: Orientation) -> Option<gtk::Paned> {
        let paned = match orient {
            Orientation::Horizontal if self.element.width() > MIN_PANE_RES * 2 => {
                gtk::Paned::new(orient)
            }
            Orientation::Vertical if self.element.height() > MIN_PANE_RES * 2 => {
                gtk::Paned::new(orient)
            }
            Orientation::Horizontal | Orientation::Vertical => return None,
            _ => unreachable!(),
        };
        info!("Splitting pane for {:?}", self.tab);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        // paned.set_position(position)

        let parent = self.element.parent().unwrap();

        if let Some(parent) = parent.downcast_ref::<gtk::Paned>() {
            let pos = parent.position();
            let start = parent.start_child().unwrap();
            let start_tab =
                start.downcast_ref::<PaneElement>().map(|te| *te.imp().tab.get().unwrap());

            if Some(self.tab) == start_tab {
                parent.set_start_child(Some(&paned));
            } else {
                parent.set_end_child(Some(&paned));
            }
            parent.set_position(pos);
        } else {
            let parent = parent.downcast_ref::<gtk::Box>().unwrap();
            parent.remove(&self.element);
            parent.append(&paned);
        }

        paned.set_start_child(Some(&self.element));

        Some(paned)
    }

    // Finds the next closest sibling in the tree by reversing splits.
    fn next_of_kin(&self) -> Option<TabId> {
        let paned = self.element.parent().and_downcast::<gtk::Paned>()?;

        let start = paned.start_child().unwrap();
        let end = paned.end_child().unwrap();

        let start_tab = start.downcast_ref::<PaneElement>().map(|te| *te.imp().tab.get().unwrap());

        let kin = if start_tab == Some(self.tab) {
            firstmost_descendent(end)
        } else {
            lastmost_descendent(start)
        };

        debug!("Next kin of {:?} was {kin:?}", self.tab);

        Some(kin)
    }
}

fn firstmost_descendent(mut widget: Widget) -> TabId {
    while let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
        widget = paned.start_child().unwrap();
    }

    *widget.downcast::<PaneElement>().unwrap().imp().tab.get().unwrap()
}

fn lastmost_descendent(mut widget: Widget) -> TabId {
    while let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
        widget = paned.end_child().unwrap();
    }

    *widget.downcast::<PaneElement>().unwrap().imp().tab.get().unwrap()
}
