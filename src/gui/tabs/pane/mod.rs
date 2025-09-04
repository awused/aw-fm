use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::cmp::min;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Instant;

use ahash::AHashMap;
use gtk::gdk::{DragAction, Key, ModifierType, Rectangle};
use gtk::gio::Icon;
use gtk::glib::{self, Propagation, WeakRef};
use gtk::graphene::Point;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{
    CustomFilter, DragSource, DropTargetAsync, EventControllerKey, FilterChange, FilterListModel,
    GestureClick, ListScrollFlags, MultiSelection, Orientation, Widget, WidgetPaintable,
};

use self::details::DetailsView;
use self::element::{PaneElement, PaneSignals};
use self::icon_view::IconView;
use super::id::TabId;
use super::tab::Tab;
use super::{Contents, PaneState, PrecisePosition};
use crate::com::{DirSettings, DisplayMode, EntryObject, SignalHolder};
use crate::database::{SavedSplit, SplitChild};
use crate::gui::clipboard::{ClipboardOp, URIS};
use crate::gui::tabs::list::TabPosition;
use crate::gui::tabs::{FocusState, NavTarget};
use crate::gui::{
    ActionTarget, CompletionResult, ControllerDisconnector, DebugIgnore, ManagerAction, gui_run,
    tabs_run,
};
use crate::natsort::normalize_lowercase;

mod details;
mod element;
mod icon_view;

// For now, we don't care much about height, so width works for both
pub const MIN_PANE_RES: i32 = 250;
// TODO [incremental] -- lower to 2 when incremental filtering isn't broken.
static MIN_SEARCH: usize = 3;

// We don't have this until we measure a grid with at least one item
static MIN_GRID_RES: OnceLock<(i32, i32)> = OnceLock::new();

thread_local! {
    static DRAGGING_TAB: Cell<Option<TabId>> = Cell::default();
    static SYMLINK_BADGE: Option<Icon> =
        match Icon::for_string("emblem-symbolic-link") {
            Ok(icon) => Some(icon),
            Err(e) => {
                error!("Failed to load symbolic link badge: {e}");
                None
            },
        };
}


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

    fn grab_focus(&self) {
        match self {
            Self::Icons(i) => i.grab_focus(),
            Self::Columns(d) => d.grab_focus(),
        }
    }

    fn focus_child(&self) -> Option<(Widget, EntryObject)> {
        match self {
            Self::Icons(i) => i.focus_child(),
            Self::Columns(d) => d.focus_child(),
        }
    }

    fn get_scroll_target(&self) -> Option<EntryObject> {
        match self {
            Self::Icons(ic) => ic.get_scroll_target(),
            Self::Columns(cv) => cv.get_scroll_target(),
        }
    }

    fn scroll_to(&self, pos: u32, flags: ListScrollFlags) {
        match self {
            Self::Icons(icon_view) => icon_view.scroll_to(pos, flags),
            Self::Columns(details_view) => details_view.scroll_to(pos, flags),
        }
    }

    const fn current_display_mode(&self) -> DisplayMode {
        match self {
            Self::Icons(_icon_view) => DisplayMode::Icons,
            Self::Columns(_details_view) => DisplayMode::Columns,
        }
    }
}

// fn get_first_visible_child(parent: &Widget) -> Option<Widget> {
//     let mut child = parent.first_child()?;
//     loop {
//         if child.is_visible() && child.is_mapped() {
//             let Some(bounds) = child.compute_bounds(parent) else {
//                 continue;
//             };
//             // Get the first fully visible item.
//             if bounds.y() >= 0.0 {
//                 break Some(child);
//             }
//         }
//
//         child = child.next_sibling()?;
//     }
// }

