use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, CompositeTemplate};

use crate::com::EntryObject;

mod imp {
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(file = "string_cell.ui")]
    pub struct StringCell {
        #[template_child]
        pub contents: TemplateChild<gtk::Inscription>,
        // #[template_child]
        // pub name2: TemplateChild<gtk::Label>,
        // #[template_child]
        // pub description: TemplateChild<gtk::Label>,
        // #[template_child]
        // pub image2: TemplateChild<gtk::Image>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StringCell {
        type ParentType = gtk::Widget;
        type Type = super::StringCell;

        const NAME: &'static str = "StringCell";

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for StringCell {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WidgetImpl for StringCell {}
}

glib::wrapper! {
    pub struct StringCell(ObjectSubclass<imp::StringCell>)
        @extends gtk::Widget, gtk::Fixed;
}

impl Default for StringCell {
    fn default() -> Self {
        Self::new()
    }
}

impl StringCell {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn align_end(&self, chars: u32) {
        self.imp().contents.set_xalign(1.0);
        self.imp().contents.set_min_chars(chars);
    }

    pub fn set_text(&self, text: &str, tooltip: Option<&str>) {
        let imp = self.imp();
        // TODO -- do something about this to_string_lossy
        imp.contents.set_text(Some(text));
        imp.contents.set_tooltip_text(tooltip);
        // imp.name.set_text(&app_info.name());
        // imp.name2.set_text(&app_info.name());
        // if let Some(desc) = entry.description() {
        // imp.description.set_text(&desc);
        // }
        // let start = Instant::now();
        // imp.image.set_from_icon_name(Some("text-rust"));
        // println!("icon load {:?}", start.elapsed());
        // imp.image2.set_from_gicon(&icon);
    }
}
