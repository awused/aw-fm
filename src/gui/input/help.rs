use std::cell::Ref;
use std::rc::Rc;

use gtk::gdk::Key;
use gtk::glib::prelude::*;
use gtk::glib::{BoxedAnyObject, Propagation};
use gtk::prelude::*;

use crate::config::{Shortcut, CONFIG};
use crate::gui::Gui;

impl Gui {
    pub(super) fn help_dialog(self: &Rc<Self>) {
        if let Some(d) = &self.open_dialogs.borrow().help {
            info!("Help dialog already open");
            d.present();
            return;
        }

        let dialog = gtk::Window::builder().title("Help").transient_for(&self.window).build();

        self.close_on_quit(&dialog);

        // default size might want to scale based on dpi
        dialog.set_default_width(800);
        dialog.set_default_height(600);

        let store = gtk::gio::ListStore::new::<BoxedAnyObject>();

        for s in &CONFIG.shortcuts {
            store.append(&BoxedAnyObject::new(s));
        }

        let modifier_factory = gtk::SignalListItemFactory::new();
        modifier_factory.connect_setup(move |_fact, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::new(None);
            label.set_halign(gtk::Align::Start);
            item.set_child(Some(&label))
        });
        modifier_factory.connect_bind(move |_fact, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let child = item.child().and_downcast::<gtk::Label>().unwrap();
            let entry = item.item().and_downcast::<BoxedAnyObject>().unwrap();
            let s: Ref<&Shortcut> = entry.borrow();
            child.set_text(s.modifiers.as_deref().unwrap_or(""));
        });

        let key_factory = gtk::SignalListItemFactory::new();
        key_factory.connect_setup(move |_fact, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::new(None);
            label.set_halign(gtk::Align::Start);
            item.set_child(Some(&label))
        });
        key_factory.connect_bind(move |_fact, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let child = item.child().and_downcast::<gtk::Label>().unwrap();
            let entry = item.item().and_downcast::<BoxedAnyObject>().unwrap();
            let s: Ref<&Shortcut> = entry.borrow();

            match Key::from_name(&s.key).unwrap().to_unicode() {
                // This should avoid most unprintable/weird characters but still translate
                // question, bracketright, etc into characters.
                Some(c) if !c.is_whitespace() && !c.is_control() => {
                    child.set_text(&c.to_string());
                }
                _ => {
                    child.set_text(&s.key);
                }
            }
        });

        let action_factory = gtk::SignalListItemFactory::new();
        action_factory.connect_setup(move |_fact, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::new(None);
            label.set_halign(gtk::Align::Start);
            item.set_child(Some(&label))
        });
        action_factory.connect_bind(move |_fact, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let child = item.child().and_downcast::<gtk::Label>().unwrap();
            let entry = item.item().and_downcast::<BoxedAnyObject>().unwrap();
            let s: Ref<&Shortcut> = entry.borrow();
            child.set_text(&s.action);
        });

        let modifier_column = gtk::ColumnViewColumn::new(Some("Modifiers"), Some(modifier_factory));
        let key_column = gtk::ColumnViewColumn::new(Some("Key"), Some(key_factory));
        let action_column = gtk::ColumnViewColumn::new(Some("Action"), Some(action_factory));
        action_column.set_expand(true);

        let view = gtk::ColumnView::new(Some(gtk::NoSelection::new(Some(store))));
        view.append_column(&modifier_column);
        view.append_column(&key_column);
        view.append_column(&action_column);

        let scrolled =
            gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).build();

        scrolled.set_child(Some(&view));

        dialog.set_child(Some(&scrolled));

        let g = self.clone();
        dialog.connect_close_request(move |d| {
            g.open_dialogs.borrow_mut().help.take();
            d.destroy();
            Propagation::Proceed
        });


        dialog.set_visible(true);

        self.open_dialogs.borrow_mut().help = Some(dialog);
    }
}
