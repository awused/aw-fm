use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use ahash::AHashSet;
use gtk::gdk::Display;
use gtk::gio::{AppInfo, AppInfoCreateFlags, File, ListStore};
use gtk::glib::{shell_parse_argv, shell_unquote};
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{CustomFilter, FilterChange, FilterListModel, Label, SingleSelection, glib};

use super::application::Application;
use super::cached_lookup;
use crate::gui::applications::DEFAULT_CACHE;
use crate::gui::{Gui, Selected, show_error, show_warning};

glib::wrapper! {
    pub struct OpenWith(ObjectSubclass<imp::OpenWith>)
        @extends gtk::Widget, gtk::Window,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::ShortcutManager, gtk::Native, gtk::Root;
}

struct PartitionedAppInfos {
    defaults: Vec<AppInfo>,
    recommended: Vec<AppInfo>,
    normal: Vec<AppInfo>,
    hidden: Vec<AppInfo>,
}

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

        s.setup_appinfo_list(&mimetypes);

        imp.files.set(Some(entries));
        imp.mimetypes.set(Some(mimetypes));


        let w = s.downgrade();
        imp.cancel.connect_clicked(move |_b| {
            w.upgrade().unwrap().close();
        });

        let w = s.downgrade();
        let display = WidgetExt::display(&gui.window);
        imp.create.connect_clicked(move |_b| {
            let s = w.upgrade().unwrap();

            if let Some(app) = Self::create_application(&s.imp().command_line.get().text()) {
                s.open_application(app, &display);
                s.close();
            }
        });

        let w = s.downgrade();
        let display = WidgetExt::display(&gui.window);
        let activate = move || {
            let s = w.upgrade().unwrap();


            let model = s.imp().list.model().and_downcast::<SingleSelection>().unwrap();
            let Some(app) = model.selected_item().and_downcast::<AppInfo>() else {
                return show_warning("No selected application");
            };

            s.open_application(app, &display);
            s.close();
        };

        let act = activate.clone();
        imp.list.connect_activate(move |_c, _a| act());
        imp.open.connect_clicked(move |_b| activate());

        s.set_transient_for(Some(&gui.window));
        s.set_visible(true);
    }

    fn setup_appinfo_list(&self, mimetypes: &[&'static str]) {
        let imp = self.imp();

        let apps = partition_app_infos(mimetypes);

        let lists = ListStore::new::<ListStore>();
        lists.append(&ListStore::from_iter(apps.defaults.iter().cloned()));
        lists.append(&ListStore::from_iter(apps.recommended.iter().cloned()));
        lists.append(&ListStore::from_iter(apps.normal.iter().cloned()));
        lists.append(&ListStore::from_iter(apps.hidden.iter().cloned()));

        let flatten_list = gtk::FlattenListModel::new(Some(lists));

        let filter_text = Rc::new(RefCell::new(String::new()));
        let filter_clone = filter_text.clone();
        let filter = CustomFilter::new(move |app| {
            let app = app.downcast_ref::<AppInfo>().unwrap();

            let text = filter_clone.borrow();
            if text.is_empty() {
                return true;
            }

            // Don't normalize here
            app.name().to_lowercase().contains(&*text)
        });

        let filt = filter.clone();
        imp.name_filter.connect_changed(move |e| {
            let new = e.text().to_lowercase();

            let mut filter_text = filter_text.borrow_mut();

            let change = if filter_text.contains(&new) {
                FilterChange::LessStrict
            } else if new.contains(&*filter_text) {
                FilterChange::MoreStrict
            } else {
                FilterChange::Different
            };

            *filter_text = new;
            drop(filter_text);

            filt.changed(change);
        });

        let filtered = FilterListModel::new(Some(flatten_list), Some(filter));

        let selection = SingleSelection::new(Some(filtered));
        selection.set_autoselect(true);

        if selection.n_items() != 0 {
            selection.set_selected(0);
        }

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = Application::default();

            item.set_child(Some(&row));
        });

        factory.connect_bind(|_factory, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let info = item.item().and_downcast::<AppInfo>().unwrap();

            let child = item.child().and_downcast::<Application>().unwrap();
            child.set_info(&info);
        });

        let header_factory = gtk::SignalListItemFactory::new();
        header_factory.connect_setup(|_factory, header| {
            let header = header.downcast_ref::<gtk::ListHeader>().unwrap();
            let row = Label::default();

            header.set_child(Some(&row));
        });

        header_factory.connect_bind(move |_factory, header| {
            let header = header.downcast_ref::<gtk::ListHeader>().unwrap();
            let info = header.item().and_downcast::<AppInfo>().unwrap();

            // The overall number of AppInfos should be low enough this isn't awful
            let text = if apps.defaults.contains(&info) {
                "Default"
            } else if apps.recommended.contains(&info) {
                "Recommended"
            } else if apps.hidden.contains(&info) {
                "Hidden"
            } else {
                "Normal"
            };

            let child = header.child().and_downcast::<Label>().unwrap();
            child.set_text(text);
        });

        imp.list.set_model(Some(&selection));
        imp.list.set_factory(Some(&factory));
        imp.list.set_header_factory(Some(&header_factory));
    }

    fn open_application(&self, app: AppInfo, display: &Display) {
        let imp = self.imp();
        let mimetypes = imp.mimetypes.take().unwrap();
        let files = imp.files.take().unwrap();

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

    fn create_application(command_line: &str) -> Option<AppInfo> {
        let args = match shell_parse_argv(command_line) {
            Ok(a) => a,
            Err(e) => {
                show_warning(format!("Couldn't parse command line: {e}"));
                return None;
            }
        };

        if args.is_empty() {
            return None;
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
            Ok(app) => Some(app),
            Err(e) => {
                show_error(format!("Couldn't create application: {e}"));
                None
            }
        }
    }
}

mod imp {
    use std::cell::Cell;

    use gtk::subclass::prelude::*;
    use gtk::{CompositeTemplate, glib};

    use crate::gui::EntryObject;

    #[derive(Default, CompositeTemplate)]
    #[template(file = "open_with.ui")]
    pub struct OpenWith {
        #[template_child]
        pub top_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub list: TemplateChild<gtk::ListView>,

        #[template_child]
        pub name_filter: TemplateChild<gtk::Entry>,

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

        pub files: Cell<Option<Vec<EntryObject>>>,
        pub mimetypes: Cell<Option<Vec<&'static str>>>,
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
                if !app_ids.insert(id) {
                    continue;
                }
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
