use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};

glib::wrapper! {
    pub struct Application(ObjectSubclass<imp::Application>)
        @extends gtk::Widget, gtk::Box;
}

impl Default for Application {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl Application {
    pub fn set_info(&self, info: &gio::AppInfo) {
        let imp = self.imp();
        imp.name.set_text(&info.name());

        if let Some(icon) = info.icon() {
            imp.image.set_from_gicon(&icon);
        }
    }
}


mod imp {
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(file = "application.ui")]
    pub struct Application {
        #[template_child]
        pub name: TemplateChild<gtk::Label>,

        #[template_child]
        pub image: TemplateChild<gtk::Image>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Application {
        type ParentType = gtk::Box;
        type Type = super::Application;

        const NAME: &'static str = "Application";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Application {}
    impl WidgetImpl for Application {}
    impl BoxImpl for Application {}
}
