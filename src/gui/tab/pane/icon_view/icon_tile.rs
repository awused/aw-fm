use gtk::glib;
use gtk::pango::ffi::pango_attr_insert_hyphens_new;
use gtk::pango::{AttrInt, AttrList};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::WidgetExt;

use crate::com::EntryObject;


thread_local! {
   static PANGO_ATTRIBUTES: AttrList = {
        let pango_list = AttrList::new();
        pango_list.insert(AttrInt::new_insert_hyphens(false));
        pango_list
   }
}

mod imp {
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(file = "icon_tile.ui")]
    pub struct IconTile {
        #[template_child]
        pub image: TemplateChild<gtk::Image>,
        #[template_child]
        pub name: TemplateChild<gtk::Inscription>,
        // pub name: TemplateChild<gtk::Label>,
        #[template_child]
        pub size: TemplateChild<gtk::Inscription>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for IconTile {
        type ParentType = gtk::Box;
        type Type = super::IconTile;

        const NAME: &'static str = "IconTile";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for IconTile {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WidgetImpl for IconTile {}
    impl BoxImpl for IconTile {}
}

glib::wrapper! {
    pub struct IconTile(ObjectSubclass<imp::IconTile>)
        @extends gtk::Widget, gtk::Fixed;
}

impl Default for IconTile {
    fn default() -> Self {
        Self::new()
    }
}

impl IconTile {
    pub fn new() -> Self {
        let s: Self = glib::Object::new();
        PANGO_ATTRIBUTES.with(|pa| s.imp().name.set_attributes(Some(pa)));
        s
    }

    pub fn set_entry(&self, entry: &EntryObject) {
        let imp = self.imp();

        imp.image.set_from_gicon(entry.icon());

        // TODO -- do something about this to_string_lossy, especially if we're never using the
        // OsString portion anywhere.
        let disp_string = entry.name.to_string_lossy();
        imp.name.set_text(Some(&disp_string));
        // imp.name.set_text(&disp_string);
        imp.name.set_tooltip_text(Some(&disp_string));

        imp.size.set_text(Some(&entry.long_size_string()));
    }
}
