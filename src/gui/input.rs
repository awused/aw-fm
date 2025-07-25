use std::collections::{VecDeque, hash_map};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use ahash::AHashMap;
use dirs::home_dir;
use gtk::gdk::{Key, ModifierType};
use gtk::glib::{self, Propagation};
use gtk::pango::{EllipsizeMode, WrapMode};
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{Orientation, Widget, Window};

use super::properties::dialog::PropDialog;
use super::tabs::id::TabId;
use super::{ActionTarget, Gui, label_attributes};
use crate::closing;
use crate::com::{DisplayMode, EntryObject, ManagerAction, SortDir, SortMode};
use crate::config::CONFIG;
use crate::gui::operations::Kind;
use crate::gui::tabs::list::TabPosition;
use crate::gui::{gui_run, show_warning};

mod help;

#[derive(Debug, Default)]
pub(super) struct OpenDialogs {
    help: Option<gtk::Window>,
    pub properties: Vec<PropDialog>,
}

impl Gui {
    pub(super) fn setup_interaction(self: &Rc<Self>) {
        let dismiss_toast = gtk::GestureClick::new();

        let g = self.clone();
        dismiss_toast.connect_pressed(move |gc, _n, _x, _y| {
            gc.widget().unwrap().set_visible(false);
            if let Some(s) = g.warning_timeout.take() {
                s.remove()
            }
        });

        self.window.imp().toast.add_controller(dismiss_toast);

        // Attach this to the Window so it is always available.
        // Keyboard focus should rarely ever leave a tab.
        let key = gtk::EventControllerKey::new();

        let g = self.clone();
        key.connect_key_pressed(move |_e, a, _b, c| {
            if let Some(s) = g.shortcut_from_key(a, c) {
                g.run_command_active(s);
            }
            Propagation::Proceed
        });

        self.window.add_controller(key);

        self.setup_idle_trim();

        if let Some(unload) = CONFIG.unload_timeout {
            self.setup_idle_unload(Duration::from_secs(unload.get()));
        }

        self.setup_bookmarks();
    }

    fn setup_idle_unload(self: &Rc<Self>, idle: Duration) {
        let focus = gtk::EventControllerFocus::new();

        let g = self.clone();
        focus.connect_enter(move |_| {
            if let Some(timeout) = g.unload_timeout.take() {
                timeout.remove();
            }
        });

        let g = self.clone();
        focus.connect_leave(move |_| {
            let gui = g.clone();
            let timeout = glib::timeout_add_local_once(idle, move || {
                debug!("Performing idle unload");
                gui.unload_timeout.take();
                gui.tabs.borrow_mut().idle_unload();

                // Wait a bit for everything to be dropped. 5 seconds is way too much, but
                // it can't really hurt anything.
                glib::timeout_add_local_once(Duration::from_secs(5), || {
                    trace!("Explicitly trimming unused memory");
                    unsafe {
                        libc::malloc_trim(0);
                    }
                });
            });

            if let Some(old) = g.unload_timeout.replace(Some(timeout)) {
                old.remove();
            }
        });

        self.window.add_controller(focus);
    }

    fn setup_idle_trim(self: &Rc<Self>) {
        let focus = gtk::EventControllerFocus::new();

        let g = self.clone();
        focus.connect_enter(move |_| {
            if let Some(timeout) = g.trim_timeout.take() {
                timeout.remove();
            }
        });

        let g = self.clone();
        focus.connect_leave(move |_| {
            let gui = g.clone();
            let timeout = glib::timeout_add_local_once(Duration::from_secs(600), move || {
                debug!("Performing idle trim");
                gui.trim_timeout.take();

                EntryObject::idle_trim();

                // Wait a bit for everything to be dropped. 5 seconds is way too much, but
                // it can't really hurt anything.
                glib::timeout_add_local_once(Duration::from_secs(5), || {
                    trace!("Explicitly trimming unused memory");
                    unsafe {
                        libc::malloc_trim(0);
                    }
                });
            });

            if let Some(old) = g.trim_timeout.replace(Some(timeout)) {
                old.remove();
            }
        });

        self.window.add_controller(focus);
    }

