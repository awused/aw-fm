use gtk::pango::ffi::pango_attr_insert_hyphens_new;
use gtk::pango::{AttrInt, AttrList};
use gtk::prelude::{Cast, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{AccessibleExt, EventControllerExt, GestureExt, WidgetExt};
use gtk::{glib, EventController, GestureClick};

use crate::com::{Disconnector, EntryObject};


thread_local! {
   static PANGO_ATTRIBUTES: AttrList = {
        let pango_list = AttrList::new();
        pango_list.insert(AttrInt::new_insert_hyphens(false));
        pango_list
   }
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
    #[template(file = "icon_tile.ui")]
    pub struct IconTile {
        #[template_child]
        pub image: TemplateChild<gtk::Image>,
        #[template_child]
        pub name: TemplateChild<gtk::Inscription>,
        // pub name: TemplateChild<gtk::Label>,
        #[template_child]
        pub size: TemplateChild<gtk::Inscription>,

        pub update_connection: Cell<Option<Disconnector<EntryObject>>>,
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

    impl IconTile {
        pub(super) fn update_contents(&self, obj: &EntryObject) {
            let thumb = obj.thumbnail_for_display();
            // There's basically no mutation that won't cause the thumbnail
            // to be regenerated, so this is expensive but never wasted.
            if let Some(texture) = thumb {
                self.image.set_from_paintable(Some(&texture));
            } else {
                self.image.set_from_gicon(&obj.icon());
            }

            let entry = obj.get();


            let size_string = entry.long_size_string();
            if !matches!(self.size.text(), Some(existing) if existing.as_str() == size_string) {
                self.size.set_text(Some(&size_string));
            }
        }
    }
}

glib::wrapper! {
    pub struct IconTile(ObjectSubclass<imp::IconTile>)
        @extends gtk::Widget, gtk::Box;
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
        s
    }

    pub fn bind(&self, obj: &EntryObject) {
        let imp = self.imp();

        // Name can never change, only set it once.
        {
            let entry = obj.get();
            let disp_string = entry.name.to_string_lossy();
            imp.name.set_text(Some(&entry.name.to_string_lossy()));

            // Seems to cause it to lock up completely in large directories with sorting?
            // Absolutely tanks performance either way.
            // self.name.set_tooltip_text(Some(&disp_string));
        }

        imp.update_contents(obj);

        // Don't need to be weak refs
        let self_ref = self.clone();
        let id = obj.connect_local("update", false, move |entry| {
            let obj: EntryObject = entry[0].get().unwrap();
            self_ref.imp().update_contents(&obj);
            trace!("Update for visible entry {:?} in icon view", &*obj.get().name);
            None
        });

        let d = Disconnector::new(obj, id);
        assert!(imp.update_connection.replace(Some(d)).is_none())
    }

    pub fn unbind(&self, obj: &EntryObject) {
        obj.deprioritize_thumb();
        self.imp().update_connection.take().unwrap();
    }

    pub fn assert_disconnected(&self) {
        assert!(self.imp().update_connection.take().is_none());
    }
}
