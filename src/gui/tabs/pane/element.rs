use std::fmt::Write;

use gtk::gdk::{ModifierType, Rectangle};
use gtk::prelude::{Cast, ListModelExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{
    EditableExt, EventControllerExt, GestureSingleExt, PopoverExt, SelectionModelExt, WidgetExt,
};
use gtk::{glib, EventControllerFocus, GestureClick, MultiSelection};

use crate::com::SignalHolder;
use crate::gui::tabs::id::TabId;
use crate::gui::tabs::list::event_run_tab;
use crate::gui::tabs::tab::Tab;
use crate::gui::{gui_run, tabs_run, Selected};

glib::wrapper! {
    pub struct PaneElement(ObjectSubclass<imp::Pane>)
        @extends gtk::Widget, gtk::Box;
}

// Contains signals attached to something else, not tied to the lifecycle of this Pane.
#[derive(Debug)]
pub(super) struct PaneSignals(SignalHolder<MultiSelection>, SignalHolder<MultiSelection>);

impl PaneElement {
    pub(super) fn new(tab: TabId, selection: &MultiSelection) -> (Self, PaneSignals) {
        let s: Self = glib::Object::new();
        let signals = s.setup_signals(selection);

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
        s.add_controller(focus);

        // Maps forward/back on a mouse to Forward/Backward
        let forward_back_mouse = GestureClick::new();
        forward_back_mouse.set_button(0);
        forward_back_mouse.connect_pressed(move |c, _n, x, y| {
            // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
            let alloc = c.widget().allocation();
            if !(x > 0.0 && (x as i32) < alloc.width() && y > 0.0 && (y as i32) < alloc.height()) {
                error!("Workaround -- ignoring junk mouse event in {tab:?}");
                return;
            }

            match c.current_button() {
                8 => event_run_tab(tab, Tab::back),
                9 => event_run_tab(tab, Tab::forward),
                _ => {}
            }
        });
        s.add_controller(forward_back_mouse);

        let imp = s.imp();

        imp.tab.set(tab).unwrap();
        imp.text_entry.set_enable_undo(true);
        imp.stack.set_visible_child_name("count");

        s.connect_destroy(move |_| trace!("Pane for {tab:?} destroyed"));
        (s, signals)
    }

    pub(super) fn setup_signals(&self, selection: &MultiSelection) -> PaneSignals {
        let count_label = &*self.imp().count;
        let selection_label = &*self.imp().selection;
        let stack = &*self.imp().stack;

        let count = count_label.clone();

        count.set_text(&format!("{} items", selection.n_items()));
        let stk = stack.clone();
        let count_signal = selection.connect_items_changed(move |list, _p, _a, _r| {
            // There is no selection_changed event on item removal
            // selection().size() is comparatively expensive but unavoidable.
            if stk.visible_child_name().map_or(false, |n| n != "count")
                && (list.n_items() == 0 || list.selection().size() == 0)
            {
                stk.set_visible_child_name("count");
            }
            count.set_text(&format!("{} items", list.n_items()));
        });
        let count_signal = SignalHolder::new(selection, count_signal);

        let selected = selection_label.clone();
        let stk = stack.clone();
        let update_selected = move |selection: &MultiSelection, _p: u32, _n: u32| {
            let mut sel = Selected::from(selection);
            let len = sel.len();
            if len == 0 && stk.visible_child_name().map_or(false, |n| n != "count") {
                stk.set_visible_child_name("count");
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

            selected.set_text(&text);
            selected.set_tooltip_text(Some(&text));

            if stk.visible_child_name().map_or(false, |n| n != "selection") {
                stk.set_visible_child_name("selection");
            }
        };

        update_selected(selection, 0, 0);

        let selection_signal = selection.connect_selection_changed(update_selected);

        let selection_signal = SignalHolder::new(selection, selection_signal);

        PaneSignals(count_signal, selection_signal)
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
        pub stack: TemplateChild<gtk::Stack>,

        #[template_child]
        pub count: TemplateChild<gtk::Label>,

        #[template_child]
        pub selection: TemplateChild<gtk::Label>,

        #[template_child]
        pub seek: TemplateChild<gtk::Label>,

        #[template_child]
        pub clipboard: TemplateChild<gtk::Label>,

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
