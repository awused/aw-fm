use std::fmt::Write;

use gtk::gio::ListStore;
use gtk::prelude::{Cast, ListModelExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{EditableExt, EntryExt, GestureSingleExt, SelectionModelExt, WidgetExt};
use gtk::{glib, Bitset, EventControllerFocus, GestureClick, MultiSelection, Widget};

use crate::com::{EntryObject, SignalHolder};
use crate::gui::tabs::contents::Contents;
use crate::gui::tabs::id::TabId;
use crate::gui::tabs::tab::Tab;
use crate::gui::tabs_run;

glib::wrapper! {
    pub struct TabElement(ObjectSubclass<imp::AwFmTab>)
        @extends gtk::Widget, gtk::Box;
}

impl TabElement {
    pub(super) fn new(tab: TabId, title: &str) -> Self {
        let s: Self = glib::Object::new();

        let imp = s.imp();

        imp.tab.set(tab).unwrap();
        imp.title.set_text(title);
        imp.title.set_tooltip_text(Some(title));

        s
    }

    pub fn clone_from(&self, other: &Self) {
        let imp = self.imp();
        let other_imp = other.imp();

        let text = other_imp.title.text();
        imp.title.set_text(&text);
        imp.title.set_tooltip_text(Some(&text));

        imp.spinner.set_spinning(other_imp.spinner.is_spinning());
        imp.spinner.set_visible(other_imp.spinner.is_visible());
    }

    pub fn set_title(&self, title: &str) {
        let imp = self.imp();
        imp.title.set_text(title);
        imp.title.set_tooltip_text(Some(title));
    }

    pub fn set_tab_visible(&self, visible: bool) {
        if visible {
            self.add_css_class("visible-tab");
        } else {
            self.remove_css_class("visible-tab");
        }
    }

    pub fn set_active(&self, active: bool) {
        if active {
            self.add_css_class("active-tab");
        } else {
            self.remove_css_class("active-tab");
        }
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
    pub struct AwFmTab {
        #[template_child]
        pub title: TemplateChild<gtk::Label>,

        #[template_child]
        pub spinner: TemplateChild<gtk::Spinner>,

        pub tab: OnceCell<TabId>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AwFmTab {
        type ParentType = gtk::Box;
        type Type = super::TabElement;

        const NAME: &'static str = "AwFmTab";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AwFmTab {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl BoxImpl for AwFmTab {}
    impl WidgetImpl for AwFmTab {}

    impl AwFmTab {}
}
