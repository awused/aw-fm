use std::borrow::Cow;
use std::cell::Cell;
use std::ffi::OsStr;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use gtk::gdk::Texture;
use gtk::gio::Icon;
use gtk::glib::{self, Object};
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::Image;

use super::Operation;
use crate::com::{Entry, EntryObject};
use crate::config::FileCollision;
use crate::gui::operations::ConflictKind;
use crate::gui::{gui_run, Gui};


#[derive(Debug)]
pub(super) enum FileChoice {
    Skip(bool),
    Overwrite(bool),
    AutoRename(bool),
    Newer(bool),
}

impl FileChoice {
    pub(super) const fn collision(&self) -> (bool, FileCollision) {
        match self {
            Self::Skip(b) => (*b, FileCollision::Skip),
            Self::Overwrite(b) => (*b, FileCollision::Overwrite),
            Self::AutoRename(b) => (*b, FileCollision::Rename),
            Self::Newer(b) => (*b, FileCollision::Newer),
        }
    }
}

#[derive(Debug)]
pub(super) enum DirChoice {
    Skip(bool),
    Merge(bool),
}

glib::wrapper! {
    pub struct AskDialog(ObjectSubclass<imp::AskDialog>)
        @extends gtk::Widget, gtk::Window;
}

impl AskDialog {
    pub(super) fn show(gui: &Rc<Gui>, op: Rc<Operation>) {
        let progress = op.progress.borrow();
        let con = progress.conflict.as_ref().unwrap();
        info!("Showing conflict resolution dialog for {con:?}");

        let s: Self = Object::new();

        s.set_original(&con.src);
        s.set_new(&con.dst);

        match con.kind {
            ConflictKind::DirDir => {
                s.imp().use_rest.set_label(Some("Apply to all remaining directories"));
                s.dir_buttons(&op);
            }
            ConflictKind::FileFile => {
                s.imp().use_rest.set_label(Some("Apply to all remaining files"));
                s.file_buttons(&op);
            }
        };

        // Not flexible for file -> dir or dir -> file
        let top_text = if con.src.file_name() == con.dst.file_name() {
            format!(
                "Destination {} {:?} already exists",
                con.kind.dst_str(),
                con.dst.file_name().unwrap_or(con.dst.as_os_str())
            )
        } else {
            format!(
                "Destination {} {:?} (source: {:?}) already exists",
                con.kind.dst_str(),
                con.dst.file_name().unwrap_or(con.dst.as_os_str()),
                con.src.file_name().unwrap_or(con.src.as_os_str())
            )
        };
        s.imp().top_text.set_text(&top_text);

        let o = op.clone();
        let w = s.downgrade();
        let k = con.kind;
        s.imp().manual_rename.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();

            let t = s.imp().name_override.buffer().text();
            if t.is_empty() {
                return;
            }

            info!("Selected Rename({t:?}) for resolving {} conflict", k.dst_str());
            s.destroy();

            o.progress.borrow_mut().conflict_rename(&t);
            o.clone().process_next();
        });

        // src, not dst, in case this is a repeat
        let fname = con.src.file_name().map_or(Cow::Borrowed(""), OsStr::to_string_lossy);

        s.imp().name_override.set_text(&fname);

        let end_pos = if let Some(stem) = con.dst.file_stem() {
            stem.to_string_lossy().chars().count() as i32
        } else {
            -1
        };

        let w = s.downgrade();
        let signal = Cell::new(op.cancellable.connect_cancelled_local(move |_c| {
            if let Some(s) = w.upgrade() {
                s.destroy();
            }
        }));

        let o = op.clone();
        s.connect_close_request(move |_w| {
            o.cancel();
            glib::Propagation::Stop
        });

        let o = op.clone();
        s.imp().cancel.connect_clicked(move |_b| {
            o.cancel();
        });

        let o = op.clone();
        s.connect_destroy(move |_s| {
            if let Some(signal) = signal.take() {
                o.cancellable.disconnect_cancelled(signal);
            }
        });

        s.set_transient_for(Some(&gui.window));
        s.set_modal(true);
        s.set_visible(true);

        s.imp().name_override.set_enable_undo(true);
        s.imp().name_override.select_region(0, end_pos);
    }

    fn set_original(&self, p: &Arc<Path>) {
        if let Some(eo) = EntryObject::lookup(p) {
            let e = eo.get();
            Self::set_image(
                &self.imp().original_icon,
                &e,
                eo.imp().thumbnail(),
                eo.imp().can_sync_thumbnail(),
            );
            self.imp().original_size.set_text(&e.long_size_string());
            self.imp().original_mtime.set_text(&e.mtime.seconds_string())
        } else if let Ok((e, _needs_count)) = Entry::new(p.clone()) {
            Self::set_image(&self.imp().original_icon, &e, None, true);
            self.imp().original_size.set_text(&e.long_size_string());
            self.imp().original_mtime.set_text(&e.mtime.seconds_string())
        } else {
            self.imp().original_icon.set_from_icon_name(Some("text-x-generic"));
            self.imp().original_size.set_text("???");
        };
    }

    fn set_new(&self, p: &Path) {
        if let Some(eo) = EntryObject::lookup(p) {
            let e = eo.get();
            Self::set_image(
                &self.imp().new_icon,
                &e,
                eo.imp().thumbnail(),
                eo.imp().can_sync_thumbnail(),
            );
            self.imp().new_size.set_text(&e.long_size_string());
            self.imp().new_mtime.set_text(&e.mtime.seconds_string())
        } else if let Ok((e, _needs_count)) = Entry::new(p.into()) {
            Self::set_image(&self.imp().new_icon, &e, None, true);
            self.imp().new_size.set_text(&e.long_size_string());
            self.imp().new_mtime.set_text(&e.mtime.seconds_string())
        } else {
            self.imp().new_icon.set_from_icon_name(Some("text-x-generic"));
            self.imp().new_size.set_text("???");
        };
    }

    fn set_image(image: &Image, entry: &Entry, tex: Option<Texture>, can_sync_thumbnail: bool) {
        if let Some(tex) = tex {
            return image.set_from_paintable(Some(&tex));
        }

        if can_sync_thumbnail {
            let tex =
                gui_run(|g| g.thumbnailer.sync_thumbnail(&entry.abs_path, entry.mime, entry.mtime));

            if let Some(tex) = tex {
                return image.set_from_paintable(Some(&tex));
            }
        }

        image.set_from_gicon(&Icon::deserialize(&entry.icon).unwrap());
    }

    fn dir_strat(&self, op: &Rc<Operation>, choice: impl FnOnce(bool) -> DirChoice) {
        let choice = choice(self.imp().use_rest.is_active());
        info!("Selected {choice:?} for resolving directory conflict",);
        self.destroy();

        op.progress.borrow_mut().set_directory_strat(choice);
        op.clone().process_next();
    }

    fn dir_buttons(&self, op: &Rc<Operation>) {
        self.imp().merge.set_visible(true);

        let w = self.downgrade();
        let o = op.clone();
        self.imp().skip.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();
            s.dir_strat(&o, DirChoice::Skip)
        });

        let w = self.downgrade();
        let o = op.clone();
        self.imp().merge.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();
            s.dir_strat(&o, DirChoice::Merge)
        });
    }

    fn file_strat(&self, op: &Rc<Operation>, choice: impl FnOnce(bool) -> FileChoice) {
        let choice = choice(self.imp().use_rest.is_active());
        info!("Selected {choice:?} for resolving file conflict",);
        self.destroy();

        op.progress.borrow_mut().set_file_strat(choice);
        op.clone().process_next();
    }

    fn file_buttons(&self, op: &Rc<Operation>) {
        self.imp().newer.set_visible(true);
        self.imp().auto_rename.set_visible(true);
        self.imp().overwrite.set_visible(true);

        let w = self.downgrade();
        let o = op.clone();
        self.imp().skip.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();
            s.file_strat(&o, FileChoice::Skip)
        });

        let w = self.downgrade();
        let o = op.clone();
        self.imp().newer.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();
            s.file_strat(&o, FileChoice::Newer)
        });

        let w = self.downgrade();
        let o = op.clone();
        self.imp().auto_rename.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();
            s.file_strat(&o, FileChoice::AutoRename)
        });

        let w = self.downgrade();
        let o = op.clone();
        self.imp().overwrite.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();
            s.file_strat(&o, FileChoice::Overwrite)
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
        pub name_override: TemplateChild<gtk::Entry>,
        #[template_child]
        pub manual_rename: TemplateChild<gtk::Button>,

        #[template_child]
        pub cancel: TemplateChild<gtk::Button>,

        #[template_child]
        pub skip: TemplateChild<gtk::Button>,
        #[template_child]
        pub merge: TemplateChild<gtk::Button>,

        #[template_child]
        pub newer: TemplateChild<gtk::Button>,
        #[template_child]
        pub auto_rename: TemplateChild<gtk::Button>,
        #[template_child]
        pub overwrite: TemplateChild<gtk::Button>,
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