fn get_last_visible_child(parent: &Widget) -> Option<Widget> {
    let parent_height = parent.height() as f32;

    let mut child = parent.last_child()?;
    loop {
        if child.is_visible() && child.is_mapped() {
            let Some(bounds) = child.compute_bounds(parent) else {
                continue;
            };
            // Get the first fully visible item.
            if bounds.bottom_right().y() <= parent_height {
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

    completion_controllers: Vec<ControllerDisconnector>,
    completion_result: DebugIgnore<Rc<Cell<Option<CompletionResult>>>>,

    // For storing the pane state while we're in the process of applying it.
    // To guard against the user navigating away while we're applying the state.
    workaround_temp_state: Option<PaneState>,
}

impl Drop for Pane {
    fn drop(&mut self) {
        if self.element.parent().is_none() {
            // If parent is None here, we've explicitly detached it to replace it with another
            // pane.
            return trace!("Dropping detached pane");
        };

        self.remove_from_parent();
    }
}

impl Pane {
    pub fn set_visible(&self, visible: bool) {
        self.element.set_visible(visible);
    }

    fn create(
        tab: TabId,
        settings: DirSettings,
        selection: &MultiSelection,
        initial_width: u32,
    ) -> Self {
        let (element, signals) = PaneElement::new(tab, selection);
        let deny_view_click = Rc::new(Cell::new(false));

        let view = match settings.display_mode {
            DisplayMode::Icons => View::Icons(IconView::new(
                &element.imp().scroller,
                tab,
                selection,
                deny_view_click.clone(),
                initial_width,
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
            if !m.is_empty() || k != Key::Escape {
                return Propagation::Proceed;
            }

            let w = c.widget().unwrap().downcast::<gtk::Entry>().unwrap();
            w.set_text(&weak.upgrade().unwrap().imp().original_text.borrow());
            w.set_position(-1);
            Propagation::Stop
        });
        element.imp().text_entry.add_controller(key);

        let click = GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);

        click.connect_pressed(move |c, _n, x, y| {
            // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
            let w = c.widget().unwrap();
            if !w.contains(x, y) {
                warn!("Workaround -- ignoring junk mouse event in {tab:?}");
                return;
            }

            tabs_run(|tlist| tlist.set_active(tab));
        });

        element.imp().scroller.vscrollbar().add_controller(click);

        Self {
            view,

            element,

            tab,
            selection: selection.clone(),

            deny_view_click,

            _signals: signals,

            connections: Vec::new(),

            completion_controllers: Vec::new(),
            completion_result: DebugIgnore::default(),

            workaround_temp_state: None,
        }
    }

    fn setup_completion(&mut self) {
        let tab = self.tab;
        let imp = self.element.imp();

        let completion = gtk::EventControllerKey::new();
        let existing = self.completion_result.clone();
        completion.connect_key_pressed(move |kc, key, _, mods| {
            let mods = mods & !ModifierType::LOCK_MASK;
            if (mods & !ModifierType::SHIFT_MASK) != ModifierType::CONTROL_MASK {
                return Propagation::Proceed;
            }

            let entry = kc.widget().unwrap().downcast::<gtk::Entry>().unwrap();

            if key == Key::z {
                if let Some(res) = existing.take() {
                    // This undoes the completion, not all changes from original_text
                    entry.set_text(&res.initial);
                    entry.set_position(-1);
                }
                return Propagation::Proceed;
            } else if key != Key::space {
                return Propagation::Proceed;
            }

            let initial: String = entry.text().to_string();

            if let Some(mut res) = existing.take()
                && res.candidates[res.position].to_string_lossy() == initial
            {
                if mods.contains(ModifierType::SHIFT_MASK) {
                    res.position =
                        min(res.position.wrapping_sub(1), res.candidates.len().saturating_sub(1));
                } else {
                    res.position = (res.position + 1) % res.candidates.len();
                }

                entry.set_text(&res.candidates[res.position].to_string_lossy());
                entry.set_position(-1);
                existing.set(Some(res));
                return Propagation::Proceed;
            }

            if mods != ModifierType::CONTROL_MASK {
                return Propagation::Proceed;
            }

            let mut path: PathBuf = initial.trim_start().into();

            if path.is_relative() {
                let Some(cwd) = gui_run(|g| g.tabs.borrow().find(tab).map(Tab::dir)) else {
                    return Propagation::Proceed;
                };
                path = cwd.to_path_buf().join(path);
            }

            gui_run(|g| g.send_manager(ManagerAction::Complete(path, initial, tab)));
            Propagation::Proceed
        });
        // Needed to catch ctrl+z
        completion.set_propagation_phase(gtk::PropagationPhase::Capture);
        imp.text_entry.add_controller(completion.clone());


        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| {
            gui_run(|g| g.send_manager(ManagerAction::CancelCompletion));
        });
        imp.text_entry.add_controller(focus.clone());


        let existing = self.completion_result.clone();
        let change_sig = imp.text_entry.connect_changed(move |_e| {
            existing.take();
            gui_run(|g| g.send_manager(ManagerAction::CancelCompletion));
        });


        self.completion_controllers
            .push(completion.upcast::<gtk::EventController>().into());
        self.completion_controllers.push(focus.upcast::<gtk::EventController>().into());

        self.connections.push(SignalHolder::new(&*imp.text_entry, change_sig));
    }

    fn setup_flat(&mut self, path: &Path) {
        self.connections.clear();
        let tab = self.tab;
        debug!("Creating new flat pane for {tab:?}: {path:?}");

        let location = path.to_string_lossy().to_string();

        self.element.flat_text(location);
        let imp = self.element.imp();

        let sig = imp.text_entry.connect_activate(move |e| {
            let path: PathBuf = e.text().into();
            tabs_run(|t| t.navigate_open_tab(tab, &path));
        });

        self.connections.push(SignalHolder::new(&*imp.text_entry, sig));
        self.setup_completion()
    }

    fn setup_search(
        &mut self,
        filter: CustomFilter,
        filtered: FilterListModel,
        (original_query, normalized_query): (Rc<RefCell<String>>, Rc<RefCell<String>>),
    ) {
        gui_run(|g| g.send_manager(ManagerAction::CancelCompletion));
        self.connections.clear();
        self.completion_controllers.clear();
        self.completion_result.take();

        let tab = self.tab;
        debug!("Creating new search pane for {tab:?}: {:?}", original_query.borrow());

        self.element.search_text(&original_query.borrow(), String::new());

        // Decent opportunity for UnsafeCell if it benchmarks better.
        let query = normalized_query.clone();
        let imp = self.element.imp();

        let filt = filter.clone();
        let signal = imp.text_entry.connect_changed(move |e| {
            let text = e.text();
            original_query.replace(text.to_string());

            let mut query = normalized_query.borrow_mut();
            let lower = text.to_lowercase();
            let new = match normalize_lowercase(&lower) {
                Cow::Borrowed(_) => lower,
                Cow::Owned(s) => s,
            };

            if new == *query || (query.len() < MIN_SEARCH && new.len() < MIN_SEARCH) {
                return;
            }

            // https://gitlab.gnome.org/GNOME/gtk/-/issues/5989
            // TODO [incremental]
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
            eo.get().name.normalized().contains(&*q)
        });

        self.connections.push(SignalHolder::new(&*imp.text_entry, signal));
    }

    pub(super) fn new_flat(
        tab: TabId,
        path: &Path,
        settings: DirSettings,
        selection: &MultiSelection,
        initial_width: u32,
        insert: impl FnOnce(&Widget),
    ) -> Self {
        let mut pane = Self::create(tab, settings, selection, initial_width);
        pane.setup_flat(path);
        pane.set_visible(false);

        // Where panes are created is controlled in TabsList
        insert(pane.element.upcast_ref());

        pane
    }

    pub(super) fn new_search<F: FnOnce(&Widget)>(
        tab: TabId,
        queries: (Rc<RefCell<String>>, Rc<RefCell<String>>),
        settings: DirSettings,
        selection: &MultiSelection,
        filter: CustomFilter,
        filtered: FilterListModel,
        initial_width: u32,
        insert: F,
    ) -> Self {
        let mut pane = Self::create(tab, settings, selection, initial_width);
        pane.setup_search(filter, filtered, queries);
        pane.set_visible(false);

        // Where panes are created is controlled in TabsList
        insert(pane.element.upcast_ref());

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

    pub(super) fn remove_from_parent(&self) {
        let parent = self.element.parent().unwrap();

        if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
            info!("Promoting sibling of pane {:?}", self.tab);

            let start = paned.start_child().unwrap();
            let end = paned.end_child().unwrap();
            paned.set_start_child(Widget::NONE);
            paned.set_end_child(Widget::NONE);

            let start_tab =
                start.downcast_ref::<PaneElement>().map(|te| *te.imp().tab.get().unwrap());
            let sibling = if start_tab == Some(self.tab) { end } else { start };
            // GTK focus tracking is just bad
            let focus = sibling.focus_child();

            // A split will always have a parent.
            let grandparent = paned.parent().unwrap();
            if let Some(grandpane) = grandparent.downcast_ref::<gtk::Paned>() {
                let pos = grandpane.position();
                let parent_is_start = grandpane
                    .start_child()
                    .unwrap()
                    .downcast_ref::<gtk::Paned>()
                    .is_some_and(|sc| sc.eq(paned));

                if focus.is_some() {
                    grandpane.grab_focus();
                }

                if parent_is_start {
                    grandpane.set_start_child(Some(&sibling));
                } else {
                    grandpane.set_end_child(Some(&sibling));
                }

                if let Some(focus) = focus {
                    focus.grab_focus();
                }
                grandpane.set_position(pos);
            } else {
                if focus.is_some() {
                    grandparent.grab_focus();
                }

                let grandparent = grandparent.downcast_ref::<gtk::Box>().unwrap();
                grandparent.remove(paned);
                grandparent.append(&sibling);

                sibling.grab_focus();
            }
        } else {
            let parent = parent.downcast_ref::<gtk::Box>().unwrap();
            parent.remove(&self.element);
        }
    }

    pub(super) fn make_end_child(&self, paned: gtk::Paned) {
        paned.set_end_child(Some(&self.element));
    }

    pub(super) fn append(&self, parent: &gtk::Box) {
        parent.append(&self.element);
    }

    pub(super) fn update_location(&mut self, path: &Path, settings: DirSettings, list: &Contents) {
        self.update_settings(settings, list);

        let location = path.to_string_lossy().to_string();
        self.element.flat_text(location);
    }

    pub(super) fn move_active_focus_to_text(&self) {
        if self.element.imp().active.get() {
            self.element.imp().text_entry.grab_focus_without_selecting();
        }
    }

    pub(super) fn update_search(&self, query: &str) {
        self.element.search_text(query, query.to_string());
    }

    pub(super) fn set_clipboard_text(&self, text: &str) {
        self.element.clipboard_text(text);
    }

    pub(super) fn focus_location_bar(&self) {
        self.element.imp().text_entry.grab_focus();
    }

    pub fn set_active(&mut self, active: bool) {
        self.element.imp().active.set(active);
        if active {
            self.element.add_css_class("active-pane");
            self.view.grab_focus();
        } else {
            self.element.remove_css_class("active-pane");
        }
    }

    // Returns true when the display mode has changed
    pub fn update_settings(&mut self, settings: DirSettings, list: &Contents) -> bool {
        self.deny_view_click.set(false);

        if self.view.matches(settings.display_mode) {
            self.view.update_settings(settings);
            return false;
        }

        let vs = self.element.is_visible().then(|| self.get_state(list));

        self.view = match settings.display_mode {
            DisplayMode::Icons => View::Icons(IconView::new(
                &self.element.imp().scroller,
                self.tab,
                &self.selection,
                self.deny_view_click.clone(),
                self.element.last_allocated_width(),
            )),
            DisplayMode::Columns => View::Columns(DetailsView::new(
                &self.element.imp().scroller,
                self.tab,
                settings,
                &self.selection,
                self.deny_view_click.clone(),
            )),
        };

        if let Some(vs) = vs {
            self.start_apply_state(vs);
        }
        true
    }

    pub fn handle_completion(&self, completion: CompletionResult) {
        let imp = self.element.imp();

        if imp.text_entry.text() != completion.initial {
            return trace!("Discarding completion result since text has changed");
        }

        imp.text_entry
            .set_text(&completion.candidates[completion.position].to_string_lossy());
        imp.text_entry.set_position(-1);
        self.completion_result.set(Some(completion));
    }

    pub fn get_state(&self, list: &Contents) -> PaneState {
        if let Some(state) = self.workaround_temp_state.as_ref() {
            info!("get_state called for Pane {:?} during pane state application delay", self.tab);
            return state.clone();
        }

        let sel = list.selection.selection();
        let focus = self.view.focus_child().map(|(_child, eo)| {
            // If exactly one item is selected, and it's the focused one, keep it focused.
            // TODO -- there's an janky GTK bug here that can cause the selection state to
            // be lost when using forwards/backwards.
            let select = sel.size() == 1
                && list.selection.item(sel.nth(0)).and_downcast::<EntryObject>().unwrap() == eo;

            FocusState { path: eo.get().abs_path.clone(), select }
        });

        let scroll = if self.element.imp().scroller.vadjustment().value() > 0.0 {
            self.view.get_scroll_target().map(|child| super::ScrollPosition {
                precise: Some(PrecisePosition {
                    position: self.element.imp().scroller.vadjustment().value(),
                    view: self.view.current_display_mode(),
                    res: (self.element.width(), self.element.height()),
                    count: list.size(),
                }),
                path: child.get().abs_path.clone(),
                index: list.filtered_position_by_sorted(&child.get()).unwrap_or_default(),
            })
        } else {
            None
        };

        PaneState { scroll, focus }
    }

    pub fn start_apply_state(&mut self, state: PaneState) {
        self.deny_view_click.set(false);
        self.workaround_temp_state = Some(state);
        self.element.imp().pane_state_after_allocate.set(true);

        // Does this need to run before allocate?
        // if scroll_flags.contains(ListScrollFlags::FOCUS) && self.element.focus_child().is_some()
        // {     self.view.grab_focus();
        // }
    }

    // Needs to run after the grid has been allocated, otherwise the scroll position gets scrambled
    pub fn finish_apply_state(&mut self, list: &Contents) {
        let Some(state) = self.workaround_temp_state.take() else {
            return;
        };

        let mut scroll_flags = ListScrollFlags::empty();

        let focus = state.focus.as_ref().and_then(|fs| {
            let eo = EntryObject::lookup(&fs.path)?;

            if state.scroll.as_ref().is_some_and(|scroll| scroll.path == fs.path) {
                scroll_flags |= ListScrollFlags::FOCUS;

                if fs.select {
                    scroll_flags |= ListScrollFlags::SELECT;
                }

                return None;
            }

            let pos = list.filtered_position_by_sorted(&eo.get())?;

            if fs.select {
                list.selection.select_item(pos, true);
            }

            Some(pos)
        });


        let mut precise = None;

        let pos = state
            .scroll
            .as_ref()
            .and_then(|sp| {
                precise =
                    sp.precise.as_ref().filter(|precise| precise.count == list.size()).cloned();

                if let Some(eo) = EntryObject::lookup(&sp.path) {
                    let pos = list.filtered_position_by_sorted(&eo.get());
                    debug!("Scrolling to position from element {pos:?}");
                    // If we have no focus state, move focus to some visible element.
                    if focus.is_none() {
                        scroll_flags |= ListScrollFlags::FOCUS;
                    }
                    pos
                } else {
                    Some(sp.index)
                }
            })
            .unwrap_or(0);

        if scroll_flags.contains(ListScrollFlags::FOCUS) && self.element.focus_child().is_some() {
            self.view.grab_focus();
        }

        if let Some(focus) = focus {
            self.view.scroll_to(focus, ListScrollFlags::FOCUS);
        }

        self.view.scroll_to(pos, scroll_flags);

        if let Some(precise) = &precise {
            let vadjust = self.element.imp().scroller.vadjustment();

            if precise.view == self.view.current_display_mode()
                && precise.res == (self.element.width(), self.element.height())
            {
                info!(
                    "Restoring precise scroll position for {:?}: {:?} / {:?}",
                    self.tab,
                    precise.position,
                    vadjust.upper()
                );

                if self.view.current_display_mode() == DisplayMode::Columns {
                    // Don't ask, it's just broken. The GTK experience.
                    // As far as I can tell, this isn't a problem for the grid view. Probably.
                    // Worth considering
                    let precise = precise.clone();
                    glib::idle_add_local_once(move || {
                        vadjust.set_value(precise.position);
                    });
                } else {
                    vadjust.set_value(precise.position);
                }
            }
        }
    }

    pub fn seek_to(&self, pos: u32) {
        let flags = ListScrollFlags::FOCUS | ListScrollFlags::SELECT;
        match &self.view {
            View::Icons(icons) => icons.scroll_to(pos, flags),
            View::Columns(details) => details.scroll_to(pos, flags),
        }
    }

    pub fn split(&self, orient: Orientation, forced: bool) -> Option<gtk::Paned> {
        let mapped = self.element.is_mapped();

        let paned = gtk::Paned::builder()
            .orientation(orient)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .visible(mapped);


        let paned = match orient {
            Orientation::Horizontal if self.element.width() > MIN_PANE_RES * 2 || forced => {
                if mapped {
                    paned.position(self.element.width() / 2)
                } else {
                    paned
                }
            }
            Orientation::Vertical if self.element.height() > MIN_PANE_RES * 2 || forced => {
                if mapped { paned.position(self.element.height() / 2) } else { paned }
            }
            Orientation::Horizontal | Orientation::Vertical => return None,
            _ => unreachable!(),
        }
        .build();
        info!("Splitting pane for {:?}", self.tab);

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

    pub fn hide_ancestors(&self) {
        let mut parent = self.element.parent().unwrap();

        while let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
            if !paned.get_visible() {
                return;
            }

            paned.set_visible(false);
            parent = paned.parent().unwrap();
        }
    }

    pub fn show_ancestors(&self) {
        let mut parent = self.element.parent().unwrap();

        let mut stack = Vec::new();
        while let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
            if paned.get_visible() {
                // This is to set a sane default position, because gtk
                if !stack.is_empty() || paned.position() == 0 {
                    stack.push(paned.clone());
                }
            } else {
                paned.set_visible(true);
            }
            parent = paned.parent().unwrap();
        }

        if stack.is_empty() {
            return;
        }

        let (mut w, mut h) = (parent.width(), parent.height());
        info!("Generating paned positions based on box res {w}x{h}");

        while let Some(paned) = stack.pop() {
            if paned.orientation() == Orientation::Horizontal {
                w /= 2;
                paned.set_position(w);
            } else {
                h /= 2;
                paned.set_position(h);
            }
        }
    }

    pub fn save_splits(&self, ids: &AHashMap<TabId, u32>) -> SavedSplit {
        // Only called in a group
        let mut parent = self.element.parent().and_downcast::<gtk::Paned>().unwrap();

        while let Some(paned) = parent.parent().and_downcast::<gtk::Paned>() {
            parent = paned;
        }

        save_tree(parent, ids)
    }

    pub fn workaround_focus_before_delete(&self, eo: &EntryObject) {
        match &self.view {
            View::Icons(grid) => grid.fix_focus_before_delete(eo),
            View::Columns(_) => error!("TODO -- handle broken focus on delete in column view"),
        }
    }
}

fn save_tree(paned: gtk::Paned, ids: &AHashMap<TabId, u32>) -> SavedSplit {
    let first = paned.first_child().unwrap();
    let second = paned.last_child().unwrap();

    let start = if let Some(pane) = first.downcast_ref::<PaneElement>() {
        SplitChild::Tab(ids[pane.imp().tab.get().unwrap()])
    } else {
        let paned = first.downcast::<gtk::Paned>().unwrap();
        SplitChild::Split(Box::new(save_tree(paned, ids)))
    };

    let end = if let Some(pane) = second.downcast_ref::<PaneElement>() {
        SplitChild::Tab(ids[pane.imp().tab.get().unwrap()])
    } else {
        let paned = second.downcast::<gtk::Paned>().unwrap();
        SplitChild::Split(Box::new(save_tree(paned, ids)))
    };

    let horizontal = paned.orientation() == Orientation::Horizontal;

    SavedSplit { horizontal, start, end }
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

fn setup_view_controllers<W: IsA<Widget>>(tab: TabId, widget: &W, deny: Rc<Cell<bool>>) {
    let click = GestureClick::new();

    click.set_button(0);
    click.connect_pressed(move |c, n, x, y| {
        let w = c.widget().unwrap();
        // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
        if !w.contains(x, y) {
            warn!("Workaround -- ignoring junk mouse event in {tab:?}");
            return;
        }

        // This part is not a workaround.
        if c.button() <= 3 && n == 1 {
            w.grab_focus();
        }

        if deny.get() {
            deny.set(false);
            return;
        }

        trace!("Mousebutton {} in pane {tab:?}", c.current_button());

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

                let point = Point::new(x as f32, y as f32);
                let point = gui_run(|g| w.compute_point(&g.window, &point)).unwrap();

                let rect = Rectangle::new(point.x() as i32, point.y() as i32, 1, 1);
                menu.set_pointing_to(Some(&rect));
                menu.popup();
            }
        });
    });

    widget.add_controller(click);
}

