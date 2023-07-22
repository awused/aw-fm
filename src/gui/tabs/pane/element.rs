use std::fmt::Write;
use std::time::Instant;

use gtk::gio::ListStore;
use gtk::prelude::{Cast, ListModelExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{
    EditableExt, EntryExt, EventControllerExt, GestureSingleExt, SelectionModelExt, WidgetExt,
};
use gtk::{glib, Bitset, EventControllerFocus, GestureClick, MultiSelection, Widget};

use crate::com::{EntryObject, SignalHolder};
use crate::gui::tabs::contents::Contents;
use crate::gui::tabs::id::TabId;
use crate::gui::tabs::tab::Tab;
use crate::gui::tabs_run;

glib::wrapper! {
    pub struct PaneElement(ObjectSubclass<imp::Pane>)
        @extends gtk::Widget, gtk::Box;
}

#[derive(Debug)]
pub(super) struct PaneSignals(SignalHolder<MultiSelection>, SignalHolder<MultiSelection>);

impl PaneElement {
    pub(super) fn new(tab: TabId, selection: &MultiSelection) -> (Self, PaneSignals) {
        let s: Self = glib::Object::new();
        let signals = s.setup_signals(tab, selection);

        let focus = EventControllerFocus::new();
        focus.connect_enter(move |focus| {
            debug!("Focus entered {tab:?}");
            let element = focus.widget().downcast::<Self>().unwrap();
            tabs_run(|t| t.set_active(tab));
        });
        s.add_controller(focus);

        // Maps forward/back on a mouse to Forward/Backward
        let forward_back_mouse = GestureClick::new();
        forward_back_mouse.set_button(0);
        forward_back_mouse.connect_pressed(move |c, n, _x, _y| match c.current_button() {
            8 => error!("TODO backwards for mouse pane {tab:?}"),
            9 => error!("TODO forwards for mouse pane {tab:?}"),
            _ => {}
        });
        s.add_controller(forward_back_mouse);

        let imp = s.imp();

        imp.tab.set(tab).unwrap();
        imp.text_entry.set_enable_undo(true);

        (s, signals)
    }

    fn setup_signals(&self, tab_id: TabId, selection: &MultiSelection) -> PaneSignals {
        let count_label = &*self.imp().count;
        let selection_label = &*self.imp().selection;

        let count = count_label.clone();
        let selected = selection_label.clone();
        let count_signal = selection.connect_items_changed(move |list, _p, _a, _r| {
            // There is no selection_changed event on item removal
            // selection().size() is comparatively expensive but unavoidable.
            if !count.get_visible() && (list.n_items() == 0 || list.selection().size() == 0) {
                selected.set_visible(false);
                count.set_visible(true);
            }
            count.set_text(&format!("{} items", list.n_items()));
        });
        let count_signal = SignalHolder::new(selection, count_signal);

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
            selected.set_text(&selected_string(selection, &set));
        };

        update_selected(selection, 0, 0);

        let selection_signal = selection.connect_selection_changed(update_selected);

        let selection_signal = SignalHolder::new(selection, selection_signal);

        PaneSignals(count_signal, selection_signal)
    }
}

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::gdk::Texture;
    use gtk::glib::SignalHandlerId;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};
    use once_cell::unsync::OnceCell;

    use crate::com::{EntryObject, SignalHolder, Thumbnail};
    use crate::gui::tabs::id::TabId;

    #[derive(Default, CompositeTemplate)]
    #[template(file = "element.ui")]
    pub struct Pane {
        #[template_child]
        pub text_entry: TemplateChild<gtk::Entry>,

        #[template_child]
        pub scroller: TemplateChild<gtk::ScrolledWindow>,

        #[template_child]
        pub count: TemplateChild<gtk::Label>,

        #[template_child]
        pub selection: TemplateChild<gtk::Label>,

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
