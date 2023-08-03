use std::collections::hash_map;
use std::rc::Rc;

use ahash::AHashMap;
use gtk::gio::{Menu, MenuItem, SimpleAction, SimpleActionGroup};
use gtk::glib::{ToVariant, Variant, VariantTy};
use gtk::prelude::{ActionExt, ActionMapExt};
use gtk::traits::{GtkWindowExt, PopoverExt, WidgetExt};
use gtk::{PopoverMenu, PositionType};

use super::Gui;
use crate::com::{DirSettings, EntryObject};
use crate::config::{ContextMenuGroup, CONFIG};


#[derive(Debug)]
pub(super) struct GuiMenu {
    // Checkboxes

    // Radio buttons
    display: SimpleAction,
    sort_mode: SimpleAction,
    sort_dir: SimpleAction,

    menu: PopoverMenu,
}


enum GC {
    Display(Variant),
    SortMode(Variant),
    SortDir(Variant),
    Action(Variant),
}

impl From<&str> for GC {
    fn from(command: &str) -> Self {
        if let Some((cmd, arg)) = command.split_once(' ') {
            let arg = arg.trim_start();

            match cmd {
                "Display" => return Self::Display(arg.to_variant()),
                "SortBy" => return Self::SortMode(arg.to_variant()),
                "SortDir" => return Self::SortDir(arg.to_variant()),
                _ => {}
            }
        }

        Self::Action(command.to_variant())
    }
}

impl GC {
    const fn action(&self) -> &'static str {
        match self {
            Self::Display(_) => "Display",
            Self::SortMode(_) => "SortBy",
            Self::SortDir(_) => "SortDir",
            Self::Action(_) => "action",
        }
    }

    const fn variant(&self) -> &Variant {
        match self {
            Self::Display(v) | Self::SortMode(v) | Self::SortDir(v) | Self::Action(v) => v,
        }
    }

    fn simple_action(&self, g: &Rc<Gui>) -> SimpleAction {
        let sa = SimpleAction::new_stateful(
            self.action(),
            Some(VariantTy::new("s").unwrap()),
            &"".to_variant(),
        );

        let g = g.clone();
        sa.connect_activate(move |a, v| {
            let name = a.name();
            let arg = v.unwrap().str().unwrap();
            g.run_command(&format!("{name} {arg}"));
        });

        sa
    }
}

impl GuiMenu {
    pub(super) fn new(gui: &Rc<Gui>) -> Self {
        let display = GC::Display(().to_variant()).simple_action(gui);
        let sort_mode = GC::SortMode(().to_variant()).simple_action(gui);
        let sort_dir = GC::SortDir(().to_variant()).simple_action(gui);


        let command = SimpleAction::new(
            GC::Action(().to_variant()).action(),
            Some(VariantTy::new("s").unwrap()),
        );
        let g = gui.clone();
        command.connect_activate(move |_a, v| {
            let action = v.unwrap().str().unwrap();
            g.run_command(action);
        });

        let action_group = SimpleActionGroup::new();
        action_group.add_action(&display);
        action_group.add_action(&sort_mode);
        action_group.add_action(&sort_dir);
        action_group.add_action(&command);

        gui.window.insert_action_group("context-menu", Some(&action_group));

        Self {
            display,
            sort_mode,
            sort_dir,
            menu: Self::rebuild_menu(gui),
        }
    }

    fn rebuild_menu(gui: &Rc<Gui>) -> PopoverMenu {
        let menu = Menu::new();

        let mut submenus = AHashMap::new();
        let mut sections = AHashMap::new();

        for entry in &CONFIG.context_menu {
            let menuitem = MenuItem::new(Some(&entry.name), None);
            let cmd = GC::from(entry.action.trim_start());

            menuitem.set_action_and_target_value(
                Some(&format!("context-menu.{}", cmd.action())),
                Some(cmd.variant()),
            );

            let menu = match &entry.group {
                Some(ContextMenuGroup::Submenu(sm)) => match submenus.entry(sm.clone()) {
                    hash_map::Entry::Occupied(e) => e.into_mut(),
                    hash_map::Entry::Vacant(e) => {
                        let submenu = Menu::new();
                        menu.append_submenu(Some(sm), &submenu);
                        e.insert(submenu)
                    }
                },
                Some(ContextMenuGroup::Section(sc)) => match sections.entry(sc.clone()) {
                    hash_map::Entry::Occupied(e) => e.into_mut(),
                    hash_map::Entry::Vacant(e) => {
                        let section = Menu::new();
                        menu.append_section(Some(sc), &section);
                        e.insert(section)
                    }
                },
                None => &menu,
            };

            // menuitem.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
            menu.append_item(&menuitem);
        }

        let menu = PopoverMenu::from_model_full(&menu, gtk::PopoverMenuFlags::NESTED);
        menu.set_has_arrow(false);
        menu.set_position(PositionType::Right);
        menu.set_valign(gtk::Align::Start);
        menu.set_parent(&gui.window);

        let g = gui.clone();
        // When this dies, return focus to where it was before.
        if let Some(fc) = g.window.focus_widget() {
            menu.connect_closed(move |_| {
                // Hack around GTK PopoverMenus taking focus to the grave with them.
                g.window.set_focus(Some(&fc));
            });
        }

        menu
    }

    pub fn prepare(&self, settings: DirSettings, _entries: Vec<EntryObject>) -> PopoverMenu {
        self.display.change_state(&settings.display_mode.as_ref().to_variant());
        self.sort_mode.change_state(&settings.sort.mode.as_ref().to_variant());
        self.sort_dir.change_state(&settings.sort.direction.as_ref().to_variant());

        self.menu.clone()
    }
}
