use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, CompositeTemplate};

use crate::com::{EntryObject, SignalHolder};

glib::wrapper! {
    pub struct IconCell(ObjectSubclass<imp::IconCell>)
        @extends gtk::Widget, gtk::Fixed;
}

impl IconCell {
    pub(super) fn new() -> Self {
        let obj: Self = glib::Object::new();
        obj
    }

    pub fn bind(&self, obj: &EntryObject) {
        let imp = self.imp();
        imp.update_contents(obj);

        // Don't need to be weak refs
        let self_ref = self.clone();
        let id = obj.connect_local("update", false, move |entry| {
            let obj: EntryObject = entry[0].get().unwrap();
            self_ref.imp().update_contents(&obj);

            None
        });

        let d = SignalHolder::new(obj, id);

        assert!(imp.update_connection.replace(Some(d)).is_none())
    }

    pub fn unbind(&self, obj: &EntryObject) {
        obj.deprioritize_thumb();
        self.imp().bound_object.take().unwrap();
        self.imp().update_connection.take().unwrap();
    }

    pub fn bound_object(&self) -> Option<EntryObject> {
        self.imp().bound_object.borrow().clone()
    }
}


mod imp {
    use std::borrow::Cow;
    use std::cell::{Cell, RefCell};

    use chrono::{Local, TimeZone};
    use gtk::glib::SignalHandlerId;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    use crate::com::{Entry, EntryObject, SignalHolder};

    #[derive(Default, CompositeTemplate)]
    #[template(file = "icon_cell.ui")]
    pub struct IconCell {
        #[template_child]
        pub image: TemplateChild<gtk::Image>,

        pub bound_object: RefCell<Option<EntryObject>>,
        pub update_connection: Cell<Option<SignalHolder<EntryObject>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for IconCell {
        type ParentType = gtk::Widget;
        type Type = super::IconCell;

        const NAME: &'static str = "IconCell";

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for IconCell {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WidgetImpl for IconCell {}

    impl IconCell {
        pub(super) fn update_contents(&self, obj: &EntryObject) {
            let thumb = obj.thumbnail_for_display();
            // There's basically no mutation that won't cause the thumbnail
            // to be regenerated, so this is expensive but never wasted.
            if let Some(texture) = thumb {
                self.image.set_from_paintable(Some(&texture));
            } else {
                self.image.set_from_gicon(&obj.icon());
            }

            self.bound_object.replace(Some(obj.clone()));
        }
    }
}
