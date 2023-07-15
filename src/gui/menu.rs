use std::collections::hash_map::Entry;
use std::rc::Rc;

use ahash::AHashMap;
use gtk::gdk::Rectangle;
use gtk::gio::{Menu, MenuItem, SimpleActionGroup};
use gtk::glib::{ToVariant, Variant};
use gtk::traits::{EventControllerExt, GestureSingleExt, PopoverExt, RootExt, WidgetExt};
use gtk::{GestureClick, PopoverMenu, PositionType};

use super::Gui;
// use crate::com::Toggle;
use crate::config::CONFIG;

#[derive(Debug)]
pub(super) struct GuiMenu {
    // Checkboxes
    // manga: SimpleAction,
    // upscaling: SimpleAction,
    // TODO
    // fullscreen
    // show_ui/hide_ui
    // playing: SimpleAction,

    // Radio buttons
    // fit: SimpleAction,
    // display: SimpleAction,

    // Everything else
    // command: SimpleAction,
}

// TODO -- this can be redone with enums static mappings
fn action_for(mut command: &str) -> (&str, Option<Variant>) {
    // if let Some((cmd, arg)) = command.split_once(' ') {
    //     if let Ok(arg) = Toggle::try_from(arg.trim_start()) {
    //         match arg {
    //             Toggle::Change => command = cmd,
    //             // These don't work, can't be bothered to figure them out.
    //             Toggle::On | Toggle::Off => {}
    //         }
    //     }
    // }
    //

    match command {
        "ToggleMangaMode" | "MangaMode" => ("manga", None),
        "ToggleUpscaling" | "Upscaling" => ("upscaling", None),
        "FitToContainer" | "FitToWidth" | "FitToHeight" | "FullSize" => {
            ("fit", Some(command.to_variant()))
        }
        "SinglePage" | "VerticalStrip" | "HorizontalStrip" | "DualPage" | "DualPageReversed" => {
            ("display", Some(command.to_variant()))
        }
        "TogglePlaying" | "Playing" => ("playing", None),
        _ => ("action", Some(command.to_variant())),
    }
}


impl GuiMenu {
    pub(super) fn new(gui: &Rc<Gui>) -> Self {
        let s = Self {};

        s.setup(gui);
        s
    }

    fn setup(&self, gui: &Rc<Gui>) {
        // if CONFIG.context_menu.is_empty() {
        //     return;
        // }
        //
        // let action_group = SimpleActionGroup::new();
        //
        // gui.window.insert_action_group("context-menu", Some(&action_group));
        //
        // let menu = Menu::new();
        //
        // let mut submenus = AHashMap::new();
        // let mut sections = AHashMap::new();
        //
        // for entry in &CONFIG.context_menu {
        //     let menuitem = MenuItem::new(Some(&entry.name), None);
        //     let action = action_for(entry.action.trim());
        //
        //     menuitem.set_action_and_target_value(
        //         Some(&("context-menu.".to_owned() + action.0)),
        //         action.1.as_ref(),
        //     );
        //
        //     let menu = match &entry.group {
        //         Some(ContextMenuGroup::Submenu(sm)) => match submenus.entry(sm.clone()) {
        //             Entry::Occupied(e) => e.into_mut(),
        //             Entry::Vacant(e) => {
        //                 let submenu = Menu::new();
        //                 menu.append_submenu(Some(sm), &submenu);
        //                 e.insert(submenu)
        //             }
        //         },
        //         Some(ContextMenuGroup::Section(sc)) => match sections.entry(sc.clone()) {
        //             Entry::Occupied(e) => e.into_mut(),
        //             Entry::Vacant(e) => {
        //                 let section = Menu::new();
        //                 menu.append_section(Some(sc), &section);
        //                 e.insert(section)
        //             }
        //         },
        //         None => &menu,
        //     };
        //
        //     menu.append_item(&menuitem);
        // }
        //
        // let menu = PopoverMenu::from_model_full(&menu, gtk::PopoverMenuFlags::NESTED);
        // menu.set_has_arrow(false);
        // menu.set_parent(&gui.window);
        // menu.set_position(PositionType::Right);
        // menu.set_valign(gtk::Align::Start);
        //
        // let g = gui.clone();
        // menu.connect_closed(move |_| {
        //     // Nested hacks to avoid dropping two scroll events in a row.
        //     g.drop_next_scroll.set(false);
        //     // Hack around GTK PopoverMenus taking focus to the grave with them.
        //     g.window.set_focus(Some(&g.window));
        // });
        //
        // let right_click = GestureClick::new();
        // right_click.set_button(3);
        //
        // right_click.connect_pressed(move |e, _clicked, x, y| {
        //     let ev = e.current_event().unwrap();
        //     if ev.triggers_context_menu() {
        //         let rect = Rectangle::new(x as i32, y as i32, 1, 1);
        //         menu.set_pointing_to(Some(&rect));
        //         menu.popup();
        //     }
        // });
        //
        // gui.window.add_controller(right_click);
    }
}
