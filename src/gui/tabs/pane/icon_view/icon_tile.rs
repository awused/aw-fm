use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;

use crate::com::{EntryObject, SignalHolder};
use crate::gui::PANGO_ATTRIBUTES;
use crate::gui::tabs::pane::Bound;


glib::wrapper! {
    pub struct IconTile(ObjectSubclass<imp::IconTile>)
        @extends gtk::Widget;
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

impl Bound for IconTile {
    fn bind(&self, eo: &EntryObject) {
        eo.mark_bound(self.is_mapped());

        let imp = self.imp();

        // Name can never change, only set it once.
        {
            let entry = eo.get();
            // let disp_string = entry.name.to_string_lossy();
            imp.name.set_text(Some(&entry.name.to_string_lossy()));

            // Seems to cause it to lock up completely in large directories with sorting?
            // Absolutely tanks performance either way.
            // imp.name.set_tooltip_text(Some(&disp_string));
        }

        imp.bound_object.replace(Some(eo.clone()));
        imp.update_contents(eo);

        // Don't need to be weak refs
        let self_ref = self.clone();
        let id = eo.connect_local("update", false, move |entry| {
            let obj: EntryObject = entry[0].get().unwrap();
            self_ref.imp().update_contents(&obj);
            if self_ref.is_mapped() {
                trace!("Update for visible entry {:?} in icon view", &*obj.get().name);
            }
            None
        });

        let d = SignalHolder::new(eo, id);
        assert!(imp.update_connection.replace(Some(d)).is_none())
    }

    fn unbind(&self, eo: &EntryObject) {
        eo.mark_unbound(self.is_mapped());
        self.imp().bound_object.take().unwrap();
        self.imp().update_connection.take().unwrap();
        self.imp().image.clear();
    }

    fn bound_object(&self) -> Option<EntryObject> {
        self.imp().bound_object.borrow().clone()
    }
}

mod imp {
    use std::cell::{Cell, RefCell};
    use std::sync::OnceLock;

    use gtk::prelude::WidgetExt;
    use gtk::subclass::prelude::*;
    use gtk::{CompositeTemplate, Orientation, glib};

    use crate::com::{EntryObject, SignalHolder, Thumbnail};
    use crate::gui::tabs::pane::SYMLINK_BADGE;

    #[derive(Default, CompositeTemplate)]
    #[template(file = "icon_tile.ui")]
    pub struct IconTile {
        #[template_child]
        pub inner_box: TemplateChild<gtk::Box>,

        #[template_child]
        pub image: TemplateChild<gtk::Image>,
        #[template_child]
        pub name: TemplateChild<gtk::Inscription>,
        #[template_child]
        pub size: TemplateChild<gtk::Inscription>,

        #[template_child]
        pub overlay: TemplateChild<gtk::Overlay>,

        pub symlink_badge: Cell<Option<gtk::Image>>,

        // https://gitlab.gnome.org/GNOME/gtk/-/issues/4688
        pub bound_object: RefCell<Option<EntryObject>>,
        pub update_connection: Cell<Option<SignalHolder<EntryObject>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for IconTile {
        type ParentType = gtk::Widget;
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

    static RES: OnceLock<(i32, i32)> = OnceLock::new();

    impl WidgetImpl for IconTile {
        fn measure(&self, orientation: Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let minimums = if let Some(m) = RES.get() {
                m
            } else {
                let width = self.inner_box.measure(Orientation::Horizontal, -1).0;
                let height = self.inner_box.measure(Orientation::Vertical, -1).0;
                if width > 0 && height > 0 {
                    info!("Measured grid tile res as {width}x{height}");
                    RES.set((width, height)).unwrap()
                }
                &(width, height)
            };

            match orientation {
                Orientation::Horizontal => (minimums.0, minimums.0, -1, -1),
                Orientation::Vertical => (minimums.1, minimums.1, -1, -1),
                _ => unreachable!(),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.inner_box.allocate(width, height, baseline, None);
        }
    }

    impl IconTile {
        pub(super) fn update_contents(&self, obj: &EntryObject) {
            // The overhead of checking if the texture/icon is unchanged is too high compared to
            // the savings. Would want to move thumbnails to a separate signal if it matters.
            match obj.thumbnail_or_defer() {
                Thumbnail::Texture(texture) => self.image.set_paintable(Some(&texture)),
                Thumbnail::None => self.image.set_from_gicon(&obj.icon()),
                Thumbnail::Pending => self.image.clear(),
            }

            let entry = obj.get();

            if let Some(badge) = self.symlink_badge.take() {
                if entry.symlink.is_some() {
                    self.symlink_badge.set(Some(badge));
                } else {
                    self.overlay.remove_overlay(&badge);
                }
            } else if entry.symlink.is_some() {
                SYMLINK_BADGE.with(|sb| {
                    let Some(icon) = sb else {
                        return;
                    };

                    let badge = gtk::Image::from_gicon(icon);
                    badge.set_valign(gtk::Align::End);
                    badge.set_halign(gtk::Align::Start);

                    self.overlay.add_overlay(&badge);
                });
            }

            let size_string = entry.long_size_string();
            if !matches!(self.size.text(), Some(existing) if existing.as_str() == size_string) {
                self.size.set_text(Some(&size_string));
            }
        }
    }
}
