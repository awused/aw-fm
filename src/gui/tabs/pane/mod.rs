use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use gtk::gdk::{Key, Rectangle};
use gtk::glib::{Propagation, WeakRef};
use gtk::prelude::{Cast, CastNone, IsA, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{
    AdjustmentExt, BoxExt, EditableExt, EntryExt, EventControllerExt, FilterExt, GestureExt,
    GestureSingleExt, PopoverExt, WidgetExt,
};
use gtk::{
    CustomFilter, EventControllerKey, FilterChange, FilterListModel, GestureClick, MultiSelection,
    Orientation, ScrolledWindow, Widget,
};

use self::details::DetailsView;
use self::element::{PaneElement, PaneSignals};
use self::icon_view::IconView;
use super::id::TabId;
use super::{Contents, PaneState};
use crate::com::{DirSettings, DisplayMode, EntryObject, SignalHolder};
use crate::gui::tabs::NavTarget;
use crate::gui::{gui_run, tabs_run};

mod details;
mod element;
mod icon_view;

static MIN_PANE_RES: i32 = 400;
// TODO -- lower to 2 when incremental filtering isn't broken.
static MIN_SEARCH: usize = 3;


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

trait Bound {
    fn bind(&self, eo: &EntryObject);
    fn unbind(&self, eo: &EntryObject);
    fn bound_object(&self) -> Option<EntryObject>;
}

fn setup_item_controllers<W: IsA<Widget>, B: IsA<Widget> + Bound>(
    tab: TabId,
    widget: &W,
    bound: WeakRef<B>,
) {
    let click = GestureClick::new();
    click.set_button(0);

    click.connect_pressed(move |c, _n, x, y| {
        let eo = bound.upgrade().unwrap().bound_object().unwrap();

        // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
        let alloc = c.widget().allocation();
        if !(x > 0.0 && (x as i32) < alloc.width() && y > 0.0 && (y as i32) < alloc.height()) {
            error!(
                "Workaround -- ignoring junk mouse event in {tab:?} on item {:?}",
                &*eo.get().name
            );
            return;
        }

        debug!("Click {} for {:?} in {tab:?}", c.current_button(), &*eo.get().name);

        if c.current_button() == 2 {
            if let Some(nav) = NavTarget::open_or_jump_abs(eo.get().abs_path.clone()) {
                tabs_run(|t| t.create_tab(Some(tab), nav, false));
            }
        } else if c.current_event().unwrap().triggers_context_menu() {
            let w = c.widget();

            c.set_state(gtk::EventSequenceState::Claimed);
            let menu = tabs_run(|tlist| {
                tlist.set_active(tab);
                let t = tlist.find(tab).unwrap();
                t.select_if_not(eo);
                t.context_menu()
            });

            let (x, y) = gui_run(|g| w.translate_coordinates(&g.window, x, y)).unwrap();

            let rect = Rectangle::new(x as i32, y as i32, 1, 1);
            menu.set_pointing_to(Some(&rect));
            menu.popup();
            println!("TODO -- right click context menu")
        }
    });

    widget.add_controller(click);
}

#[derive(Debug)]
pub(super) struct Pane {
    view: View,

    element: PaneElement,
    tab: TabId,
    selection: MultiSelection,

    _signals: PaneSignals,

    connections: Vec<SignalHolder<gtk::Entry>>,
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
    pub const fn tab(&self) -> TabId {
        self.tab
    }

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

        // Reset on escape
        let key = EventControllerKey::new();
        let weak = element.downgrade();
        key.connect_key_pressed(move |c, k, _b, m| {
            if !m.is_empty() {
                return Propagation::Proceed;
            }

            if Key::Escape == k {
                let w = c.widget().downcast::<gtk::Entry>().unwrap();
                w.set_text(&weak.upgrade().unwrap().imp().original_text.borrow());
                Propagation::Stop
            } else {
                Propagation::Proceed
            }
        });
        element.imp().text_entry.add_controller(key);


        Self {
            view,

            element,

            tab,
            selection: selection.clone(),

            _signals: signals,

            connections: Vec::new(),
        }
    }

    fn setup_flat(&mut self, path: &Path) {
        self.connections.clear();
        let tab = self.tab;
        debug!("Creating new flat pane for {tab:?}: {path:?}");

        let location = path.to_string_lossy().to_string();

        let imp = self.element.imp();
        imp.text_entry.set_text(&location);
        imp.original_text.replace(location);


        // TODO -- autocomplete for directories only (should be fast since they're always first)
        // Needs deprecated GtkCompletion which seems buggy

        // imp.text_entry.connect_changed(|e| {
        //     println!("TODO -- changed {e:?}");
        // });

        let sig = imp.text_entry.connect_activate(move |e| {
            let path: PathBuf = e.text().into();
            tabs_run(|t| t.navigate(tab, &path));
        });
        let connections = vec![SignalHolder::new(&*imp.text_entry, sig)];

        self.connections = connections;
    }

    fn setup_search(
        &mut self,
        filter: CustomFilter,
        filtered: FilterListModel,
        queries: (Rc<RefCell<String>>, Rc<RefCell<String>>),
    ) {
        let original = queries.0;
        let query_rc = queries.1;

        self.connections.clear();
        let tab = self.tab;
        let imp = self.element.imp();
        debug!("Creating new search pane for {tab:?}: {:?}", original.borrow());

        imp.text_entry.set_text(&original.borrow());
        imp.original_text.replace("".to_string());

        // Decent opportunity for UnsafeCell if it benchmarks better.
        let query = query_rc.clone();

        let filt = filter.clone();
        let signal = imp.text_entry.connect_changed(move |e| {
            let text = e.text();
            original.replace(text.to_string());

            let mut query = query_rc.borrow_mut();
            let new = text.to_lowercase();

            if new == *query || (query.len() < MIN_SEARCH && new.len() < MIN_SEARCH) {
                return;
            }

            // https://gitlab.gnome.org/GNOME/gtk/-/issues/5989
            // let mut incremental = true;

            let change = if query.len() < MIN_SEARCH {
                FilterChange::LessStrict
            } else if new.len() < MIN_SEARCH {
                FilterChange::MoreStrict
            } else if query.contains(&new) {
                FilterChange::LessStrict
            } else if new.contains(&*query) {
                // Causes annoying flickering
                // incremental = false;
                FilterChange::MoreStrict
            } else {
                // Clobbers selection
                // incremental = false;
                FilterChange::Different
            };

            *query = new;
            drop(query);

            let start = Instant::now();
            // filtered.set_incremental(incremental);
            filt.changed(change);
            trace!(
                "Updated search filter to be {change:?} in {:?}, incremental {}",
                start.elapsed(),
                filtered.is_incremental()
            );
        });

        filter.set_filter_func(move |obj| {
            let q = query.borrow();
            if q.len() < MIN_SEARCH {
                return false;
            }

            let eo = obj.downcast_ref::<EntryObject>().unwrap();
            eo.get().name.lowercase().contains(&*q)
        });

        self.connections = vec![SignalHolder::new(&*imp.text_entry, signal)];
    }

    pub(super) fn new_flat<F: FnOnce(&Widget)>(
        tab: TabId,
        path: &Path,
        settings: DirSettings,
        selection: &MultiSelection,
        attach: F,
    ) -> Self {
        let mut pane = Self::create(tab, settings, selection);
        pane.setup_flat(path);

        // Where panes are created is controlled in TabsList
        attach(pane.element.upcast_ref());

        pane
    }

    pub(super) fn new_search<F: FnOnce(&Widget)>(
        tab: TabId,
        queries: (Rc<RefCell<String>>, Rc<RefCell<String>>),
        settings: DirSettings,
        selection: &MultiSelection,
        filter: CustomFilter,
        filtered: FilterListModel,
        attach: F,
    ) -> Self {
        let mut pane = Self::create(tab, settings, selection);
        pane.setup_search(filter, filtered, queries);

        // Where panes are created is controlled in TabsList
        attach(pane.element.upcast_ref());

        pane
    }

    pub(super) fn search_to_flat(&mut self, path: &Path, selection: &MultiSelection) {
        match &mut self.view {
            View::Icons(ic) => ic.change_model(selection),
            View::Columns(cv) => cv.change_model(selection),
        }

        self._signals = self.element.setup_signals(selection);
        self.setup_flat(path);
    }

    pub(super) fn flat_to_search(
        &mut self,
        queries: (Rc<RefCell<String>>, Rc<RefCell<String>>),
        selection: &MultiSelection,
        filter: CustomFilter,
        filtered: FilterListModel,
    ) {
        match &mut self.view {
            View::Icons(ic) => ic.change_model(selection),
            View::Columns(cv) => cv.change_model(selection),
        }

        self._signals = self.element.setup_signals(selection);
        self.setup_search(filter, filtered, queries);

        if self.element.imp().active.get() {
            self.element.imp().text_entry.grab_focus_without_selecting();
        }
    }

    pub(super) fn replace_with_other_tab(self, new: &Widget) {
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

    pub(super) fn move_active_focus_to_text(&self) {
        if self.element.imp().active.get() {
            self.element.imp().text_entry.grab_focus_without_selecting();
        }
    }

    pub(super) fn update_search(&self, query: &str) {
        self.element.imp().text_entry.set_text(query);
    }

    pub(super) fn set_clipboard_text(&self, text: &str) {
        let stack = &self.element.imp().stack;
        let clipboard = &self.element.imp().clipboard;

        clipboard.set_text(text);
        clipboard.set_tooltip_text(Some(text));

        stack.set_visible_child_name("clipboard");
    }

    pub fn set_active(&mut self, active: bool) {
        self.element.imp().active.set(active);
        if active {
            self.element.add_css_class("active-pane");
            self.element.imp().text_entry.grab_focus_without_selecting();
        } else {
            self.element.remove_css_class("active-pane");
        }
    }

    pub fn update_settings(&mut self, settings: DirSettings, list: &Contents) {
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

    pub fn get_state(&self, list: &Contents) -> PaneState {
        let scroll_pos = if self.element.imp().scroller.vadjustment().value() > 0.0 {
            let eo = match &self.view {
                View::Icons(ic) => ic.get_last_visible(),
                View::Columns(cv) => cv.get_last_visible(),
            };

            eo.map(|child| super::ScrollPosition {
                path: child.get().abs_path.clone(),
                index: list.filtered_position_by_sorted(&child.get()).unwrap_or_default(),
            })
        } else {
            None
        };


        PaneState { scroll_pos }
    }

    pub fn apply_state(&mut self, state: PaneState, list: &Contents) {
        let pos = state
            .scroll_pos
            .and_then(|sp| {
                if let Some(eo) = EntryObject::lookup(&sp.path) {
                    let pos = list.filtered_position_by_sorted(&eo.get());
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
    pub fn workaround_scroller(&self) -> &ScrolledWindow {
        &self.element.imp().scroller
    }

    pub fn split(&self, orient: Orientation) -> Option<gtk::Paned> {
        let paned = match orient {
            Orientation::Horizontal if self.element.width() > MIN_PANE_RES * 2 => {
                gtk::Paned::builder().orientation(orient).position(self.element.width() / 2)
            }
            Orientation::Vertical if self.element.height() > MIN_PANE_RES * 2 => {
                gtk::Paned::builder().orientation(orient).position(self.element.height() / 2)
            }
            Orientation::Horizontal | Orientation::Vertical => return None,
            _ => unreachable!(),
        };
        info!("Splitting pane for {:?}", self.tab);
        let paned = paned.shrink_start_child(false).shrink_end_child(false).build();

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
    pub fn next_of_kin(&self) -> Option<TabId> {
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
