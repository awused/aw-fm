use std::rc::Rc;

use ahash::AHashSet;
use gtk::gdk::Display;
use gtk::gio::{AppInfo, ListStore};
use gtk::prelude::{AppInfoExt, ListModelExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{CheckButtonExt, WidgetExt};
use gtk::{gio, glib, SingleSelection};

use super::cached_lookup;
use crate::gui::tabs::id::TabId;
use crate::gui::{Gui, Selected};

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

impl OpenWith {
    pub(super) fn new(
        gui: &Rc<Gui>,
        tab: TabId,
        display: &Display,
        selected: Selected<'_>,
    ) -> Option<Self> {
        if selected.len() == 0 {
            warn!("OpenWith called with no selection");
            return None;
        }

        let s: Self = glib::Object::new();

        let entries: Vec<_> = selected.collect();
        let mut mimetypes = Vec::new();

        for eo in &entries {
            let s = &eo.get().mime;
            if !mimetypes.contains(s) {
                mimetypes.push(s.clone());
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

        s.imp().top_text.set_text(&top_text);

        if mimetypes.len() == 1 && !entries[0].get().dir() {
            s.imp().set_default.set_label(Some(&format!("Always use for {}", mimetypes[0])));
        } else {
            s.imp().set_default.set_visible(false);
        };

        // TODO [gtk4.12] section headers
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
        factory.connect_setup(|_factory, item| {});


        s.imp().mimetypes.set(mimetypes).unwrap();
        s.imp().files.set(entries).unwrap();

        Some(s)
    }
}

mod imp {
    use std::cell::OnceCell;

    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    use crate::com::EntryObject;

    #[derive(Default, CompositeTemplate)]
    #[template(file = "open_with.ui")]
    pub struct OpenWith {
        #[template_child]
        pub top_text: TemplateChild<gtk::Label>,

        #[template_child]
        pub scroller: TemplateChild<gtk::ScrolledWindow>,

        #[template_child]
        pub set_default: TemplateChild<gtk::CheckButton>,

        pub mimetypes: OnceCell<Vec<String>>,
        pub files: OnceCell<Vec<EntryObject>>,
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

fn partition_app_infos(mimetypes: &[String]) -> PartitionedAppInfos {
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
