use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk::gio::Icon;
use gtk::glib::{self, Object};
use gtk::prelude::ObjectExt;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{ButtonExt, GtkWindowExt, WidgetExt};

use crate::com::{ChildInfo, EntryObject};
use crate::gui::Gui;

// TODO -- tabs with per-mimetype content? Image resolution, etc?

glib::wrapper! {
    pub struct PropDialog(ObjectSubclass<imp::PropDialog>)
        @extends gtk::Widget, gtk::Window;
}

// TODO -- also symlinks
impl PropDialog {
    pub(super) fn show(
        gui: &Rc<Gui>,
        cancel: Arc<AtomicBool>,
        files: Vec<EntryObject>,
        dirs: Vec<EntryObject>,
    ) -> Self {
        info!("Showing conflict resolution dialog for {} files", files.len());

        let s: Self = Object::new();
        let imp = s.imp();
        imp.cancel.set(cancel).unwrap();

        let mut size = 0;
        let mut allocated = 0;

        for f in &files {
            size += f.get().raw_size();
            allocated += f.get().allocated_size;
        }

        for d in &dirs {
            allocated += d.get().allocated_size;
        }

        imp.size.set(size);
        imp.allocated.set(allocated);

        if !dirs.is_empty() {
            imp.spinner.set_spinning(true);
            imp.spinner.set_visible(true);
        }

        if files.len() == 1 && dirs.is_empty() {
            imp.name_text.set_text(&files[0].get().name.to_string_lossy());
            imp.type_text.set_text(&files[0].get().mime);

            s.set_image(gui, &files[0]);
        } else if files.is_empty() && dirs.len() == 1 {
            imp.name_text.set_text(&dirs[0].get().name.to_string_lossy());
            imp.type_text.set_text(&dirs[0].get().mime);

            s.set_image(gui, &dirs[0]);
        } else if dirs.is_empty() {
            imp.name_label.set_text("");
            imp.name_text.set_text(&format!("{} files", files.len()));

            let mimetype = files[0].get().mime.clone();
            if files.iter().any(|f| f.get().mime != mimetype) {
                imp.type_box.set_visible(false);
                s.default_image();
            } else {
                imp.type_text.set_text(&mimetype);
                s.set_image(gui, &files[0]);
            }
        } else if files.is_empty() {
            imp.name_label.set_text("");
            imp.name_text.set_text(&format!("{} directories", dirs.len()));

            imp.type_text.set_text(&dirs[0].get().mime);
            s.set_image(gui, &dirs[0]);
        } else {
            imp.name_label.set_text("");
            imp.name_text.set_text(&format!(
                "{} directories and {} files",
                dirs.len(),
                files.len()
            ));

            imp.type_box.set_visible(false);
            s.default_image();
        }

        let w = s.downgrade();
        imp.close.connect_clicked(move |_b| {
            w.upgrade().unwrap().close();
        });

        let g = gui.clone();
        s.connect_close_request(move |pd| {
            let cancel = pd.imp().cancel.get().unwrap();
            cancel.store(true, Ordering::Relaxed);

            let mut open = g.open_dialogs.borrow_mut();

            if let Some(i) = open.properties.iter().position(|d| d.matches(cancel)) {
                open.properties.swap_remove(i);
            }

            glib::Propagation::Proceed
        });

        s.set_transient_for(Some(&gui.window));
        s.set_modal(true);
        s.set_visible(true);

        s
    }

    pub(super) fn matches(&self, id: &Arc<AtomicBool>) -> bool {
        Arc::ptr_eq(self.imp().cancel.get().unwrap(), id)
    }

    pub(super) fn add(&self, children: ChildInfo) {
        let imp = self.imp();

        imp.size.set(imp.size.get() + children.size);
        imp.allocated.set(imp.allocated.get() + children.allocated);
        imp.child_files.set(imp.child_files.get() + children.files);
        imp.child_dirs.set(imp.child_dirs.get() + children.dirs);

        if children.done {
            imp.spinner.set_spinning(false);
            imp.spinner.set_visible(false);
        }
    }

    fn set_image(&self, g: &Gui, eo: &EntryObject) {
        if let Some(tex) = eo.imp().thumbnail() {
            return self.imp().icon.set_from_paintable(Some(&tex));
        }

        if eo.imp().can_sync_thumbnail() {
            let e = eo.get();
            let tex = g.thumbnailer.sync_thumbnail(&e.abs_path, &e.mime, e.mtime);

            if let Some(tex) = tex {
                return self.imp().icon.set_from_paintable(Some(&tex));
            }
        }

        self.imp().icon.set_from_gicon(&Icon::deserialize(&eo.get().icon).unwrap());
    }

    fn default_image(&self) {
        self.imp().icon.set_from_icon_name(Some("text-x-generic"));
    }
}

mod imp {
    use std::cell::{Cell, OnceCell};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    #[derive(Default, CompositeTemplate)]
    #[template(file = "dialog.ui")]
    pub struct PropDialog {
        #[template_child]
        pub top_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub icon: TemplateChild<gtk::Image>,

        #[template_child]
        pub name_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub name_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub type_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub type_text: TemplateChild<gtk::Label>,
        // Type(s)
        // Size (on disk)

        // Count(when relevant)

        // Location -- keep search in mind

        // Mtime
        // Btime ??
        #[template_child]
        pub original_size: TemplateChild<gtk::Label>,
        #[template_child]
        pub original_mtime: TemplateChild<gtk::Label>,

        #[template_child]
        pub(super) spinner: TemplateChild<gtk::Spinner>,

        #[template_child]
        pub close: TemplateChild<gtk::Button>,

        pub cancel: OnceCell<Arc<AtomicBool>>,
        pub size: Cell<u64>,
        pub allocated: Cell<u64>,

        pub child_files: Cell<usize>,
        pub child_dirs: Cell<usize>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PropDialog {
        type ParentType = gtk::Window;
        type Type = super::PropDialog;

        const NAME: &'static str = "AwFmProperties";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for PropDialog {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WindowImpl for PropDialog {}
    impl WidgetImpl for PropDialog {}

    impl PropDialog {}
}
