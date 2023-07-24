use std::fmt::Write;
use std::path::{Path, PathBuf};

use gtk::gio::ListStore;
use gtk::glib::GString;
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
    pub(super) fn new(tab: TabId, path: &Path) -> Self {
        let s: Self = glib::Object::new();

        let imp = s.imp();

        imp.tab.set(tab).unwrap();
        s.flat_title(path);

        s
    }

    pub fn clone_from(&self, other: &Self) {
        let imp = self.imp();
        let other_imp = other.imp();

        imp.title.set_text(&other_imp.title.text());
        let tooltip = other_imp.title.tooltip_text();
        imp.title.set_tooltip_text(tooltip.as_ref().map(GString::as_str));

        imp.spinner.set_spinning(other_imp.spinner.is_spinning());
        imp.spinner.set_visible(other_imp.spinner.is_visible());
    }

    pub fn flat_title(&self, path: &Path) {
        let imp = self.imp();

        // let mut last_two_dirs = PathBuf::new();
        // match (path.parent().and_then(Path::file_name), path.file_name()) {
        //     (Some(parent), Some(dir)) => {
        //         let mut two_segments = PathBuf::from(parent);
        //         two_segments.push(dir);
        //         imp.title.set_text(&two_segments.to_string_lossy())
        //     }
        //     _ => {
        //         imp.title.set_text(&path.to_string_lossy());
        //     }
        // }

        imp.title
            .set_text(&path.file_name().unwrap_or(path.as_os_str()).to_string_lossy());
        imp.title.set_tooltip_text(Some(&path.to_string_lossy()));
    }

    pub fn set_pane_visible(&self, visible: bool) {
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
