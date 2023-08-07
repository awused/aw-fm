use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use gtk::gdk::{DragAction, Key, ModifierType, Rectangle};
use gtk::glib::{self, Propagation, WeakRef};
use gtk::prelude::{Cast, CastNone, IsA, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{
    AdjustmentExt, BoxExt, EditableExt, EntryExt, EventControllerExt, FilterExt, GestureExt,
    GestureSingleExt, PopoverExt, WidgetExt,
};
use gtk::{
    CustomFilter, DragSource, DropTargetAsync, EventControllerKey, FilterChange, FilterListModel,
    GestureClick, MultiSelection, Orientation, ScrolledWindow, Widget, WidgetPaintable,
};

use self::details::DetailsView;
use self::element::{PaneElement, PaneSignals};
use self::icon_view::IconView;
use super::clipboard::{Operation, URIS};
use super::id::TabId;
use super::tab::Tab;
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


#[derive(Debug)]
pub(super) struct Pane {
    view: View,

    element: PaneElement,
    tab: TabId,
    selection: MultiSelection,

    // This is a workaround for GTK not providing ways to better segment clicks.
    // If a click is handled on an item, don't handle it again on the
    deny_view_click: Rc<Cell<bool>>,

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
        let deny_view_click = Rc::new(Cell::new(false));

        let view = match settings.display_mode {
            DisplayMode::Icons => View::Icons(IconView::new(
                &element.imp().scroller,
                tab,
                selection,
                deny_view_click.clone(),
            )),
            DisplayMode::Columns => View::Columns(DetailsView::new(
                &element.imp().scroller,
                tab,
                settings,
                selection,
                deny_view_click.clone(),
            )),
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

            deny_view_click,

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
        self.deny_view_click.set(false);
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
        self.deny_view_click.set(false);
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
        self.deny_view_click.set(false);

        if self.view.matches(settings.display_mode) {
            self.view.update_settings(settings);
            return;
        }

        let vs = self.get_state(list);

        self.view = match settings.display_mode {
            DisplayMode::Icons => View::Icons(IconView::new(
                &self.element.imp().scroller,
                self.tab,
                &self.selection,
                self.deny_view_click.clone(),
            )),
            DisplayMode::Columns => View::Columns(DetailsView::new(
                &self.element.imp().scroller,
                self.tab,
                settings,
                &self.selection,
                self.deny_view_click.clone(),
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
        self.deny_view_click.set(false);

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

    pub fn workaround_disable_rubberband(&self) {
        match &self.view {
            View::Icons(v) => v.workaround_disable_rubberband(),
            View::Columns(v) => v.workaround_disable_rubberband(),
        }
    }

    pub fn workaround_enable_rubberband(&self) {
        match &self.view {
            View::Icons(v) => v.workaround_enable_rubberband(),
            View::Columns(v) => v.workaround_enable_rubberband(),
        }
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


trait Bound {
    fn bind(&self, eo: &EntryObject);
    fn unbind(&self, eo: &EntryObject);
    fn bound_object(&self) -> Option<EntryObject>;
}

thread_local! {
    static DRAGGING_TAB: Cell<Option<TabId>> = Cell::default();
}

fn setup_view_controllers<W: IsA<Widget>>(tab: TabId, widget: &W, deny: Rc<Cell<bool>>) {
    let click = GestureClick::new();

    click.set_button(0);
    click.connect_pressed(move |c, n, x, y| {
        // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
        let alloc = c.widget().allocation();
        if !(x > 0.0 && (x as i32) < alloc.width() && y > 0.0 && (y as i32) < alloc.height()) {
            warn!("Workaround -- ignoring junk mouse event in {tab:?}");
            return;
        }

        // This part is not a workaround.
        if c.button() <= 3 && n == 1 {
            c.widget().grab_focus();
        }

        if deny.get() {
            deny.set(false);
            return;
        }

        trace!("Mousebutton {} in pane {:?}", c.current_button(), tab);

        tabs_run(|tlist| {
            let t = tlist.find(tab).unwrap();

            let mods = c.current_event().unwrap().modifier_state();
            if c.button() <= 3
                && !mods.contains(ModifierType::SHIFT_MASK)
                && !mods.contains(ModifierType::CONTROL_MASK)
            {
                t.clear_selection();
            }

            if c.current_event().unwrap().triggers_context_menu() {
                let menu = tlist.find(tab).unwrap().context_menu();

                let (x, y) =
                    gui_run(|g| c.widget().translate_coordinates(&g.window, x, y)).unwrap();

                let rect = Rectangle::new(x as i32, y as i32, 1, 1);
                menu.set_pointing_to(Some(&rect));
                menu.popup();
            }
        });
    });

    let drop_target = DropTargetAsync::new(None, DragAction::all());
    drop_target.connect_accept(move |_dta, dr| {
        if DRAGGING_TAB.with(|dt| dt.get() == Some(tab)) {
            info!("Ignoring drag into same tab");
            return false;
        }

        if !dr.formats().contain_mime_type(URIS) {
            return false;
        }

        if !tabs_run(|tlist| tlist.find(tab).unwrap().accepts_paste()) {
            return false;
        }

        true
    });

    drop_target.connect_drop(move |_dta, dr, _x, _y| {
        tabs_run(|tlist| {
            info!("Handling drop in {tab:?}");
            let t = tlist.find(tab).unwrap();

            t.drag_drop(dr, None)
        })
    });

    widget.add_controller(drop_target);
    widget.add_controller(click);
}

// Sets up various controllers that should be set only on items, and not on dead space around
// items.
fn setup_item_controllers<W: IsA<Widget>, B: IsA<Widget> + Bound>(
    tab: TabId,
    widget: &W,
    bound: WeakRef<B>,
    deny_view: Rc<Cell<bool>>,
) {
    let click = GestureClick::new();
    click.set_button(0);

    let b = bound.clone();
    let deny = deny_view.clone();
    click.connect_pressed(move |c, _n, x, y| {
        let eo = b.upgrade().unwrap().bound_object().unwrap();

        // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
        let alloc = c.widget().allocation();
        if !(x > 0.0 && (x as i32) < alloc.width() && y > 0.0 && (y as i32) < alloc.height()) {
            warn!(
                "Workaround -- ignoring junk mouse event in {tab:?} on item {:?} {x} {y}",
                &*eo.get().name
            );
            return;
        }


        debug!("Click {} for {:?} in {tab:?}", c.current_button(), &*eo.get().name);
        deny.set(true);

        if c.current_button() == 1 {
            tabs_run(|t| t.find(tab).unwrap().workaround_disable_rubberband());
        } else if c.current_button() == 2 {
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
        }
    });

    let deny = deny_view.clone();
    click.connect_released(move |_c, _n, _x, _y| {
        // This can get called at strange times
        glib::idle_add_local_once(move || {
            tabs_run(|t| t.find(tab).map(Tab::workaround_enable_rubberband));
        });
        deny.set(false);
    });

    click.connect_stopped(move |_c| {
        // This can get called at strange times
        glib::idle_add_local_once(move || {
            tabs_run(|t| t.find(tab).map(Tab::workaround_enable_rubberband));
        });
        deny_view.set(false);
    });

    let drag_source = DragSource::new();
    drag_source.set_actions(DragAction::all());
    drag_source.connect_prepare(move |ds, _x, _y| {
        let bw = bound.upgrade().unwrap();
        let eo = bw.bound_object().unwrap();

        let provider = tabs_run(|tlist| {
            tlist.set_active(tab);
            let t = tlist.find(tab).unwrap();
            t.select_if_not(eo);
            t.content_provider(Operation::Cut)
        });

        let paintable = WidgetPaintable::new(Some(&bw));

        ds.set_icon(Some(&paintable), bw.width() / 2, bw.height() / 2);
        DRAGGING_TAB.with(|dt| dt.set(Some(tab)));
        Some(provider.into())
    });

    drag_source.connect_drag_end(|_ds, _drag, _n| {
        trace!("Clearing drag source");
        DRAGGING_TAB.with(Cell::take);
    });
    drag_source.set_propagation_phase(gtk::PropagationPhase::Capture);

    widget.add_controller(click);
    widget.add_controller(drag_source);
}
