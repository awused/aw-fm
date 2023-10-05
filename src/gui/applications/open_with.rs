use std::path::Path;
use std::rc::Rc;

use ahash::AHashSet;
use gtk::gdk::Display;
use gtk::gio::{AppInfo, AppInfoCreateFlags, File, ListStore};
use gtk::glib::{shell_parse_argv, shell_unquote};
use gtk::prelude::{
    AppInfoExt, Cast, CastNone, DisplayExt, EditableExt, GdkAppLaunchContextExt, ListModelExt,
    ObjectExt,
};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{ButtonExt, CheckButtonExt, GtkWindowExt, ListItemExt, WidgetExt};
use gtk::{glib, SingleSelection};

use super::application::Application;
use super::cached_lookup;
use crate::com::EntryObject;
use crate::gui::applications::DEFAULT_CACHE;
use crate::gui::{show_error, show_warning, Gui, Selected};

glib::wrapper! {
    pub struct OpenWith(ObjectSubclass<imp::OpenWith>)
        @extends gtk::Widget, gtk::Window;
}

struct PartitionedAppInfos {
    defaults: Vec<AppInfo>,
    recommended: Vec<AppInfo>,
    normal: Vec<AppInfo>,
    hidden: Vec<AppInfo>,
}

// TODO -- create from CLI command

impl OpenWith {
    pub(super) fn open(gui: &Rc<Gui>, selected: Selected<'_>) {
        if selected.len() == 0 {
            return warn!("OpenWith called with no selection");
        }

        let s: Self = glib::Object::new();
        let imp = s.imp();

        gui.close_on_quit_or_esc(&s);

        let entries: Vec<_> = selected.collect();
        let mut mimetypes = Vec::new();

        for eo in &entries {
            let s = &eo.get().mime;
            if !mimetypes.contains(s) {
                mimetypes.push(s);
            }
        }

        let top_text = if entries.len() == 1 {
            format!("Choose an application for {}", entries[0].get().name.to_string_lossy())
        } else if mimetypes.len() == 1 && !entries[0].get().dir() {
            format!("Choose an application for these {} {} files", entries.len(), mimetypes[0])
        } else if mimetypes.len() == 1 {
            format!("Choose an application for these {} directories", entries.len())
        } else {
            format!("Choose an application for the {} selected files", entries.len())
        };

        imp.top_text.set_text(&top_text);

        if mimetypes.len() == 1 && !entries[0].get().dir() {
            imp.set_default.set_label(Some(&format!("Always use for {}", mimetypes[0])));
        } else {
            imp.set_default.set_visible(false);
        };

        // TODO [gtk4.12] section headers
        // let flatten_list = gtk::FlattenListModel::new(model);
        let apps = partition_app_infos(&mimetypes);

        let list = ListStore::new::<AppInfo>();
        for a in apps.defaults {
            list.append(&a);
        }
        for a in apps.recommended {
            list.append(&a);
        }
        for a in apps.normal {
            list.append(&a);
        }
        for a in apps.hidden {
            list.append(&a);
        }

        let selection = SingleSelection::new(Some(list));
        selection.set_autoselect(true);

        if selection.n_items() != 0 {
            selection.set_selected(0);
        }

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = Application::default();

            item.set_activatable(false);
            item.set_child(Some(&row));
        });

        factory.connect_bind(|_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let info = item.item().and_downcast::<AppInfo>().unwrap();

            let child = item.child().and_downcast::<Application>().unwrap();
            child.set_info(&info);
        });

        imp.list.set_model(Some(&selection));
        imp.list.set_factory(Some(&factory));

        let w = s.downgrade();
        imp.cancel.connect_clicked(move |_b| {
            w.upgrade().unwrap().close();
        });

        let w = s.downgrade();
        imp.create.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();

            s.create_application(&s.imp().command_line.get().text());
        });

        let w = s.downgrade();
        let display = gui.window.display();
        imp.open.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();

            s.open_application(&display, &mimetypes, &entries);
            s.close();
        });

        s.set_transient_for(Some(&gui.window));
        s.set_visible(true);
    }

    fn open_application(
        &self,
        display: &Display,
        mimetypes: &[&'static str],
        files: &[EntryObject],
    ) {
        let imp = self.imp();

        let model = imp.list.model().and_downcast::<SingleSelection>().unwrap();
        let Some(app) = model.selected_item().and_downcast::<AppInfo>() else {
            return show_warning("No selected application");
        };

        debug!("Opening {} files with {:?}", files.len(), app.id());

        if imp.set_default.is_active() {
            if mimetypes.len() != 1 {
                return show_error(format!(
                    "Cannot set default application for {} mimetypes, this should never happen",
                    mimetypes.len()
                ));
            }

            info!("Setting default application for {} to {:?}", mimetypes[0], app.id());

            if let Err(e) = app.set_as_default_for_type(mimetypes[0]) {
                show_error(format!("Error setting default application for {}: {e}", mimetypes[0]));
            }

            DEFAULT_CACHE.with_borrow_mut(|c| c.remove(&mimetypes[0]));
        }

        let context = display.app_launch_context();
        context.set_timestamp(gtk::gdk::CURRENT_TIME);

        let files: Vec<_> = files.iter().map(|f| File::for_path(&f.get().abs_path)).collect();
        if let Err(e) = app.launch(&files, Some(&context)) {
            show_error(format!("Application launch error: {app:?} {e:?}"));
        }
    }

    fn create_application(&self, command_line: &str) {
        let args = match shell_parse_argv(command_line) {
            Ok(a) => a,
            Err(e) => return show_warning(format!("Couldn't parse command line: {e}")),
        };

        if args.is_empty() {
            return;
        }

        let app_name = match shell_unquote(&args[0]) {
            Ok(unquoted) => Path::new(&unquoted)
                .file_name()
                .unwrap_or(&unquoted)
                .to_string_lossy()
                .to_string(),
            Err(_) => args[0].to_string_lossy().to_string(),
        };

        match AppInfo::create_from_commandline(
            command_line,
            Some(&app_name),
            AppInfoCreateFlags::NONE,
        ) {
            Ok(app) => {
                let model = self.imp().list.model().and_downcast::<SingleSelection>().unwrap();
                let list = model.model().and_downcast::<ListStore>().unwrap();
                list.insert(0, &app);

                model.set_selected(0);
                // TODO [gtk4.12] scroll up
            }
            Err(e) => show_error(format!("Couldn't create application: {e}")),
        }
    }
}

