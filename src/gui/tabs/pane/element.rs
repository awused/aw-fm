use std::fmt::Write;
use std::ops::Deref;

use gtk::gdk::{DragAction, Key, ModifierType};
use gtk::glib::Propagation;
use gtk::prelude::{Cast, CastNone, ListModelExt, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{
    EditableExt, EventControllerExt, GestureSingleExt, SelectionModelExt, WidgetExt,
};
use gtk::{glib, DropTargetAsync, EventControllerFocus, GestureClick, MultiSelection};
use strum_macros::{AsRefStr, EnumString};
use StackChild::*;

use super::DRAGGING_TAB;
use crate::com::SignalHolder;
use crate::gui::clipboard::URIS;
use crate::gui::tabs::id::TabId;
use crate::gui::tabs::list::event_run_tab;
use crate::gui::tabs::tab::Tab;
use crate::gui::{tabs_run, Selected};

glib::wrapper! {
    pub struct PaneElement(ObjectSubclass<imp::Pane>)
        @extends gtk::Widget, gtk::Box;
}

#[derive(AsRefStr, EnumString, Eq, PartialEq)]
#[strum(serialize_all = "lowercase")]
enum StackChild {
    Count,
    Selection,
    Seek,
    Clipboard,
}

impl Deref for StackChild {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}


// Contains signals attached to something else, not tied to the lifecycle of this Pane.
#[derive(Debug)]
pub(super) struct PaneSignals(SignalHolder<MultiSelection>, SignalHolder<MultiSelection>);

impl PaneElement {
    pub(super) fn new(tab: TabId, selection: &MultiSelection) -> (Self, PaneSignals) {
        let s: Self = glib::Object::new();
        s.imp().tab.set(tab).unwrap();

        let signals = s.setup_signals(selection);

        s.add_controllers(tab);

        s.imp().text_entry.set_enable_undo(true);
        s.imp().stack.set_visible_child_name(&Count);

        s.connect_destroy(move |_| trace!("Pane for {tab:?} destroyed"));
        (s, signals)
    }

    pub(super) fn search_text(&self, text: &str, original: String) {
        let imp = self.imp();
        imp.text_entry.set_text(text);
        imp.original_text.replace(original);
        imp.seek.set_text("");
        imp.stack.set_visible_child_name(&Count);
    }

    pub(super) fn flat_text(&self, location: String) {
        let imp = self.imp();
        imp.text_entry.set_text(&location);
        imp.original_text.replace(location);
        imp.seek.set_text("");
        imp.stack.set_visible_child_name(&Count);
    }

    pub(super) fn clipboard_text(&self, text: &str) {
        let imp = self.imp();
        imp.clipboard.set_text(text);
        imp.clipboard.set_tooltip_text(Some(text));

        if imp.stack.visible_child_name().map_or(false, |n| n != *Seek) {
            imp.stack.set_visible_child_name(&Clipboard);
        }
    }

    fn add_controllers(&self, tab: TabId) {
        let focus = EventControllerFocus::new();
        focus.connect_enter(move |focus| {
            debug!("Focus entered {tab:?}");
            let element = focus.widget().downcast::<Self>().unwrap();
            if !element.imp().active.get() {
                // We set active when focus enters but deliberately do not unset it when focus
                // leaves. If no other tab grabs focus we keep active set.
                tabs_run(|t| t.set_active(tab));
            }
        });
        self.add_controller(focus);

        // Maps forward/back on a mouse to Forward/Backward
        let forward_back_mouse = GestureClick::new();
        forward_back_mouse.set_button(0);
        forward_back_mouse.connect_pressed(move |c, _n, x, y| {
            // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
            let w = c.widget();
            if !(x > 0.0 && (x as i32) < w.width() && y > 0.0 && (y as i32) < w.height()) {
                warn!("Workaround -- ignoring junk mouse event in {tab:?}");
                return;
            }

            match c.current_button() {
                8 => event_run_tab(tab, Tab::back),
                9 => event_run_tab(tab, Tab::forward),
                _ => {}
            }
        });
        self.add_controller(forward_back_mouse);

        let drop_target = DropTargetAsync::new(None, DragAction::all());
        drop_target.connect_accept(move |_dta, dr| {
            if DRAGGING_TAB.get() == Some(tab) {
                info!("Ignoring drag into same tab");
                return false;
            }

            if !dr.formats().contain_mime_type(URIS) {
                return false;
            }

            let accepts_paste = tabs_run(|tlist| {
                let tab = tlist.find(tab).unwrap();
                if let Some(dragging_tab) = DRAGGING_TAB.get() {
                    if let Some(dragging) = tlist.find(dragging_tab) {
                        if dragging.matches_arc(&tab.dir()) {
                            info!("Ignoring drag into same directory");
                            return false;
                        }
                    }
                }
                tab.accepts_paste()
            });

            if !accepts_paste {
                return false;
            }

            true
        });

        drop_target.connect_drop(move |_dta, dr, _x, _y| {
            // Workaround for https://gitlab.gnome.org/GNOME/gtk/-/issues/6086
            warn!("Manually clearing DROP_ACTIVE flag");
            _dta.widget().unset_state_flags(gtk::StateFlags::DROP_ACTIVE);

            tabs_run(|tlist| {
                info!("Handling drop in {tab:?}");
                let t = tlist.find(tab).unwrap();

                t.drag_drop(dr, None)
            })
        });

        self.imp().scroller.add_controller(drop_target);

        let seek_controller = gtk::EventControllerKey::new();
        seek_controller.connect_key_pressed(move |kc, key, _, mods| {
            let pane = kc.widget().parent().and_downcast::<Self>().unwrap();
            pane.handle_seek(key, mods)
        });

        self.imp().scroller.add_controller(seek_controller);
    }

    pub(super) fn setup_signals(&self, selection: &MultiSelection) -> PaneSignals {
        self.imp().count.set_text(&format!("{} items", selection.n_items()));

        let w = self.downgrade();
        let count_signal = selection.connect_items_changed(move |list, _p, _a, _r| {
            let s = w.upgrade().unwrap();
            let imp = s.imp();

            s.maybe_close_seek(list);

            // There is no selection_changed event on item removal
            // selection().size() is comparatively expensive but unavoidable.

            if imp.stack.visible_child_name().map_or(false, |n| n != *Count && n != *Seek)
                && (list.n_items() == 0 || list.selection().size() == 0)
            {
                imp.stack.set_visible_child_name(&Count);
            }

            imp.count.set_text(&format!("{} items", list.n_items()));
        });
        let count_signal = SignalHolder::new(selection, count_signal);

        let w = self.downgrade();
        let update_selected = move |selection: &MultiSelection, _p: u32, _n: u32| {
            let s = w.upgrade().unwrap();
            let imp = s.imp();

            // Not perfectly efficient, but we'll only run Selected::from twice in the worst case.
            s.maybe_close_seek(selection);

            let mut sel = Selected::from(selection);
            let len = sel.len();
            if len == 0 {
                imp.selection.set_text("");

                if imp.stack.visible_child_name().map_or(false, |n| n != *Count && n != *Seek) {
                    imp.stack.set_visible_child_name(&Count);
                }
                return;
            }

            let text = if len == 1 {
                let eo = sel.next().unwrap();
                let entry = eo.get();

                format!(
                    "\"{}\" selected ({}{})",
                    entry.name.to_string_lossy(),
                    if entry.dir() { "containing " } else { "" },
                    entry.long_size_string()
                )
            } else {
                // Costly, but not unbearably slow at <20ms for 100k items.
                selected_string(sel)
            };

            imp.selection.set_text(&text);
            imp.selection.set_tooltip_text(Some(&text));

            if imp.stack.visible_child_name().map_or(false, |n| n != *Selection && n != *Seek) {
                imp.stack.set_visible_child_name(&Selection);
            }
        };

        update_selected(selection, 0, 0);

        let selection_signal = selection.connect_selection_changed(update_selected);

        let selection_signal = SignalHolder::new(selection, selection_signal);

        PaneSignals(count_signal, selection_signal)
    }

    fn maybe_close_seek(&self, list: &MultiSelection) {
        let imp = self.imp();
        let tab = *imp.tab.get().unwrap();

        if imp.stack.visible_child_name().map_or(false, |n| n == *Seek) {
            let mut sel = Selected::from(list);
            if sel.len() == 0 {
                imp.stack.set_visible_child_name(&Count);
            } else if sel.len() > 1
                || !sel.next().unwrap().matches_seek(&imp.seek.text().to_lowercase())
            {
                imp.stack.set_visible_child_name(&Selection);
            } else {
                return;
            }

            debug!("Closing seek in {tab:?}");
            imp.seek.set_text("");
        }
    }

    fn handle_seek(&self, key: Key, mods: ModifierType) -> Propagation {
        let seek = &self.imp().seek;
        let stack = &self.imp().stack;
        let tab = *self.imp().tab.get().unwrap();

        if !mods.difference(ModifierType::SHIFT_MASK).is_empty() {
            return Propagation::Proceed;
        }

        let seek_visible = stack.visible_child_name().map_or(false, |n| n == *Seek);

        if seek_visible {
            if key == Key::Tab || key == Key::ISO_Left_Tab {
                let t = seek.text();
                if mods.is_empty() {
                    debug!("Seek next \"{t}\" in {tab:?}");
                    tabs_run(move |tlist| {
                        tlist.find_mut(tab).unwrap().seek_next(&t);
                    });
                } else if mods.contains(ModifierType::SHIFT_MASK) {
                    tabs_run(move |tlist| {
                        debug!("Seek prev \"{t}\" in {tab:?}");
                        tlist.find_mut(tab).unwrap().seek_prev(&t);
                    });
                }
                return Propagation::Stop;
            }

            let handled = if key == Key::BackSpace {
                let t = seek.text();
                let mut chars = t.chars();
                chars.next_back();
                seek.set_text(chars.as_str());
                // No need to do any seek here?
                true
            } else if key == Key::Escape {
                seek.set_text("");
                true
            } else {
                false
            };

            if handled {
                if seek.text().is_empty() {
                    debug!("Closing seek in {tab:?}");
                    if self.imp().selection.text().is_empty() {
                        stack.set_visible_child_name(&Count);
                    } else {
                        stack.set_visible_child_name(&Selection);
                    }
                }
                return Propagation::Stop;
            }
        }

        let Some(c) = key.to_unicode() else {
            return Propagation::Proceed;
        };

        // Allow ^ for prefix matching?
        if !c.is_alphanumeric() {
            return Propagation::Proceed;
        }

        let mut t = seek.text().to_string();
        t.push(c);
        seek.set_text(&t);

        if !seek_visible {
            debug!("Opening seek in {tab:?}");
            stack.set_visible_child_name(&Seek);
        }

        debug!("Seek to \"{t}\" in {tab:?}");
        tabs_run(move |tlist| {
            tlist.find_mut(tab).unwrap().seek(&t);
        });
        Propagation::Stop
    }
}

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};
    use once_cell::unsync::OnceCell;

    use crate::gui::tabs::id::TabId;

    #[derive(Default, CompositeTemplate)]
    #[template(file = "element.ui")]
    pub struct Pane {
        #[template_child]
        pub text_entry: TemplateChild<gtk::Entry>,

        #[template_child]
        pub scroller: TemplateChild<gtk::ScrolledWindow>,

        #[template_child]
        pub(super) stack: TemplateChild<gtk::Stack>,

        #[template_child]
        pub(super) count: TemplateChild<gtk::Label>,

        #[template_child]
        pub(super) selection: TemplateChild<gtk::Label>,

        #[template_child]
        pub(super) seek: TemplateChild<gtk::Label>,

        #[template_child]
        pub(super) clipboard: TemplateChild<gtk::Label>,

        pub active: Cell<bool>,
        pub original_text: RefCell<String>,
        pub tab: OnceCell<TabId>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Pane {
        type ParentType = gtk::Box;
        type Type = super::PaneElement;

        const NAME: &'static str = "Pane";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Pane {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl BoxImpl for Pane {}
    impl WidgetImpl for Pane {}

    impl Pane {}
}


fn selected_string(selected: Selected) -> String {
    let len = selected.len();
    let mut dirs = 0;
    let mut bytes = 0;
    let mut contents = 0;
    for obj in selected {
        let entry = obj.get();

        if entry.dir() {
            dirs += 1;
            contents += entry.raw_size();
        } else {
            bytes += entry.raw_size();
        }
    }

    let mut label = String::new();
    if dirs > 0 {
        let _r = write!(
            &mut label,
            "{dirs} folder{} selected (containing {contents} items)",
            if dirs > 1 { "s" } else { "" }
        );
        if dirs < len {
            let _r = write!(&mut label, ", ");
        }
    }

    if dirs < len {
        let _r = write!(
            &mut label,
            "{} file{} selected ({})",
            len - dirs,
            if len - dirs > 1 { "s" } else { "" },
            humansize::format_size(bytes, humansize::WINDOWS)
        );
    }
    label
}
