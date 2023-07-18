use gtk::glib::Object;
use gtk::prelude::ObjectExt;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, Application};

use crate::com::{Disconnector, EntryObject};


glib::wrapper! {
    pub struct MainWindow(ObjectSubclass<imp::MainWindow>)
        @extends gtk::Widget, gtk::ApplicationWindow, gtk::Window;
}

impl MainWindow {
    pub fn new(app: &Application) -> Self {
        // Create new window
        Object::builder().property("application", app).build()
    }

    // TODO
    // Do not start drag and drop unless the mouse is actually "on" something, and not just
    // dead space.
    //
    //
    // let click = GestureClick::new();
    // click.connect_pressed(|a, b, c, d| {
    //     let parent = a.widget().downcast::<Self>().unwrap();
    //     parent.imp().image.bounds()
    //     println!("Click on {a:?}, {b:?}, {c:?}, {d:?}");
    //     let ev = a.current_event().unwrap();
    //
    //     let up = a.upcast_ref::<EventController>();
    //     up.current_event();
    //     a.set_state(gtk::EventSequenceState::Claimed);
    // });
    // s.add_controller(click);
}

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::gdk::Texture;
    use gtk::glib::SignalHandlerId;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    use crate::com::{Disconnector, EntryObject, Thumbnail};

    #[derive(Default, CompositeTemplate)]
    #[template(file = "main_window.ui")]
    pub struct MainWindow {
        #[template_child]
        pub tabs: TemplateChild<gtk::ListView>,

        #[template_child]
        pub panes: TemplateChild<gtk::Box>,

        #[template_child]
        pub toast: TemplateChild<gtk::Label>,
        // #[template_child]
        // pub error_message: TemplateChild<gtk::Label>,
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
