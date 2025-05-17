use std::fmt::Write;
use std::ops::Deref;
use std::time::Duration;

use StackChild::*;
use gtk::gdk::{DragAction, Key, ModifierType};
use gtk::glib::Propagation;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{DropTargetAsync, EventControllerFocus, GestureClick, MultiSelection, glib};
use strum_macros::{AsRefStr, EnumString};

use super::DRAGGING_TAB;
use crate::com::{ActionTarget, SignalHolder};
use crate::gui::clipboard::URIS;
use crate::gui::tabs::id::TabId;
use crate::gui::{Selected, gui_run, tabs_run};

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
#[allow(dead_code)]
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
        imp.text_entry.set_position(-1);
    }

    pub(super) fn flat_text(&self, location: String) {
        let imp = self.imp();
        imp.text_entry.set_text(&location);
        imp.original_text.replace(location);
        imp.seek.set_text("");
        imp.stack.set_visible_child_name(&Count);
        imp.text_entry.set_position(-1);
    }

    pub(super) fn clipboard_text(&self, text: &str) {
        let imp = self.imp();
        imp.clipboard.set_text(text);
        imp.clipboard.set_tooltip_text(Some(text));

        if imp.stack.visible_child_name().is_some_and(|n| n != *Seek) {
            imp.stack.set_visible_child_name(&Clipboard);
        }
    }

    fn add_controllers(&self, tab: TabId) {
        let focus = EventControllerFocus::new();
        focus.connect_enter(move |focus| {
            debug!("Focus entered {tab:?}");
            let element = focus.widget().unwrap().downcast::<Self>().unwrap();
            if !element.imp().active.get() {
                // We set active when focus enters but deliberately do not unset it when focus
                // leaves. If no other tab grabs focus we keep active set.
                tabs_run(|t| t.set_active(tab));
            }
        });
        self.add_controller(focus);

        // Configurable mouse buttons
        let config_actions = GestureClick::new();
        config_actions.set_button(0);
        config_actions.connect_pressed(move |c, _n, x, y| {
            // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
            let w = c.widget().unwrap();
            if !w.contains(x, y) {
                warn!("Workaround -- ignoring junk mouse event in {tab:?}");
                return;
            }

            let target = ActionTarget::Tab(tab);
            gui_run(|g| g.run_mouse_command(target, c.current_button(), c.current_event_state()));
        });
        self.add_controller(config_actions);

        let drop_target = DropTargetAsync::new(None, DragAction::all());
        drop_target.connect_accept(move |_dta, dr| {
            if DRAGGING_TAB.get() == Some(tab) {
                info!("Ignoring drag into same tab");
                return false;
            }

            if !dr.formats().contain_mime_type(URIS) {
                return false;
            }


            tabs_run(|tlist| {
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
            })
        });

        drop_target.connect_drop(move |_dta, dr, _x, _y| {
            // Workaround for https://gitlab.gnome.org/GNOME/gtk/-/issues/6086
            warn!("Manually clearing DROP_ACTIVE flag");
            _dta.widget().unwrap().unset_state_flags(gtk::StateFlags::DROP_ACTIVE);

            tabs_run(|tlist| {
                info!("Handling drop in {tab:?}");
                let t = tlist.find(tab).unwrap();

                t.drag_drop(dr, None)
            })
        });
        self.imp().scroller.add_controller(drop_target);

        let seek_controller = gtk::EventControllerKey::new();
        seek_controller.connect_key_pressed(move |kc, key, _, mods| {
            let pane = kc.widget().and_then(|w| w.parent()).and_downcast::<Self>().unwrap();
            pane.handle_seek(key, mods)
        });
        self.imp().scroller.add_controller(seek_controller);
    }

    fn update_selected_text(&self, list: &MultiSelection) {
        let imp = self.imp();

        if let Some(id) = imp.selection_text_update.take() {
            id.remove();
        }

        // Not perfectly efficient, but we'll only run Selected::from twice in the worst case.
        self.maybe_close_seek(list);

        let mut sel = Selected::from(list);
        let len = sel.len();
        if len == 0 {
            imp.selection.set_text("");

            if imp.stack.visible_child_name().is_some_and(|n| n != *Count && n != *Seek) {
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

        if imp.stack.visible_child_name().is_some_and(|n| n != *Selection && n != *Seek) {
            imp.stack.set_visible_child_name(&Selection);
        }
    }

    fn defer_selected_text_update(&self, list: &MultiSelection) {
        let imp = self.imp();

        if imp.stack.visible_child_name().is_none_or(|n| n != *Selection) {
            return;
        }

        if let Some(id) = imp.selection_text_update.take() {
            id.remove();
        }

        // Handle updating the selected text ("N items selected, 50GB")
        // Always defer these until there's >50ms of idle time.
        // Even if one was scheduled, we removed it earlier.
        let w = self.downgrade();
        let list = list.downgrade();
        imp.selection_text_update.set(Some(glib::timeout_add_local_once(
            Duration::from_millis(50),
            move || {
                let Some(s) = w.upgrade() else {
                    return;
                };
                s.imp().selection_text_update.take();

                if s.imp().stack.visible_child_name().is_none_or(|n| n != *Selection) {
                    return;
                }

                let Some(list) = list.upgrade() else {
                    return;
                };

                trace!(
                    "Running deferred selection text update in {:?}",
                    s.imp().tab.get().unwrap()
                );

                s.update_selected_text(&list);
            },
        )));
    }

    pub(super) fn setup_signals(&self, selection: &MultiSelection) -> PaneSignals {
        self.imp().count.set_text(&format!("{} items", selection.n_items()));

        let w = self.downgrade();
        let count_signal = selection.connect_items_changed(move |list, _p, removed, _added| {
            let s = w.upgrade().unwrap();

            s.maybe_close_seek(list);
            s.imp().count.set_text(&format!("{} items", list.n_items()));

            // There is no selection_changed event on item removal
            if removed > 0 {
                s.defer_selected_text_update(list)
            }
        });

        let count_signal = SignalHolder::new(selection, count_signal);

        self.update_selected_text(selection);

        let w = self.downgrade();
        let selection_signal = selection.connect_selection_changed(move |list, _p, _n| {
            w.upgrade().unwrap().update_selected_text(list)
        });
        let selection_signal = SignalHolder::new(selection, selection_signal);

        PaneSignals(count_signal, selection_signal)
    }

    fn maybe_close_seek(&self, list: &MultiSelection) {
        let imp = self.imp();
        if imp.stack.visible_child_name().is_none_or(|n| n != *Seek) {
            return;
        }

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

        debug!("Closing seek in {:?}", imp.tab.get().unwrap());
        imp.seek.set_text("");
    }

    fn handle_seek(&self, key: Key, mods: ModifierType) -> Propagation {
        let seek = &self.imp().seek;
        let stack = &self.imp().stack;
        let tab = *self.imp().tab.get().unwrap();

        if !mods.difference(ModifierType::SHIFT_MASK).is_empty() {
            return Propagation::Proceed;
        }

        let seek_visible = stack.visible_child_name().is_some_and(|n| n == *Seek);

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
        // There are unicode spaces that could matter, but not for me
        // is_whitespace() would not be appropriate
        if c != ' ' && !c.is_alphanumeric() {
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

    use gtk::glib::SourceId;
    use gtk::subclass::prelude::*;
    use gtk::{CompositeTemplate, glib};
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
        pub selection_text_update: Cell<Option<SourceId>>,
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
            humansize::SizeFormatter::new(bytes, humansize::WINDOWS)
        );
    }
    label
}
