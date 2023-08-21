use std::ffi::OsString;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use gtk::gdk::Texture;
use gtk::gio::Icon;
use gtk::glib::clone::Downgrade;
use gtk::glib::{self, Object};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{ButtonExt, CheckButtonExt, GtkWindowExt, WidgetExt};
use gtk::{IconTheme, Image};

use super::Operation;
use crate::com::{Entry, EntryObject};
use crate::config::FileCollision;
use crate::gui::operations::Conflict;
use crate::gui::{gui_run, Gui};


#[derive(Debug)]
pub(super) enum FileChoice {
    Skip(bool),
    Overwrite(bool),
    AutoRename(bool),
    Newer(bool),
    Rename(OsString),
}

impl FileChoice {
    pub(super) const fn collision(&self) -> Option<(bool, FileCollision)> {
        Some(match self {
            Self::Skip(b) => (*b, FileCollision::Skip),
            Self::Overwrite(b) => (*b, FileCollision::Overwrite),
            Self::AutoRename(b) => (*b, FileCollision::Rename),
            Self::Newer(b) => (*b, FileCollision::Newer),
            Self::Rename(_) => return None,
        })
    }
}

#[derive(Debug)]
pub(super) enum DirChoice {
    Skip(bool),
    Merge(bool),
    Rename(OsString),
}

glib::wrapper! {
    pub struct AskDialog(ObjectSubclass<imp::AskDialog>)
        @extends gtk::Widget, gtk::Window;
}

impl AskDialog {
    pub(super) fn show(gui: &Rc<Gui>, op: Rc<Operation>) {
        let progress = op.progress.borrow();
        let conflict = progress.pending_pair.as_ref().unwrap();
        info!("Showing conflict resolution dialog for {conflict:?}");

        let s: Self = Object::new();

        let (src, dst) = conflict.pair();

        s.set_original(src);
        s.set_new(dst);


        match conflict {
            Conflict::Directory(src, dst) => {
                s.imp().use_rest.set_label(Some("Apply to all remaining directories"));
                s.dir_buttons(&op);
            }
            Conflict::File(..) => {
                s.imp().use_rest.set_label(Some("Apply to all remaining files"));
                todo!()
            }
        }

        let o = op.clone();
        s.connect_close_request(move |w| {
            o.cancel();
            glib::Propagation::Proceed
        });

        let o = op.clone();
        let w = s.downgrade();
        s.imp().cancel.connect_clicked(move |c| {
            let s = w.upgrade().unwrap();
            o.cancel();
            s.destroy();
        });


        s.set_transient_for(Some(&gui.window));
        s.set_modal(true);
        s.set_visible(true);
    }

    fn set_original(&self, p: &Arc<Path>) {
        if let Some(eo) = EntryObject::lookup(p) {
            let e = eo.get();
            Self::set_image(&self.imp().original_icon, &e, eo.imp().thumbnail());
            self.imp().original_size.set_text(&e.long_size_string());
            self.imp().original_mtime.set_text(&e.mtime.seconds_string())
        } else if let Ok((e, needs_count)) = Entry::new(p.clone()) {
            Self::set_image(&self.imp().original_icon, &e, None);
            self.imp().original_size.set_text(&e.long_size_string());
            self.imp().original_mtime.set_text(&e.mtime.seconds_string())
        } else {
            self.imp().original_size.set_text("???");
        };
    }

    fn set_new(&self, p: &Path) {
        if let Some(eo) = EntryObject::lookup(p) {
            let e = eo.get();
            Self::set_image(&self.imp().new_icon, &e, eo.imp().thumbnail());
            self.imp().new_size.set_text(&e.long_size_string());
            self.imp().new_mtime.set_text(&e.mtime.seconds_string())
        } else if let Ok((e, needs_count)) = Entry::new(p.into()) {
            Self::set_image(&self.imp().new_icon, &e, None);
            self.imp().new_size.set_text(&e.long_size_string());
            self.imp().new_mtime.set_text(&e.mtime.seconds_string())
        } else {
            self.imp().new_size.set_text("???");
        };
    }

    fn set_image(image: &Image, entry: &Entry, tex: Option<Texture>) {
        if let Some(tex) = tex {
            return image.set_from_paintable(Some(&tex));
        }

        if !entry.dir() {
            if let Some(tex) =
                gui_run(|g| g.thumbnailer.sync_thumbnail(&entry.abs_path, &entry.mime, entry.mtime))
            {
                return image.set_from_paintable(Some(&tex));
            }
        }

        image.set_from_gicon(&Icon::deserialize(&entry.icon).unwrap());
    }

    fn dir_buttons(&self, op: &Rc<Operation>) {
        self.imp().merge.set_visible(true);

        let w = self.downgrade();
        let o = op.clone();
        self.imp().skip.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();
            info!(
                "Selected skip({}) for resolving directory conflict",
                s.imp().use_rest.is_active()
            );
            s.destroy();

            o.progress
                .borrow_mut()
                .set_directory_strat(DirChoice::Skip(s.imp().use_rest.is_active()));
            o.clone().process_next();
        });

        let w = self.downgrade();
        let o = op.clone();
        self.imp().merge.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();
            info!(
                "Selected merge({}) for resolving directory conflict",
                s.imp().use_rest.is_active()
            );
            s.destroy();

            o.progress
                .borrow_mut()
                .set_directory_strat(DirChoice::Merge(s.imp().use_rest.is_active()));
            o.clone().process_next();
        });
    }
}

mod imp {
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    #[derive(Default, CompositeTemplate)]
    #[template(file = "ask.ui")]
    pub struct AskDialog {
        #[template_child]
        pub top_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub original_icon: TemplateChild<gtk::Image>,
        #[template_child]
        pub original_size: TemplateChild<gtk::Label>,
        #[template_child]
        pub original_mtime: TemplateChild<gtk::Label>,

        #[template_child]
        pub new_icon: TemplateChild<gtk::Image>,
        #[template_child]
        pub new_size: TemplateChild<gtk::Label>,
        #[template_child]
        pub new_mtime: TemplateChild<gtk::Label>,

        #[template_child]
        pub use_rest: TemplateChild<gtk::CheckButton>,

        #[template_child]
        pub cancel: TemplateChild<gtk::Button>,

        #[template_child]
        pub skip: TemplateChild<gtk::Button>,
        #[template_child]
        pub merge: TemplateChild<gtk::Button>,
        //     #[template_child]
        //     pub newer: TemplateChild<gtk::Button>,
        //     #[template_child]
        //     pub rename: TemplateChild<gtk::Button>,
        //     #[template_child]
        //     pub auto_rename: TemplateChild<gtk::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AskDialog {
        type ParentType = gtk::Window;
        type Type = super::AskDialog;

        const NAME: &'static str = "AwFmAskDialog";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AskDialog {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WindowImpl for AskDialog {}
    impl WidgetImpl for AskDialog {}

    impl AskDialog {}
}
