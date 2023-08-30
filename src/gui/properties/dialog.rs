use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk::gio::Icon;
use gtk::glib::{self, Object};
use gtk::prelude::ObjectExt;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{ButtonExt, GtkWindowExt, WidgetExt};
use num_format::{Locale, ToFormattedString};

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
        location: &Path,
        search: bool,
        cancel: Arc<AtomicBool>,
        files: Vec<EntryObject>,
        dirs: Vec<EntryObject>,
    ) -> Self {
        info!("Showing conflict resolution dialog for {} files", files.len());

        let s: Self = Object::new();
        let imp = s.imp();
        imp.cancel.set(cancel).unwrap();

        if !search {
            imp.location.set_text(&location.to_string_lossy());
        } else {
            imp.location.set_text(&format!("Search in {}", location.to_string_lossy()));
        }

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

        if dirs.is_empty() {
            imp.children_box.set_visible(false);
            imp.spinner.stop();
            imp.spinner.set_visible(false);
        } else {
            imp.spinner.start();
            imp.spinner.set_visible(true);
        }

        if files.len() == 1 && dirs.is_empty() {
            imp.name_text.set_text(&files[0].get().name.to_string_lossy());
            imp.type_text.set_text(&files[0].get().mime);
            imp.mtime_text.set_text(&files[0].get().mtime.seconds_string());

            s.set_image(gui, &files[0]);
        } else if files.is_empty() && dirs.len() == 1 {
            imp.name_text.set_text(&dirs[0].get().name.to_string_lossy());
            imp.type_text.set_text(&dirs[0].get().mime);
            imp.mtime_text.set_text(&dirs[0].get().mtime.seconds_string());

            s.set_image(gui, &dirs[0]);
        } else if dirs.is_empty() {
            imp.mtime_box.set_visible(false);

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
            imp.mtime_box.set_visible(false);

            imp.name_label.set_text("");
            imp.name_text.set_text(&format!("{} directories", dirs.len()));

            imp.type_text.set_text(&dirs[0].get().mime);
            s.set_image(gui, &dirs[0]);
        } else {
            imp.type_box.set_visible(false);
            imp.mtime_box.set_visible(false);

            imp.name_label.set_text("");
            imp.name_text.set_text(&format!(
                "{} directories and {} files",
                dirs.len(),
                files.len()
            ));

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
        s.update_text();

        s
    }

    pub(super) fn matches(&self, id: &Arc<AtomicBool>) -> bool {
        Arc::ptr_eq(self.imp().cancel.get().unwrap(), id)
    }

    pub(super) fn add_children(&self, children: ChildInfo) {
        let imp = self.imp();

        imp.size.set(imp.size.get() + children.size);
        imp.allocated.set(imp.allocated.get() + children.allocated);
        imp.child_files.set(imp.child_files.get() + children.files);
        imp.child_dirs.set(imp.child_dirs.get() + children.dirs);

        if children.done {
            imp.spinner.stop();
            imp.spinner.set_visible(false);
        }

        self.update_text();
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

    fn update_text(&self) {
        let imp = self.imp();
        let dirs = imp.child_dirs.get();
        let files = imp.child_files.get();

        if dirs > 0 && files > 0 {
            imp.children_text.set_text(&format!(
                "{dirs} director{} and {files} file{}",
                if dirs > 1 { "ies" } else { "y" },
                if files > 1 { "s" } else { "" }
            ));
        } else if dirs > 0 {
            imp.children_text
                .set_text(&format!("{dirs} director{}", if dirs > 1 { "ies" } else { "y" }));
        } else if files > 0 {
            imp.children_text
                .set_text(&format!("{files} file{}", if files > 1 { "s" } else { "" }));
        } else if !imp.spinner.is_spinning() {
            imp.children_text.set_text("Nothing");
        }

        imp.size_text.set_text(&format!(
            "{} ({} bytes)",
            humansize::format_size(imp.size.get(), humansize::WINDOWS),
            imp.size.get().to_formatted_string(&Locale::en)
        ));
        imp.allocated_text.set_text(&format!(
            "{} ({} bytes)",
            humansize::format_size(imp.allocated.get(), humansize::WINDOWS),
            imp.allocated.get().to_formatted_string(&Locale::en)
        ));
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
        pub icon: TemplateChild<gtk::Image>,

        #[template_child]
        pub name_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub name_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub type_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub type_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub children_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub spinner: TemplateChild<gtk::Spinner>,
        #[template_child]
        pub children_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub size_text: TemplateChild<gtk::Label>,
        #[template_child]
        pub allocated_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub location: TemplateChild<gtk::Label>,

        #[template_child]
        pub mtime_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub mtime_text: TemplateChild<gtk::Label>,

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
