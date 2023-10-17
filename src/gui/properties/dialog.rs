use std::borrow::Cow;
use std::cell::Cell;
use std::error::Error;
use std::fs::{File, Permissions};
use std::os::unix::prelude::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gtk::gio::Icon;
use gtk::glib::{self, Object};
use gtk::prelude::{CheckButtonExt, ObjectExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{ButtonExt, GtkWindowExt, WidgetExt};
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, ImageDecoder};
use num_format::{Locale, ToFormattedString};
use users::{get_group_by_gid, get_user_by_uid};

use crate::com::{ChildInfo, EntryObject};
use crate::gui::{show_error, show_warning, Gui};

glib::wrapper! {
    pub struct PropDialog(ObjectSubclass<imp::PropDialog>)
        @extends gtk::Widget, gtk::Window;
}


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

        s.setup_basic_metadata(gui, &files, &dirs);

        if files.len() == 1 && dirs.is_empty() {
            s.watch_for_media(&files[0]);
        } else {
            imp.notebook.remove_page(imp.media_page.position().try_into().ok());
        }

        if files.len() + dirs.len() > 1 {
            imp.notebook.remove_page(imp.permissions_page.position().try_into().ok());
        } else {
            let entry = files.get(0).unwrap_or_else(|| &dirs[0]);
            s.setup_permissions(entry);
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

    pub(super) fn setup_basic_metadata(
        &self,
        gui: &Gui,
        files: &[EntryObject],
        dirs: &[EntryObject],
    ) {
        let imp = self.imp();

        let mut size = 0;
        let mut allocated = 0;

        for f in files {
            size += f.get().raw_size();
            allocated += f.get().allocated_size;
        }

        for d in dirs {
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
            self.set_title(Some(&format!(
                "Properties - {}",
                files[0].get().name.to_string_lossy()
            )));
            imp.name_text.set_text(&files[0].get().name.to_string_lossy());
            imp.mtime_text.set_text(&files[0].get().mtime.seconds_string());

            self.set_image(gui, &files[0]);

            if let Some(link) = &files[0].get().symlink {
                imp.link_badge.set_visible(true);
                imp.media_link_badge.set_visible(true);
                imp.link_box.set_visible(true);
                imp.link_text.set_text(&link.to_string_lossy());

                if files[0].get().mime != "inode/symlink" {
                    imp.type_text.set_text(&format!("{} (symlink)", files[0].get().mime));
                } else {
                    imp.type_text.set_text(files[0].get().mime);
                }
            } else {
                imp.type_text.set_text(files[0].get().mime);
            }
        } else if files.is_empty() && dirs.len() == 1 {
            self.set_title(Some(&format!("Properties - {}", dirs[0].get().name.to_string_lossy())));
            imp.name_text.set_text(&dirs[0].get().name.to_string_lossy());
            imp.mtime_text.set_text(&dirs[0].get().mtime.seconds_string());

            self.set_image(gui, &dirs[0]);

            if let Some(link) = &dirs[0].get().symlink {
                imp.link_badge.set_visible(true);
                imp.type_text.set_text(&format!("{} (symlink)", dirs[0].get().mime));
                imp.link_box.set_visible(true);
                imp.link_text.set_text(&link.to_string_lossy());
            } else {
                imp.type_text.set_text(dirs[0].get().mime);
            }
        } else if dirs.is_empty() {
            imp.mtime_box.set_visible(false);

            imp.name_label.set_text("");
            imp.name_text.set_text(&format!("{} files", files.len()));

            let mimetype = files[0].get().mime;
            if files.iter().any(|f| f.get().mime != mimetype) {
                imp.type_box.set_visible(false);
                self.default_image();
            } else {
                imp.type_text.set_text(mimetype);
                self.set_image(gui, &files[0]);
            }
        } else if files.is_empty() {
            imp.mtime_box.set_visible(false);

            imp.name_label.set_text("");
            imp.name_text.set_text(&format!("{} directories", dirs.len()));

            imp.type_text.set_text(dirs[0].get().mime);
            self.set_image(gui, &dirs[0]);
        } else {
            imp.type_box.set_visible(false);
            imp.mtime_box.set_visible(false);

            imp.name_label.set_text("");
            imp.name_text.set_text(&format!(
                "{} directories and {} files",
                dirs.len(),
                files.len()
            ));

            self.default_image();
        }
    }

    fn set_image(&self, g: &Gui, eo: &EntryObject) {
        if let Some(tex) = eo.imp().thumbnail() {
            self.imp().media_icon.set_from_paintable(Some(&tex));
            self.imp().icon.set_from_paintable(Some(&tex));
            return;
        }

        if eo.imp().can_sync_thumbnail() {
            let e = eo.get();
            let tex = g.thumbnailer.sync_thumbnail(&e.abs_path, e.mime, e.mtime);

            if let Some(tex) = tex {
                self.imp().media_icon.set_from_paintable(Some(&tex));
                self.imp().icon.set_from_paintable(Some(&tex));
                return;
            }
        }

        let icon = &Icon::deserialize(&eo.get().icon).unwrap();
        self.imp().media_icon.set_from_gicon(icon);
        self.imp().icon.set_from_gicon(icon);
    }

    fn default_image(&self) {
        // Media page won't be visible if this is called
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

    fn setup_permissions(&self, eo: &EntryObject) {
        let imp = self.imp();

        let metadata = match eo.get().abs_path.metadata() {
            Ok(m) => m,
            Err(e) => {
                error!("{e}");
                return show_warning(format!("Failed to load file metadata: {e}"));
            }
        };

        let owner = get_user_by_uid(metadata.uid());
        let owner_name = owner
            .as_ref()
            .map_or(Cow::Borrowed("unknown user"), |u| u.name().to_string_lossy());

        imp.perm_owner.set_text(&format!("User ({owner_name})"));

        let group = get_group_by_gid(metadata.gid());
        let group_name = group
            .as_ref()
            .map_or(Cow::Borrowed("unknown group"), |g| g.name().to_string_lossy());

        imp.perm_group.set_text(&format!("Group ({group_name})"));

        // This will clobber updates if the user edits permissions with some other method while the
        // dialog is open. This is fine for my personal use.
        let path = &eo.get().abs_path;
        let mode = Rc::new(Cell::new(metadata.permissions().mode()));

        Self::mode_checkbox(&imp.u_r, path, 0o400, &mode);
        Self::mode_checkbox(&imp.u_w, path, 0o200, &mode);
        Self::mode_checkbox(&imp.u_x, path, 0o100, &mode);

        Self::mode_checkbox(&imp.g_r, path, 0o040, &mode);
        Self::mode_checkbox(&imp.g_w, path, 0o020, &mode);
        Self::mode_checkbox(&imp.g_x, path, 0o010, &mode);

        Self::mode_checkbox(&imp.a_r, path, 0o004, &mode);
        Self::mode_checkbox(&imp.a_w, path, 0o002, &mode);
        Self::mode_checkbox(&imp.a_x, path, 0o001, &mode);
    }

    fn mode_checkbox(check: &gtk::CheckButton, path: &Arc<Path>, mask: u32, mode: &Rc<Cell<u32>>) {
        let path = path.clone();
        let mode = mode.clone();

        if mode.get() & mask != 0 {
            check.set_active(true);
        }

        check.connect_toggled(move |b| {
            let old_m = mode.get();
            mode.set(if b.is_active() { old_m | mask } else { old_m & !mask });

            info!("Changing permissions for {path:?} from {old_m:o} to {:o}", mode.get());

            if let Err(e) = std::fs::set_permissions(&path, Permissions::from_mode(mode.get())) {
                show_error(format!("Error setting permissions: {e}"));
            }
        });
    }

    fn watch_for_media(&self, eo: &EntryObject) {
        let mime = eo.get().mime;

        if !mime.contains("image") && !mime.contains("video") && !mime.contains("audio") {
            return self
                .imp()
                .notebook
                .remove_page(self.imp().media_page.position().try_into().ok());
        }

        self.imp().media_spinner.start();

        let path = eo.get().abs_path.clone();

        // This can be expensive, so only do it when the media tab is first viewed.
        // In exchange, it's fine to just block the main thread.
        let weak = self.downgrade();
        self.imp().notebook.connect_switch_page(move |_notebook, _page, index| {
            let Some(s) = weak.upgrade() else { return };

            if s.imp().media_initialized.get() {
                return;
            }
            s.imp().media_initialized.set(true);

            if s.imp().media_page.position() != index as i32 {
                return;
            }

            if mime.contains("image") {
                s.show_image_data(&path, mime);
            }
        });
    }

    fn show_image_data(&self, path: &Arc<Path>, mime: &'static str) {
        let path = path.clone();
        let fut = gtk::gio::spawn_blocking(move || match Self::read_image_data(&path, mime) {
            Ok(r) => Ok(r),
            Err(e) => Err(format!("Failed to read file data: {e}")),
        });

        let weak = self.downgrade();
        gtk::glib::MainContext::default().spawn_local(async move {
            let r = fut.await;

            let Some(s) = weak.upgrade() else { return };

            s.imp().media_spinner.stop();
            s.imp().media_details.set_visible(true);

            let ((x, y), dur) = match r {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return show_warning(e),
                Err(e) => return show_warning(format!("Failed to read file data: {e:?}")),
            };

            s.imp().resolution_text.set_text(&format!("{x}x{y}"));

            match dur {
                Some(d) => s.imp().duration_text.set_text(&format!("{d:?}")),
                None => s.imp().duration_box.set_visible(false),
            }
        });
    }

    #[allow(clippy::type_complexity)]
    fn read_image_data(
        path: &Path,
        mime: &str,
    ) -> Result<((u32, u32), Option<Duration>), Box<dyn Error>> {
        if mime == "image/gif" || mime == "image/png" || mime == "image/webp" {
            let contents = File::open(path)?;

            // These will create empty iterators if the image isn't animated, which is fine.
            let (dimensions, frames) = if mime == "image/gif" {
                let dec = GifDecoder::new(contents)?;
                let dimensions = dec.dimensions();

                (dimensions, dec.into_frames())
            } else if mime == "image/png" {
                let dec = PngDecoder::new(contents)?;
                let dimensions = dec.dimensions();

                (dimensions, dec.apng().into_frames())
            } else if mime == "image/webp" {
                let dec = WebPDecoder::new(contents)?;
                let dimensions = dec.dimensions();

                (dimensions, dec.into_frames())
            } else {
                unreachable!()
            };

            let mut valid = true;
            let mut count = 0;

            let dur: Duration = frames
                .take_while(|r| {
                    if let Err(e) = r {
                        error!("Failed to read frames of animated image: {e}");
                        valid = false;
                        return false;
                    }
                    count += 1;
                    true
                })
                .map(|f| {
                    let d: Duration = f.unwrap().delay().into();
                    if d.is_zero() {
                        return Duration::from_millis(100);
                    }
                    d
                })
                .sum();

            if count <= 1 || !valid {
                Ok((dimensions, None))
            } else {
                Ok((dimensions, Some(dur)))
            }
        } else {
            Ok((image::image_dimensions(path)?, None))
        }
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
        pub notebook: TemplateChild<gtk::Notebook>,

        #[template_child]
        pub icon: TemplateChild<gtk::Image>,
        #[template_child]
        pub link_badge: TemplateChild<gtk::Image>,

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
        pub link_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub link_text: TemplateChild<gtk::Label>,

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

        // Permissions page
        #[template_child]
        pub permissions_page: TemplateChild<gtk::NotebookPage>,

        #[template_child]
        pub perm_owner: TemplateChild<gtk::Label>,
        #[template_child]
        pub perm_group: TemplateChild<gtk::Label>,

        #[template_child]
        pub u_r: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub u_w: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub u_x: TemplateChild<gtk::CheckButton>,

        #[template_child]
        pub g_r: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub g_w: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub g_x: TemplateChild<gtk::CheckButton>,

        #[template_child]
        pub a_r: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub a_w: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub a_x: TemplateChild<gtk::CheckButton>,

        // Image/Video/Music page
        #[template_child]
        pub media_page: TemplateChild<gtk::NotebookPage>,
        #[template_child]
        pub media_label: TemplateChild<gtk::Label>,

        #[template_child]
        pub media_icon: TemplateChild<gtk::Image>,
        #[template_child]
        pub media_link_badge: TemplateChild<gtk::Image>,

        #[template_child]
        pub media_details: TemplateChild<gtk::Box>,
        #[template_child]
        pub media_spinner: TemplateChild<gtk::Spinner>,

        #[template_child]
        pub resolution_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub resolution_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub duration_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub duration_text: TemplateChild<gtk::Label>,

        // Format
        // Container
        //
        // Images: Resolution, ??colorspace??, ??mode??, animation duration/frame count
        //
        // Video: Resolution, duration, codecs, ?bitrates?
        // Duration
        // Codec?
        // Colorspace?
        // Artist, Album, Album Artist, ??year??, bitrate
        #[template_child]
        pub close: TemplateChild<gtk::Button>,

        pub cancel: OnceCell<Arc<AtomicBool>>,
        pub size: Cell<u64>,
        pub allocated: Cell<u64>,

        pub child_files: Cell<usize>,
        pub child_dirs: Cell<usize>,

        pub media_initialized: Cell<bool>,
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
}