// Sets up various controllers that should be set only on items, and not on dead space around
// items.
fn setup_item_controllers<W: IsA<Widget>, B: IsA<Widget> + Bound, P: IsA<Widget>>(
    tab: TabId,
    widget: &W,
    bound: WeakRef<B>,
    paintable: WeakRef<P>,
    deny_view: Rc<Cell<bool>>,
) {
    let click = GestureClick::new();
    click.set_button(0);

    let b = bound.clone();
    let deny = deny_view.clone();
    click.connect_pressed(move |c, _n, x, y| {
        let eo = b.upgrade().unwrap().bound_object().unwrap();

        // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
        let w = c.widget().unwrap();
        if !w.contains(x, y) {
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
            let path = eo.get().abs_path.clone();
            if let Some(nav) = NavTarget::open_or_jump_abs(path) {
                tabs_run(|t| t.create_tab(TabPosition::After(ActionTarget::Tab(tab)), nav, false));
            }
        } else if c.current_event().unwrap().triggers_context_menu() {
            c.set_state(gtk::EventSequenceState::Claimed);
            let menu = tabs_run(|tlist| {
                tlist.set_active(tab);
                let t = tlist.find(tab).unwrap();
                t.select_if_not(eo);
                t.context_menu()
            });

            let point = Point::new(x as f32, y as f32);
            let point = gui_run(|g| w.compute_point(&g.window, &point)).unwrap();

            let rect = Rectangle::new(point.x() as i32, point.y() as i32, 1, 1);
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

    let b = bound.clone();
    drag_source.connect_prepare(move |ds, _x, _y| {
        let pw = paintable.upgrade().unwrap();
        let bw = b.upgrade().unwrap();
        let eo = bw.bound_object().unwrap();

        let provider = tabs_run(|tlist| {
            tlist.set_active(tab);
            let t = tlist.find(tab).unwrap();
            t.select_if_not(eo);
            t.content_provider(ClipboardOp::Cut)
        });

        let paintable = WidgetPaintable::new(Some(&pw));

        ds.set_icon(Some(&paintable), pw.width() / 2, pw.height() / 2);
        DRAGGING_TAB.set(Some(tab));
        Some(provider.into())
    });

    drag_source.connect_drag_end(|_ds, _drag, _n| {
        trace!("Clearing drag source");
        DRAGGING_TAB.take();
    });
    drag_source.set_propagation_phase(gtk::PropagationPhase::Capture);


    let drop_target = DropTargetAsync::new(None, DragAction::all());
    let b = bound.clone();
    drop_target.connect_accept(move |_dta, dr| {
        if !dr.formats().contain_mime_type(URIS) {
            return false;
        }

        let bw = b.upgrade().unwrap();
        let eo = bw.bound_object().unwrap();

        if !eo.get().dir() {
            return false;
        }

        true
    });

    drop_target.connect_drop(move |_dta, dr, _x, _y| {
        // Workaround for https://gitlab.gnome.org/GNOME/gtk/-/issues/6086
        warn!("Manually clearing DROP_ACTIVE flag");
        _dta.widget().unwrap().unset_state_flags(gtk::StateFlags::DROP_ACTIVE);

        let bw = bound.upgrade().unwrap();
        let eo = bw.bound_object().unwrap();

        if !eo.get().dir() {
            warn!("Ignoring drop on regular file {:?}", eo.get().dir());
            return false;
        }


        tabs_run(|tlist| {
            info!("Handling drop in {tab:?} on {:?}", &*eo.get().name);
            let t = tlist.find(tab).unwrap();

            t.drag_drop(dr, Some(eo))
        })
    });

    widget.add_controller(click);
    widget.add_controller(drag_source);
    widget.add_controller(drop_target);
}