    fn setup_bookmarks(self: &Rc<Self>) {
        if CONFIG.bookmarks.is_empty() {
            return;
        }

        let container = &self.window.imp().bookmarks;

        let header = gtk::Label::builder()
            .label("Bookmarks")
            .css_classes(["left-header"])
            .xalign(0.0)
            .build();
        container.append(&header);

        for book in &CONFIG.bookmarks {
            let label = gtk::Label::builder()
                .label(&book.name)
                .tooltip_text(&book.action)
                .max_width_chars(1)
                .ellipsize(EllipsizeMode::End)
                .css_classes(["bookmark"])
                .xalign(0.0)
                .build();

            let click = gtk::GestureClick::new();
            click.set_button(1);
            let g = self.clone();
            click.connect_pressed(move |gc, _n, _x, _y| {
                let command = gc.widget().unwrap().tooltip_text().unwrap();
                info!("Running command from clicked bookmark: {command}");
                g.run_command_active(&command);
            });

            label.add_controller(click);

            container.append(&label);
        }

        let header = gtk::Label::builder()
            .label("Tabs")
            .css_classes(["left-header"])
            .xalign(0.0)
            .build();
        container.append(&header);
    }

    pub(super) fn rename_dialog(self: &Rc<Self>, tab: TabId, eo: EntryObject) {
        let path = eo.get().abs_path.clone();

        let Some(fname) = path.file_name() else {
            return info!("Can't rename without file name");
        };

        let dialog = gtk::Window::builder()
            .title("Rename")
            .transient_for(&self.window)
            .modal(true)
            .build();

        self.close_on_quit_or_esc(&dialog);

        dialog.set_default_width(800);

        let vbox = gtk::Box::new(Orientation::Vertical, 12);

        let label = gtk::Label::new(Some(&path.to_string_lossy()));
        label.set_margin_start(8);
        label.set_margin_end(8);
        label.set_wrap(true);
        label.set_wrap_mode(WrapMode::WordChar);
        label_attributes(&label);

        vbox.append(&label);

        let entry = gtk::Entry::new();
        entry.set_text(&fname.to_string_lossy());

        let end_pos = if eo.get().dir() {
            -1
        } else if let Some(stem) = path.file_stem() {
            stem.to_string_lossy().chars().count() as i32
        } else {
            -1
        };

        let d = dialog.downgrade();
        let rename = move |e: &gtk::Entry| {
            d.upgrade().unwrap().destroy();

            let new_name = e.text();
            if new_name.is_empty() || new_name.contains('/') || new_name.contains('\\') {
                return show_warning(format!("Invalid name for file \"{new_name}\""));
            }

            let new_path = path.parent().unwrap().join(new_name);
            if *path == *new_path {
                info!("Ignoring rename to same name for {new_path:?}");
                return;
            }

            gui_run(|g| {
                g.start_operation(tab, Kind::Rename(new_path.into()), vec![path.clone()].into())
            });
        };

        // activates-default is slow, so clone this closure instead
        entry.connect_activate(rename.clone());

        vbox.append(&entry);

        let actions = wrap_in_box_with_close_button(&dialog, vbox, "Cancel");

        let confirm = gtk::Button::with_label("Rename");
        let e = entry.clone();
        confirm.connect_clicked(move |_| rename(&e));

        actions.append(&confirm);

        dialog.connect_close_request(move |d| {
            d.destroy();
            Propagation::Proceed
        });

        dialog.set_visible(true);

        entry.set_enable_undo(true);
        entry.select_region(0, end_pos);
    }

    pub(super) fn create_dialog(self: &Rc<Self>, tab: TabId, dir: Arc<Path>, folder: bool) {
        let dialog = gtk::Window::builder()
            .title(if folder { "Create Folder" } else { "Create File" })
            .transient_for(&self.window)
            .modal(true)
            .build();

        self.close_on_quit_or_esc(&dialog);

        dialog.set_default_width(800);

        let vbox = gtk::Box::new(Orientation::Vertical, 12);

        let label = gtk::Label::new(Some(&format!(
            "Create new {} in {dir:?}",
            if folder { "folder" } else { "file" }
        )));
        label.set_margin_start(8);
        label.set_margin_end(8);
        label.set_wrap(true);
        label.set_wrap_mode(WrapMode::WordChar);
        label_attributes(&label);

        let d = dialog.downgrade();
        let create = move |e: &gtk::Entry| {
            d.upgrade().unwrap().destroy();

            let new_name = e.text();
            if new_name.is_empty() || new_name.contains('/') || new_name.contains('\\') {
                return show_warning(format!("Invalid name for file \"{new_name}\""));
            }

            let path = dir.join(new_name).into();
            gui_run(|g| {
                g.start_operation(
                    tab,
                    if folder { Kind::MakeDir(path) } else { Kind::MakeFile(path) },
                    VecDeque::new(),
                )
            });
        };

        let entry = gtk::Entry::new();
        // activates-default is slow, so clone this closure instead
        entry.connect_activate(create.clone());


        vbox.append(&label);
        vbox.append(&entry);

        let actions = wrap_in_box_with_close_button(&dialog, vbox, "Cancel");

        let confirm = gtk::Button::with_label("Create");
        let e = entry.clone();
        confirm.connect_clicked(move |_| create(&e));

        actions.append(&confirm);

        dialog.connect_close_request(move |d| {
            d.destroy();
            Propagation::Proceed
        });

        dialog.set_visible(true);

        entry.set_enable_undo(true);
    }