mod imp {
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    #[derive(Default, CompositeTemplate)]
    #[template(file = "open_with.ui")]
    pub struct OpenWith {
        #[template_child]
        pub top_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub list: TemplateChild<gtk::ListView>,

        #[template_child]
        pub command_line: TemplateChild<gtk::Entry>,

        #[template_child]
        pub create: TemplateChild<gtk::Button>,

        #[template_child]
        pub set_default: TemplateChild<gtk::CheckButton>,

        #[template_child]
        pub cancel: TemplateChild<gtk::Button>,

        #[template_child]
        pub open: TemplateChild<gtk::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OpenWith {
        type ParentType = gtk::Window;
        type Type = super::OpenWith;

        const NAME: &'static str = "AwFmOpenWith";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for OpenWith {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WindowImpl for OpenWith {}
    impl WidgetImpl for OpenWith {}

    impl OpenWith {}
}

fn partition_app_infos(mimetypes: &[&'static str]) -> PartitionedAppInfos {
    // App IDs are assumed to be unique,
    let mut app_ids = AHashSet::new();
    let mut defaults = Vec::new();

    for mime in mimetypes {
        if let Some(app) = cached_lookup(mime) {
            if let Some(id) = app.id() {
                if app_ids.contains(&id) {
                    continue;
                }
                app_ids.insert(id);
            } else if defaults.iter().any(|d| app.equal(d)) {
                // This should almost never actually be hit
                continue;
            }

            defaults.push(app);
        }
    }

    let mut recommended = Vec::new();
    for mime in mimetypes {
        let applications = AppInfo::recommended_for_type(mime);

        for app in applications {
            if let Some(id) = app.id() {
                if app_ids.contains(&id) {
                    continue;
                }

                app_ids.insert(id);
            } else if defaults.iter().any(|d| app.equal(d))
                || recommended.iter().any(|r| app.equal(r))
            {
                // This should almost never actually be hit
                continue;
            }

            recommended.push(app);
        }
    }

    let mut normal = Vec::new();
    let mut hidden = Vec::new();

    for app in AppInfo::all() {
        if let Some(id) = app.id() {
            if app_ids.contains(&id) {
                continue;
            }
        } else if defaults.iter().any(|d| app.equal(d)) || recommended.iter().any(|r| app.equal(r))
        {
            // This should almost never actually be hit
            continue;
        }

        if app.should_show() {
            normal.push(app);
        } else {
            hidden.push(app);
        }
    }

    defaults.sort();
    recommended.sort();
    normal.sort();
    hidden.sort();
    PartitionedAppInfos { defaults, recommended, normal, hidden }
}
