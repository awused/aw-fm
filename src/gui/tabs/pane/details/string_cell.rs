use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, CompositeTemplate};

use crate::com::{EntryObject, SignalHolder};

#[derive(Clone, Copy, Debug, Default)]
pub(super) enum EntryString {
    #[default]
    Unset,
    Name,
    Size,
    Modified,
}

glib::wrapper! {
    pub struct StringCell(ObjectSubclass<imp::StringCell>)
        @extends gtk::Widget, gtk::Fixed;
}

impl StringCell {
    pub(super) fn new(kind: EntryString) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().kind.set(kind);
        obj
    }

    pub fn align_end(&self, chars: u32) {
        self.imp().contents.set_xalign(1.0);
        self.imp().contents.set_min_chars(chars);
    }

    pub fn bind(&self, obj: &EntryObject) {
        let imp = self.imp();
        imp.update_contents(&obj.get());

        // Can never change.
        if matches!(imp.kind.get(), EntryString::Name) {
            debug_assert!(imp.update_connection.take().is_none());
            return;
        }

        // Don't need to be weak refs
        let self_ref = self.clone();
        let id = obj.connect_local("update", false, move |entry| {
            let obj: EntryObject = entry[0].get().unwrap();
            self_ref.imp().update_contents(&obj.get());
            None
        });

        let d = SignalHolder::new(obj, id);

        assert!(imp.update_connection.replace(Some(d)).is_none())
    }

    pub fn unbind(&self, obj: &EntryObject) {
        self.imp().update_connection.take();
    }
}


mod imp {
    use std::borrow::Cow;
    use std::cell::{Cell, RefCell};

    use chrono::{Local, TimeZone};
    use gtk::glib::SignalHandlerId;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    use super::EntryString;
    use crate::com::{Entry, EntryObject, SignalHolder};

    #[derive(Default, CompositeTemplate)]
    #[template(file = "string_cell.ui")]
    pub struct StringCell {
        #[template_child]
        pub contents: TemplateChild<gtk::Inscription>,
        pub(super) kind: Cell<EntryString>,

        pub update_connection: Cell<Option<SignalHolder<EntryObject>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StringCell {
        type ParentType = gtk::Widget;
        type Type = super::StringCell;

        const NAME: &'static str = "StringCell";

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for StringCell {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WidgetImpl for StringCell {}

    impl StringCell {
        pub(super) fn update_contents(&self, entry: &Entry) -> bool {
            let new_text = match self.kind.get() {
                EntryString::Unset => unreachable!(),
                EntryString::Name => entry.name.to_string_lossy(),
                EntryString::Size => Cow::Owned(entry.short_size_string()),
                EntryString::Modified => {
                    // Only use seconds for columns
                    let localtime = Local.timestamp_opt(entry.mtime.sec as i64, 0).unwrap();
                    let text = localtime.format("%Y-%m-%d %H:%M:%S");
                    Cow::Owned(format!("{text}"))
                }
            };

            if !matches!(self.contents.text(), Some(existing) if existing.as_str() == new_text) {
                self.contents.set_text(Some(&new_text));
                true
            } else {
                false
            }
        }
    }
}
