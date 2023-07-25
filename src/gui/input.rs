use std::collections::hash_map;
use std::ffi::OsString;
use std::path::Path;
use std::rc::Rc;
use std::str::FromStr;

use ahash::AHashMap;
use dirs::home_dir;
use gtk::gdk::{Key, ModifierType};
use gtk::glib::ControlFlow;
use gtk::prelude::Cast;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{EventControllerExt, GtkWindowExt, WidgetExt};
use gtk::Orientation;

use super::Gui;
use crate::closing;
use crate::com::{DisplayMode, ManagerAction};
use crate::config::CONFIG;

mod help;

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

        // Attach this to the Window so it is always available.
        // Keyboard focus should rarely ever leave a tab.
        let key = gtk::EventControllerKey::new();

        let g = self.clone();
        key.connect_key_pressed(move |_e, a, _b, c| {
            if let Some(s) = g.shortcut_from_key(a, c) {
                g.run_command(s);
            }
            // Inhibit(false) -> ControlFlow::Break
            // https://github.com/gtk-rs/gtk4-rs/issues/1435
            ControlFlow::Break
        });

        self.window.add_controller(key);
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
            // https://github.com/gtk-rs/gtk4-rs/issues/1435
            ControlFlow::Break
        });

        w.add_controller(key);
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
        // Do not trim the end of cmd because files and directories can end in spaces
        let cmd = cmd.trim_start();

        debug!("Running command {}", cmd);

        // if self.simple_action(cmd) {
        //     return;
        // }

        if let Some((cmd, arg)) = cmd.split_once(' ') {
            let arg = arg.trim_start();

            let _ = match cmd {
                "Mode" => match DisplayMode::from_str(arg) {
                    Ok(m) => return self.tabs.borrow_mut().active_display_mode(m),
                    Err(_e) => true,
                },
                "Navigate" => return self.tabs.borrow_mut().active_navigate(Path::new(arg)),
                "JumpTo" => return self.tabs.borrow_mut().active_jump(Path::new(arg)),
                "NewTab" => return self.tabs.borrow_mut().open_tab(Path::new(arg), true),
                "NewBackgroundTab" => {
                    return self.tabs.borrow_mut().open_tab(Path::new(arg), false);
                }

                "Split" => match arg {
                    "horizontal" => {
                        return self.tabs.borrow_mut().active_split(Orientation::Horizontal);
                    }
                    "vertical" => {
                        return self.tabs.borrow_mut().active_split(Orientation::Vertical);
                    }
                    _ => true,
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
            // if let Ok(arg) = Toggle::try_from(arg) {
            //     let _ = match cmd {
            //         _ => true,
            //     };
            // }
        }

        let mut tabs = self.tabs.borrow_mut();

        let _ = match cmd {
            "Quit" => {
                closing::close();
                return self.window.close();
            }
            "Help" => return self.help_dialog(),
            "Home" => {
                return tabs.active_navigate(&home_dir().unwrap_or_default());
            }
            "NewTab" => return tabs.new_tab(true),
            "NewBackgroundTab" => return tabs.new_tab(false),

            "CloseTab" => return tabs.active_close_tab(),
            "ClosePane" => return tabs.active_close_pane(),
            "CloseActive" => return tabs.active_close_both(),

            "Forward" => return tabs.active_forward(),
            "Back" => return tabs.active_back(),
            "Parent" => return tabs.active_parent(),
            "Child" => return tabs.active_child(),
            _ => true,
        };

        let e = format!("Unrecognized command {cmd:?}");
        warn!("{e}");
        self.warning(&e);
    }

    fn get_env(&self) -> Vec<(String, OsString)> {
        // vec![]
        todo!()
    }
}
