use gtk::glib;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::prelude::{ButtonExt, EditableExt, EntryExt, WidgetExt};

use super::Chooser;
use crate::closing;
use crate::config::ChooserCommand;
use crate::gui::chooser::chooser_run;


glib::wrapper! {
    pub struct ChooserBar(ObjectSubclass<imp::ChooserBar>)
        @extends gtk::Widget, gtk::Box,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}


impl ChooserCommand {
    const fn default_accept(&self) -> &'static str {
        match self {
            Self::OpenFile { .. } => "Open",
            Self::SaveFile { .. } => "Save",
            Self::SaveFiles { .. } => "Save Here",
        }
    }
}

impl ChooserBar {
    pub(super) fn new(cmd: &ChooserCommand) -> Self {
        let s: Self = glib::Object::new();
        let imp = s.imp();
        let args = cmd.args();

        imp.cancel.connect_clicked(move |_| {
            println!("cancelled");
            closing::close();
        });

        imp.accept.connect_clicked(move |_| {
            chooser_run(Chooser::accept);
        });

        imp.accept
            .set_label(args.label.as_deref().unwrap_or_else(|| cmd.default_accept()));


        if matches!(cmd, ChooserCommand::SaveFiles { .. }) {
            imp.text_entry.set_visible(false);
        } else {
            imp.text_entry.connect_activate(|_| {
                chooser_run(Chooser::accept);
            });
            imp.text_entry.connect_text_notify(|text| {
                chooser_run(|c| c.text(text.text()));
            });
        }


        s
    }
}


mod imp {
    use std::cell::RefCell;
    use std::path::Path;
    use std::sync::Arc;

    use gtk::subclass::prelude::*;
    use gtk::{CompositeTemplate, glib};

    #[derive(Default, CompositeTemplate)]
    #[template(file = "bar.ui")]
    pub struct ChooserBar {
        #[template_child]
        pub accept: TemplateChild<gtk::Button>,

        #[template_child]
        pub cancel: TemplateChild<gtk::Button>,

        #[template_child]
        pub text_entry: TemplateChild<gtk::Entry>,

        pub files: RefCell<Vec<Arc<Path>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ChooserBar {
        type ParentType = gtk::Box;
        type Type = super::ChooserBar;

        const NAME: &'static str = "ChooserBar";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ChooserBar {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WidgetImpl for ChooserBar {}
    impl BoxImpl for ChooserBar {}
}
