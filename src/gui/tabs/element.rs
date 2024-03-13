use std::path::Path;

use gtk::gdk::{ContentProvider, DragAction, ModifierType};
use gtk::glib::{BoxedAnyObject, GString};
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, DragSource, DropTarget, GestureClick, Orientation, WidgetPaintable};

use crate::gui::tabs::id::TabId;
use crate::gui::tabs_run;


glib::wrapper! {
    pub struct TabElement(ObjectSubclass<imp::AwFmTab>)
        @extends gtk::Widget, gtk::Box;
}

impl TabElement {
    pub(super) fn tab(&self) -> TabId {
        *self.imp().tab.get().unwrap()
    }

    pub(super) fn new(tab: TabId, path: &Path) -> Self {
        let s: Self = glib::Object::new();

        let imp = s.imp();

        imp.tab.set(tab).unwrap();
        s.flat_title(path);

        let mouse = GestureClick::new();
        mouse.set_button(0);
        mouse.connect_pressed(move |c, n, x, y| {
            // https://gitlab.gnome.org/GNOME/gtk/-/issues/5884
            let w = c.widget();
            if !w.contains(x, y) {
                warn!("Workaround -- ignoring junk mouse event on {tab:?} element",);
                return;
            }

            if n != 1 {
                return;
            }
            match c.current_button() {
                1 => {
                    let mods = c.current_event().unwrap().modifier_state();
                    let orient = if mods == ModifierType::SHIFT_MASK {
                        Orientation::Vertical
                    } else if mods == ModifierType::CONTROL_MASK {
                        Orientation::Horizontal
                    } else {
                        return;
                    };

                    c.set_state(gtk::EventSequenceState::Claimed);
                    tabs_run(|tl| tl.active_split(orient, Some(tab)));
                }
                2 => {
                    debug!("Closing {tab:?} from middle click");
                    tabs_run(|tl| tl.close_tab(tab));
                }
                _ => {}
            }
        });
        s.add_controller(mouse);

        // Drag and drop reordering. No fancy animations or drop target indicaticators yet.
        let drag_source = DragSource::new();
        drag_source.set_actions(DragAction::MOVE);
        drag_source.connect_prepare(move |_ds, _x, _y| {
            Some(ContentProvider::for_value(&BoxedAnyObject::new(tab).into()))
        });
        drag_source.connect_drag_begin(|ds, _drag| {
            let paintable = WidgetPaintable::new(Some(&ds.widget()));

            ds.set_icon(Some(&paintable), 0, 0);
        });
        s.add_controller(drag_source);

        let drop_target = DropTarget::new(BoxedAnyObject::static_type(), DragAction::MOVE);
        drop_target.connect_drop(move |dt, v, _x, y| {
            let source = *v.get::<BoxedAnyObject>().unwrap().borrow::<TabId>();
            if source == tab {
                debug!("Ignoring tab dropped onto itself");
                return false;
            }

            // 2.5-3.0 feels more like half the target than 2.0 does
            if y > dt.widget().height() as f64 / 2.5 {
                debug!("Reordering {source:?} after {tab:?}");
                tabs_run(|t| t.reorder(source, tab, true));
            } else {
                debug!("Reordering {source:?} before {tab:?}");
                tabs_run(|t| t.reorder(source, tab, false));
            }

            true
        });

        s.add_controller(drop_target);

        s
    }

    pub fn clone_from(&self, other: &Self) {
        let imp = self.imp();
        let other_imp = other.imp();

        imp.title.set_text(&other_imp.title.text());
        let tooltip = other_imp.title.tooltip_text();
        imp.title.set_tooltip_text(tooltip.as_ref().map(GString::as_str));

        imp.spinner.set_spinning(other_imp.spinner.is_spinning());
        imp.spinner.set_visible(other_imp.spinner.is_visible());
    }

    pub fn flat_title(&self, path: &Path) {
        let imp = self.imp();

        // let mut last_two_dirs = PathBuf::new();
        // match (path.parent().and_then(Path::file_name), path.file_name()) {
        //     (Some(parent), Some(dir)) => {
        //         let mut two_segments = PathBuf::from(parent);
        //         two_segments.push(dir);
        //         imp.title.set_text(&two_segments.to_string_lossy())
        //     }
        //     _ => {
        //         imp.title.set_text(&path.to_string_lossy());
        //     }
        // }

        imp.title
            .set_text(&path.file_name().unwrap_or(path.as_os_str()).to_string_lossy());
        imp.title.set_tooltip_text(Some(&path.to_string_lossy()));
    }

    pub fn search_title(&self, path: &Path) {
        let imp = self.imp();
        let title =
            format!("Search: {}", &path.file_name().unwrap_or(path.as_os_str()).to_string_lossy());
        let tooltip = format!("Searching in {}", &path.to_string_lossy());

        imp.title.set_text(&title);
        imp.title.set_tooltip_text(Some(&tooltip));
    }

    pub fn set_pane_visible(&self, visible: bool) {
        if visible {
            self.add_css_class("visible-tab");
        } else {
            self.remove_css_class("visible-tab");
        }
    }

    pub fn set_active(&self, active: bool) {
        if active {
            self.add_css_class("active-tab");
        } else {
            self.remove_css_class("active-tab");
        }
    }

    pub fn set_child(&self, active: bool) {
        if active {
            self.add_css_class("child-tab");
        } else {
            self.remove_css_class("child-tab");
        }
    }

    pub fn spin(&self) {
        self.imp().spinner.start();
        self.imp().spinner.set_visible(true);
    }

    pub fn stop_spin(&self) {
        self.imp().spinner.stop();
        self.imp().spinner.set_visible(false);
    }
}

mod imp {
    use std::cell::Cell;

    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate, ListItem};
    use once_cell::unsync::OnceCell;

    use crate::gui::tabs::id::TabId;

    #[derive(Default, CompositeTemplate)]
    #[template(file = "element.ui")]
    pub struct AwFmTab {
        #[template_child]
        pub title: TemplateChild<gtk::Label>,

        #[template_child]
        pub(super) spinner: TemplateChild<gtk::Spinner>,

        pub tab: OnceCell<TabId>,
        // TEMPORARY WORKAROUND for broken gtk binding
        pub list_item: Cell<Option<ListItem>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AwFmTab {
        type ParentType = gtk::Box;
        type Type = super::TabElement;

        const NAME: &'static str = "AwFmTab";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AwFmTab {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl BoxImpl for AwFmTab {}
    impl WidgetImpl for AwFmTab {}

    impl AwFmTab {}
}
