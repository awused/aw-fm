use gtk::pango::ffi::pango_attr_insert_hyphens_new;
use gtk::pango::{AttrInt, AttrList};
use gtk::prelude::{Cast, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{AccessibleExt, EventControllerExt, GestureExt, WidgetExt};
use gtk::{glib, EventController, GestureClick};

use crate::com::{EntryObject, SignalHolder};


thread_local! {
   static PANGO_ATTRIBUTES: AttrList = {
        let pango_list = AttrList::new();
        pango_list.insert(AttrInt::new_insert_hyphens(false));
        pango_list
   }
}


glib::wrapper! {
    pub struct IconTile(ObjectSubclass<imp::IconTile>)
        @extends gtk::Widget, gtk::Box;
}

impl Default for IconTile {
    fn default() -> Self {
        let s: Self = glib::Object::new();
        PANGO_ATTRIBUTES.with(|pa| s.imp().name.set_attributes(Some(pa)));

        s.connect_map(|s| {
            if let Some(obj) = s.bound_object() {
                obj.mark_mapped_changed(true);
            } else {
                error!("Mapping unbound IconTile");
            }
        });

        s.connect_unmap(|s| {
            if let Some(obj) = s.bound_object() {
                obj.mark_mapped_changed(false);
            } else {
                error!("Unmapping unbound IconTile");
            }
        });

        s
    }
}

impl IconTile {
    pub fn bind(&self, eo: &EntryObject) {
        let imp = self.imp();

        // Name can never change, only set it once.
        {
            let entry = eo.get();
            let disp_string = entry.name.to_string_lossy();
            imp.name.set_text(Some(&entry.name.to_string_lossy()));

            // Seems to cause it to lock up completely in large directories with sorting?
            // Absolutely tanks performance either way.
            // self.name.set_tooltip_text(Some(&disp_string));
        }

        imp.update_contents(eo);

        // Don't need to be weak refs
        let self_ref = self.clone();
        let id = eo.connect_local("update", false, move |entry| {
            let obj: EntryObject = entry[0].get().unwrap();
            self_ref.imp().update_contents(&obj);
            trace!("Update for visible entry {:?} in icon view", &*obj.get().name);
            None
        });

        eo.mark_bound(self.is_mapped());

        let d = SignalHolder::new(eo, id);
        assert!(imp.update_connection.replace(Some(d)).is_none())
    }

    pub fn unbind(&self, eo: &EntryObject) {
        eo.mark_unbound(self.is_mapped());
        self.imp().bound_object.take().unwrap();
        self.imp().update_connection.take().unwrap();
    }

    pub fn bound_object(&self) -> Option<EntryObject> {
        self.imp().bound_object.borrow().clone()
    }
}

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::gdk::Texture;
    use gtk::glib::SignalHandlerId;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    use crate::com::{EntryObject, SignalHolder, Thumbnail};

    #[derive(Default, CompositeTemplate)]
    #[template(file = "icon_tile.ui")]
    pub struct IconTile {
        #[template_child]
        pub image: TemplateChild<gtk::Image>,
        #[template_child]
        pub name: TemplateChild<gtk::Inscription>,
        #[template_child]
        pub size: TemplateChild<gtk::Inscription>,

        // https://gitlab.gnome.org/GNOME/gtk/-/issues/4688
        pub bound_object: RefCell<Option<EntryObject>>,
        pub update_connection: Cell<Option<SignalHolder<EntryObject>>>,
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
            // There's basically no mutation that won't cause the thumbnail
            // to be regenerated, so this is expensive but never wasted.
            if let Some(texture) = obj.imp().thumbnail() {
                self.image.set_from_paintable(Some(&texture));
            } else {
                self.image.set_from_gicon(&obj.icon());
            }

            self.bound_object.replace(Some(obj.clone()));

            let entry = obj.get();


            let size_string = entry.long_size_string();
            if !matches!(self.size.text(), Some(existing) if existing.as_str() == size_string) {
                self.size.set_text(Some(&size_string));
            }
        }
    }
}
