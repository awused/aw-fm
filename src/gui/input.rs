use std::cell::Ref;
use std::collections::hash_map;
use std::ffi::OsString;
use std::rc::Rc;
use std::str::FromStr;

use ahash::AHashMap;
use gtk::gdk::{Key, ModifierType};
use gtk::glib::BoxedAnyObject;
use gtk::prelude::{Cast, CastNone, StaticType};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{EventControllerExt, GestureSingleExt, GtkWindowExt, WidgetExt};

use super::Gui;
use crate::closing;
use crate::com::{DisplayMode, ManagerAction, Toggle};
use crate::config::{Shortcut, CONFIG};

#[derive(Debug, Default)]
pub(super) struct OpenDialogs {
    help: Option<gtk::Window>,
}

impl Gui {
    pub(super) fn setup_interaction(self: &Rc<Self>) {
        let dismiss_toast = gtk::GestureClick::new();

        dismiss_toast.connect_pressed(|gc, _n, _x, _y| {
            gc.widget().set_visible(false);
        });

        self.window.imp().toast.add_controller(dismiss_toast);

        let key = gtk::EventControllerKey::new();

        let g = self.clone();
        key.connect_key_pressed(move |_e, a, _b, c| {
            if let Some(s) = g.shortcut_from_key(a, c) {
                g.run_command(s);
            }
            gtk::Inhibit(false)
        });

        self.window.add_controller(key);

        let click = gtk::GestureClick::new();

        let g = self.clone();
        click.connect_released(move |_c, n, _x, _y| {
            if n == 2 {
                if g.window.is_maximized() {
                    g.window.unmaximize()
                } else {
                    g.window.maximize()
                }
            }
        });

        self.window.add_controller(click);

        // Maps forward/back on a mouse to Forward/Backward
        let forward_back_mouse = gtk::GestureClick::new();

        forward_back_mouse.set_button(0);
        forward_back_mouse.connect_pressed(|c, n, _x, _y| match c.current_button() {
            8 => error!("TODO backwards for mouse pane"),
            9 => error!("TODO forwards for mouse pane"),
            _ => {}
        });

        self.window.add_controller(forward_back_mouse);

        //     let drop_target = gtk::DropTarget::new(FileList::static_type(), DragAction::COPY);
        //
        //     let g = self.clone();
        //     drop_target.connect_drop(move |_dt, v, _x, _y| {
        //         let files = match v.get::<FileList>() {
        //             Ok(files) => files.files(),
        //             Err(e) => {
        //                 error!("Error reading files from drop event: {e}");
        //                 return true;
        //             }
        //         };
        //         let paths: Vec<_> = files.into_iter().filter_map(|f| f.path()).collect();
        //
        //         g.send_manager((ManagerAction::Open(paths), ScrollMotionTarget::Start.into(),
        // None));
        //
        //         true
        //     });
        //
        //     self.window.add_controller(drop_target);
    }

    fn close_on_quit<T: WidgetExt>(self: &Rc<Self>, w: &T) {
        let key = gtk::EventControllerKey::new();
        let g = self.clone();
        key.connect_key_pressed(move |e, a, _b, c| {
            match g.shortcut_from_key(a, c) {
                Some(s) if s == "Quit" => {
                    e.widget()
                        .downcast::<gtk::Window>()
                        .expect("Dialog was somehow not a window")
                        .close();
                }
                _ => (),
            }
            gtk::Inhibit(false)
        });

        w.add_controller(key);
    }

    fn help_dialog(self: &Rc<Self>) {
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

        // It's enough, for now, to just set this at dialog spawn time.
        #[cfg(windows)]
        dialog.add_css_class(self.win32.dpi_class());

        let store = gtk::gio::ListStore::new(BoxedAnyObject::static_type());

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
            gtk::Inhibit(false)
        });

        let g = self.clone();
        dialog.connect_destroy(move |_| {
            // Nested hacks to avoid dropping two scroll events in a row.
            g.drop_next_scroll.set(false);
        });

        dialog.set_visible(true);

        self.open_dialogs.borrow_mut().help = Some(dialog);
    }

    fn shortcut_from_key<'a>(self: &'a Rc<Self>, k: Key, mods: ModifierType) -> Option<&'a String> {
        let mods = mods & !ModifierType::LOCK_MASK;
        let upper = k.to_upper();

        self.shortcuts.get(&mods)?.get(&upper)
    }

    pub(super) fn parse_shortcuts() -> AHashMap<ModifierType, AHashMap<Key, String>> {
        let mut shortcuts = AHashMap::new();

        for s in &CONFIG.shortcuts {
            let mut modifiers: ModifierType = ModifierType::from_bits(0).unwrap();
            if let Some(m) = &s.modifiers {
                let m = m.to_lowercase();
                if m.contains("control") {
                    modifiers |= ModifierType::CONTROL_MASK;
                }
                if m.contains("alt") {
                    modifiers |= ModifierType::ALT_MASK;
                }
                if m.contains("shift") {
                    modifiers |= ModifierType::SHIFT_MASK;
                }
                if m.contains("super") {
                    modifiers |= ModifierType::SUPER_MASK;
                }
                if m.contains("command") {
                    modifiers |= ModifierType::META_MASK;
                }
            };

            let inner = match shortcuts.entry(modifiers) {
                hash_map::Entry::Occupied(inner) => inner.into_mut(),
                hash_map::Entry::Vacant(vacant) => vacant.insert(AHashMap::new()),
            };

            let k = Key::from_name(&s.key)
                .unwrap_or_else(|| panic!("{}", format!("Could not decode Key: {}", &s.key)));
            inner.insert(k, s.action.clone());
        }
        shortcuts
    }

    pub(super) fn run_command(self: &Rc<Self>, cmd: &str) {
        let cmd = cmd.trim();

        debug!("Running command {}", cmd);

        // if self.simple_action(cmd) {
        //     return;
        // }

        if let Some((cmd, arg)) = cmd.split_once(' ') {
            let arg = arg.trim_start();

            let _ = match cmd {
                "Mode" => match DisplayMode::from_str(arg) {
                    Ok(m) => return self.tabs.borrow_mut().active_display_mode(m),
                    Err(e) => true,
                },
                "Execute" => {
                    return self
                        .send_manager(ManagerAction::Execute(arg.to_string(), self.get_env()));
                }
                "Script" => {
                    return self
                        .send_manager(ManagerAction::Script(arg.to_string(), self.get_env()));
                }

                _ => true,
            };

            // For now only toggles work here. Some of the regexes could be eliminated instead.
            if let Ok(arg) = Toggle::try_from(arg) {
                let _ = match cmd {
                    _ => true,
                };
            }
        }

        let _ = match cmd {
            "Quit" => {
                closing::close();
                return self.window.close();
            }
            "Help" => return self.help_dialog(),

            "Parent" => return self.tabs.borrow_mut().active_parent(),
            //"Child" => return self.tabs.borrow_mut().active_child(),
            _ => true,
        };

        let e = format!("Unrecognized command {cmd:?}");
        warn!("{e}");
        self.convey_error(&e);
    }

    fn get_env(&self) -> Vec<(String, OsString)> {
        vec![todo!()]
    }
}
