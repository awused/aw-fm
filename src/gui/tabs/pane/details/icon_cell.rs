use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::com::{EntryObject, SignalHolder};
use crate::gui::tabs::pane::Bound;

glib::wrapper! {
    pub struct IconCell(ObjectSubclass<imp::IconCell>)
        @extends gtk::Widget, gtk::Fixed;
}

impl Default for IconCell {
    fn default() -> Self {
        let s: Self = glib::Object::new();

        s.connect_map(|s| {
            if let Some(obj) = s.bound_object() {
                obj.mark_mapped_changed(true);
            } else {
                error!("Mapping unbound IconCell");
            }
        });

        s.connect_unmap(|s| {
            if let Some(obj) = s.bound_object() {
                obj.mark_mapped_changed(false);
            } else {
                error!("Unmapping unbound IconCell");
            }
        });

        s
    }
}


impl Bound for IconCell {
    fn bind(&self, eo: &EntryObject) {
        let imp = self.imp();
        imp.update_contents(eo);

        // Don't need to be weak refs
        let self_ref = self.clone();
        let id = eo.connect_local("update", false, move |entry| {
            let obj: EntryObject = entry[0].get().unwrap();
            self_ref.imp().update_contents(&obj);
            if self_ref.is_mapped() {
                trace!("Update for visible entry {:?} in column view", &*obj.get().name);
            }
            None
        });

        eo.mark_bound(self.is_mapped());

        let d = SignalHolder::new(eo, id);

        assert!(imp.update_connection.replace(Some(d)).is_none())
    }

    fn unbind(&self, eo: &EntryObject) {
        eo.mark_unbound(self.is_mapped());
        self.imp().bound_object.take().unwrap();
        self.imp().update_connection.take().unwrap();
    }

    fn bound_object(&self) -> Option<EntryObject> {
        self.imp().bound_object.borrow().clone()
    }
}

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::prelude::WidgetExt;
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    use crate::com::{EntryObject, SignalHolder};
    use crate::gui::tabs::pane::SYMLINK_BADGE;

    #[derive(Default, CompositeTemplate)]
    #[template(file = "icon_cell.ui")]
    pub struct IconCell {
        #[template_child]
        pub image: TemplateChild<gtk::Image>,

        #[template_child]
        pub overlay: TemplateChild<gtk::Overlay>,

        pub symlink_badge: Cell<Option<gtk::Image>>,

        // https://gitlab.gnome.org/GNOME/gtk/-/issues/4688
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
            let thumb = obj.imp().thumbnail();
            // There's basically no mutation that won't cause the thumbnail
            // to be regenerated, so this is expensive but never wasted.
            if let Some(texture) = thumb {
                self.image.set_from_paintable(Some(&texture));
            } else {
                self.image.set_from_gicon(&obj.icon());
            }

            if let Some(badge) = self.symlink_badge.take() {
                if obj.get().symlink {
                    self.symlink_badge.set(Some(badge));
                } else {
                    self.overlay.remove_overlay(&badge);
                }
            } else if obj.get().symlink {
                SYMLINK_BADGE.with(|sb| {
                    let Some(icon) = &**sb else {
                        return;
                    };

                    let badge = gtk::Image::from_gicon(icon);
                    badge.set_valign(gtk::Align::End);
                    badge.set_halign(gtk::Align::Start);

                    self.overlay.add_overlay(&badge);
                });
            }

            self.bound_object.replace(Some(obj.clone()));
        }
    }
}
