use gtk::glib::Object;
use gtk::{glib, Application};


glib::wrapper! {
    pub struct MainWindow(ObjectSubclass<imp::MainWindow>)
        @extends gtk::Widget, gtk::ApplicationWindow, gtk::Window;
}

impl MainWindow {
    pub fn new(app: &Application) -> Self {
        // Create new window
        Object::builder().property("application", app).build()
    }
}

mod imp {
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};


    #[derive(Default, CompositeTemplate)]
    #[template(file = "main_window.ui")]
    pub struct MainWindow {
        #[template_child]
        pub overlay: TemplateChild<gtk::Overlay>,

        #[template_child]
        pub tabs: TemplateChild<gtk::ListView>,

        #[template_child]
        pub panes: TemplateChild<gtk::Box>,

        #[template_child]
        pub toast: TemplateChild<gtk::Label>,

        #[template_child]
        pub bookmarks: TemplateChild<gtk::Box>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MainWindow {
        type ParentType = gtk::ApplicationWindow;
        type Type = super::MainWindow;

        const NAME: &'static str = "AwFmMainWindow";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for MainWindow {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WindowImpl for MainWindow {}
    impl ApplicationWindowImpl for MainWindow {}
    impl WidgetImpl for MainWindow {}

    impl MainWindow {}
}