    pub(super) fn close_on_quit_or_esc<T: WidgetExt>(self: &Rc<Self>, w: &T) {
        let key = gtk::EventControllerKey::new();
        let g = self.clone();
        key.connect_key_pressed(move |e, key, _b, mods| {
            if (mods.is_empty() && key == Key::Escape)
                || g.shortcut_from_key(key, mods).is_some_and(|s| s == "Quit")
            {
                e.widget()
                    .and_downcast::<gtk::Window>()
                    .expect("Dialog was somehow not a window")
                    .close();
            }
            Propagation::Proceed
        });

        w.add_controller(key);
    }

    fn shortcut_from_key(self: &Rc<Self>, k: Key, mods: ModifierType) -> Option<&String> {
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
                .unwrap_or_else(|| panic!("Could not decode Key: {}", &s.key));
            inner.insert(k, s.action.clone());
        }
        shortcuts
    }

    pub(super) fn parse_mouse_actions() -> AHashMap<ModifierType, AHashMap<u32, String>> {
        let mut actions = AHashMap::new();

        for s in &CONFIG.mouse_buttons {
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

            let inner = match actions.entry(modifiers) {
                hash_map::Entry::Occupied(inner) => inner.into_mut(),
                hash_map::Entry::Vacant(vacant) => vacant.insert(AHashMap::new()),
            };

            inner.insert(s.button, s.action.clone());
        }
        actions
    }

    pub(super) fn run_command_active(self: &Rc<Self>, cmd: &str) {
        let target = self.tabs.borrow().active_action_target();
        self.run_command(target, cmd)
    }

    pub(super) fn run_mouse_command(
        self: &Rc<Self>,
        target: ActionTarget,
        button: u32,
        mods: ModifierType,
    ) {
        let mods = mods & !ModifierType::LOCK_MASK;

        let Some(cmd) = self.mouse_actions.get(&mods).and_then(|m| m.get(&button)) else {
            return;
        };

        self.run_command(target, cmd);
    }

    pub(super) fn run_command(self: &Rc<Self>, target: ActionTarget, cmd: &str) {
        // Do not trim the end of cmd because files and directories can end in spaces
        let cmd = cmd.trim_start();

        debug!("Running command {cmd} in {target:?}");

        // This may not be worth the headache, but it saves a fair bit of boilerplate
        let mut tabs = self.tabs.borrow_mut();


        if let Some((cmd, arg)) = cmd.split_once(' ') {
            let arg = arg.trim_start();

            let _ = match cmd {
                "Display" => match DisplayMode::from_str(arg) {
                    Ok(m) => return tabs.display_mode(target, m),
                    Err(_e) => true,
                },
                "SortBy" => match SortMode::from_str(arg) {
                    Ok(m) => return tabs.sort_mode(target, m),
                    Err(_e) => true,
                },
                "SortDir" => match SortDir::from_str(arg) {
                    Ok(d) => return tabs.sort_direction(target, d),
                    Err(_e) => true,
                },

                "Navigate" => return tabs.navigate(target, Path::new(arg)),
                "JumpTo" => return tabs.jump(target, Path::new(arg)),
                "NewTab" => return tabs.open_tab(Path::new(arg), TabPosition::After(target), true),
                "NewBackgroundTab" => {
                    return tabs.open_tab(Path::new(arg), TabPosition::After(target), false);
                }

                "Split" => match arg {
                    "horizontal" => {
                        return tabs.visible_split(target, Orientation::Horizontal, None);
                    }
                    "vertical" => {
                        return tabs.visible_split(target, Orientation::Vertical, None);
                    }
                    _ => true,
                },

                "Search" => return tabs.search(target, arg),

                "SaveSession" => {
                    if let Some(session) = tabs.get_session() {
                        self.database.save_session(arg.to_owned(), session);
                    } else {
                        show_warning("No tabs open to save as session");
                    }
                    return;
                }
                "LoadSession" => {
                    if let Some(session) = self.database.load_session(arg.to_string()) {
                        tabs.load_session(session);
                    } else {
                        show_warning(format!("No session named \"{arg}\" found"));
                    }
                    return;
                }
                "DeleteSession" => {
                    return self.database.delete_session(arg.to_string());
                }

                "Execute" => {
                    drop(tabs);
                    return self.send_manager(ManagerAction::Execute(
                        PathBuf::from(arg).into(),
                        self.get_env(),
                    ));
                }
                "Script" => {
                    drop(tabs);
                    return self.send_manager(ManagerAction::Script(
                        PathBuf::from(arg).into(),
                        target,
                        self.get_env(),
                    ));
                }

                _ => true,
            };
        }

        let _ = match cmd {
            "Quit" => {
                closing::close();
                return self.window.close();
            }
            "Help" => return self.help_dialog(),
            "Activate" => return tabs.activate(target),
            "OpenDefault" => return tabs.open_default(target),
            "OpenWith" => return tabs.open_with(target),

            "Copy" => return tabs.copy(target),
            "Cut" => return tabs.cut(target),
            "Paste" => return tabs.paste(target),

            "Cancel" => {
                drop(tabs);
                return self.cancel_operations();
            }
            "Undo" => {
                drop(tabs);
                // TODO -- should this be tied to a specific tab? or directory?
                return self.undo_operation();
            }

            "Home" => {
                return tabs.navigate(target, &home_dir().unwrap_or_default());
            }

            "NewTab" => return tabs.new_tab(target, true),
            "NewBackgroundTab" => return tabs.new_tab(target, false),
            "ReopenTab" => return tabs.reopen(),

            "Refresh" => return tabs.refresh(target),
            "RefreshAll" => return tabs.refresh_all(),
            "ReloadActions" => return self.menu.get().unwrap().rebuild_menu(self),

            "CloseTab" => return tabs.close_tab(target),
            "ClosePane" => return tabs.close_pane(target),
            "HidePanes" => return tabs.hide_all_visible(target),
            "CloseActive" | "CloseTabNoReplacement" => {
                return tabs.close_tab_no_replacement(target);
            }

            "Forward" => return tabs.forward(target),
            "Back" => return tabs.back(target),
            "Parent" => return tabs.parent(target),
            "Child" => return tabs.child(target),
            "BackOrParent" => return tabs.back_or_parent(target),

            "Trash" => return tabs.trash(target),
            "Delete" => return tabs.active_delete(target),

            "Rename" => return tabs.rename(target),
            "Properties" => return tabs.properties(target),

            "FocusLocation" => return tabs.focus_location_bar(target),
            "Unselect" => return tabs.unselect(target),

            "NewFolder" => return tabs.create(target, true),
            "NewFile" => return tabs.create(target, false),

            "Search" => return tabs.search(target, ""),
            _ => true,
        };

        let e = format!("Unrecognized command {cmd:?}");
        warn!("{e}");
        self.warning(&e);
    }

    pub(super) fn get_env(&self) -> Vec<(String, OsString)> {
        self.tabs.borrow().get_env()
    }
}

// Returns the box containing the actions so that more can be added
pub(super) fn wrap_in_box_with_close_button(
    dialog: &Window,
    contents: impl IsA<Widget>,
    label: &str,
) -> gtk::Box {
    let action_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    action_box.set_halign(gtk::Align::End);
    action_box.add_css_class("action-box");

    let w = dialog.downgrade();
    let close = gtk::Button::with_label(label);
    close.connect_clicked(move |_| {
        w.upgrade().unwrap().close();
    });

    action_box.append(&close);

    let dialog_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    dialog_box.append(&contents);
    dialog_box.append(&action_box);

    dialog.set_child(Some(&dialog_box));

    action_box
}
