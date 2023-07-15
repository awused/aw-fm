// mod layout;
mod menu;
#[cfg(windows)]
mod windows;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::VecDeque;
use std::env::current_dir;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ahash::AHashMap;
use gnome_desktop::traits::DesktopThumbnailFactoryExt;
use gnome_desktop::DesktopThumbnailFactory;
use gtk::gdk::ModifierType;
use gtk::gio::ffi::GListStore;
use gtk::gio::{Cancellable, FileQueryInfoFlags, FILE_ATTRIBUTE_THUMBNAIL_PATH};
use gtk::glib::Object;
use gtk::prelude::*;
use gtk::Orientation::Horizontal;
use gtk::{
    gdk, gio, glib, Align, EventControllerScroll, EventControllerScrollFlags, GridView,
    MultiSelection, ScrolledWindow,
};
// use once_cell::unsync::OnceCell;
use path_clean::PathClean;
use tokio::sync::mpsc::UnboundedSender;

use self::tabs::TabsList;
// use self::layout::{LayoutContents, LayoutManager};
use super::com::*;
use crate::config::{CONFIG, OPTIONS};
use crate::database::DBCon;
// use crate::state_cache::{save_settings, State, STATE};
use crate::{closing, config};

mod tabs;

pub static WINDOW_ID: OnceLock<String> = OnceLock::new();

// The Rc<> ends up more ergonomic in most cases but it's too much of a pain to pass things into
// GObjects.
// Rc<RefCell<Option<Gui>>> might work better in some cases.
thread_local!(static GUI: OnceCell<Rc<Gui>> = OnceCell::default());

#[derive(Debug, Copy, Clone, Default)]
struct WindowState {
    maximized: bool,
    fullscreen: bool,
    // This stores the size of the window when it isn't fullscreen or maximized.
    memorized_size: crate::com::Res,
}


#[derive(Debug)]
struct Gui {
    window: gtk::ApplicationWindow,
    win_state: Cell<WindowState>,
    overlay: gtk::Overlay,
    menu: OnceCell<menu::GuiMenu>,

    // RefCell<Vec<Tab>>>
    // Tabs can recursively look for each other.
    tabs: RefCell<TabsList>,

    database: DBCon,

    page_num: gtk::Label,
    page_name: gtk::Label,
    archive_name: gtk::Label,
    mode: gtk::Label,
    zoom_level: gtk::Label,
    edge_indicator: gtk::Label,
    bottom_bar: gtk::Box,
    label_updates: RefCell<Option<glib::SourceId>>,

    // layout_manager: RefCell<LayoutManager>,
    // Called "pad" scrolling to differentiate it with continuous scrolling between pages.
    pad_scrolling: Cell<bool>,
    drop_next_scroll: Cell<bool>,
    animation_playing: Cell<bool>,

    last_action: Cell<Option<Instant>>,
    first_content_paint: OnceCell<()>,
    // open_dialogs: RefCell<input::OpenDialogs>,
    // shortcuts: AHashMap<ModifierType, AHashMap<gdk::Key, String>>,
    manager_sender: Rc<UnboundedSender<MAWithResponse>>,

    #[cfg(windows)]
    win32: windows::WindowsEx,
}

pub fn run(
    manager_sender: UnboundedSender<MAWithResponse>,
    gui_receiver: glib::Receiver<GuiAction>,
) {
    let flags = if CONFIG.unique {
        gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::SEND_ENVIRONMENT
    } else {
        gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::NON_UNIQUE
    };

    let application = gtk::Application::new(Some("awused.aw-fm"), flags);

    let gui_to_manager = Rc::from(manager_sender);
    let gui_receiver = Cell::from(Some(gui_receiver));

    application.connect_activate(move |a| {
        let provider = gtk::CssProvider::new();
        let bg = CONFIG.background_colour.unwrap_or(gdk::RGBA::BLACK);
        provider.load_from_data(
            &(include_str!("style.css").to_string()
                + &format!("\n window {{ background: {bg}; }}")),
        );

        // We give the CssProvider to the default screen so the CSS rules we added
        // can be applied to our window.
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("Error initializing gtk css provider."),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        Gui::new(a, gui_to_manager.clone(), &gui_receiver);
    });

    // This is a stupid hack around glib trying to exert exclusive control over the command line.
    application.connect_command_line(|a, _| {
        if GUI.with(|g| g.get().is_none()) {
            a.activate();
        } else {
            println!("Handling command line from another process")
        }
        0
    });

    let _cod = closing::CloseOnDrop::default();
    application.run();
}

impl Gui {
    pub fn new(
        application: &gtk::Application,
        manager_sender: Rc<UnboundedSender<MAWithResponse>>,
        gui_receiver: &Cell<Option<glib::Receiver<GuiAction>>>,
    ) -> Rc<Self> {
        let window = gtk::ApplicationWindow::new(application);

        let rc = Rc::new(Self {
            window,
            win_state: Cell::default(),
            overlay: gtk::Overlay::new(),
            menu: OnceCell::default(),

            tabs: RefCell::new(TabsList::new_uninit()),

            database: DBCon::connect(),

            page_num: gtk::Label::new(None),
            page_name: gtk::Label::new(None),
            archive_name: gtk::Label::new(None),
            mode: gtk::Label::new(None),
            zoom_level: gtk::Label::new(Some("100%")),
            edge_indicator: gtk::Label::new(None),
            bottom_bar: gtk::Box::new(Horizontal, 15),
            label_updates: RefCell::default(),

            // bg: Cell::new(config::CONFIG.background_colour.unwrap_or(gdk::RGBA::BLACK)),

            // layout_manager: RefCell::new(LayoutManager::new(weak.clone())),
            pad_scrolling: Cell::default(),
            drop_next_scroll: Cell::default(),
            animation_playing: Cell::new(true),

            last_action: Cell::default(),
            first_content_paint: OnceCell::default(),
            // open_dialogs: RefCell::default(),
            // shortcuts: Self::parse_shortcuts(),
            manager_sender,

            #[cfg(windows)]
            win32: windows::WindowsEx::default(),
        });

        let g = rc.clone();
        GUI.with(|cell| cell.set(g).unwrap());


        rc.menu.set(menu::GuiMenu::new(&rc)).unwrap();

        let g = rc.clone();
        application.connect_shutdown(move |_a| {
            info!("Shutting down application");
            #[cfg(windows)]
            g.win32.teardown();

            g.database.destroy();

            closing::close();
        });

        // We only support running once so this should never panic.
        // If there is a legitimate use for activating twice, send on the other channel.
        // There are also cyclical references that are annoying to clean up so this Gui object will
        // live forever, but that's fine since the application will exit when the Gui exits.
        let g = rc.clone();
        gui_receiver
            .take()
            .expect("Activated application twice. This should never happen.")
            .attach(None, move |gu| g.handle_update(gu));

        rc.setup();

        // Hack around https://github.com/gtk-rs/gtk4-rs/issues/520
        #[cfg(windows)]
        rc.win32.setup(rc.clone());

        rc
    }

    fn setup(self: &Rc<Self>) {
        let mut path = OPTIONS
            .file_name
            .clone()
            .unwrap_or_else(|| current_dir().unwrap_or_else(|_| "/".into()))
            .clean();

        if path.is_relative() {
            // prepending "/" is likely to be wrong, but eh.
            let mut abs = current_dir().unwrap_or_else(|_| "/".into());
            abs.push(path);
            path = abs.clean();
        }

        self.tabs.borrow_mut().initialize(path);

        self.layout();
        // self.setup_interaction();


        // let g = self.clone();
        // TODO -- handle resizing
        // self.canvas.connect_resize(move |_, width, height| {
        //     // Resolution change is also a user action.
        //     g.last_action.set(Some(Instant::now()));
        //
        //     assert!(width >= 0 && height >= 0, "Can't have negative width or height");
        //
        //     g.send_manager((ManagerAction::Resolution, GuiActionContext::default(), None));
        // });

        // TODO -- state cache - less necessary than aw-man
        // let g = self.clone();
        // self.window.connect_close_request(move |w| {
        //     let s = g.win_state.get();
        //     let size = if s.maximized || s.fullscreen {
        //         s.memorized_size
        //     } else {
        //         (w.width(), w.height()).into()
        //     };
        //     save_settings(State { size, maximized: w.is_maximized() });
        //     gtk::Inhibit(false)
        // });

        let g = self.clone();
        self.window.connect_maximized_notify(move |_w| {
            g.window_state_changed();
        });

        let g = self.clone();
        self.window.connect_fullscreened_notify(move |_w| {
            g.window_state_changed();
        });

        self.window.set_visible(true);
    }

    fn window_state_changed(self: &Rc<Self>) {
        let mut s = self.win_state.get();

        #[cfg(unix)]
        let fullscreen = self.window.is_fullscreen();
        #[cfg(windows)]
        let fullscreen = self.win32.is_fullscreen();

        let maximized = self.window.is_maximized();

        // These callbacks run after the state has changed.
        if !s.maximized && !s.fullscreen {
            s.memorized_size = (self.window.width(), self.window.height()).into();
        }

        s.maximized = maximized;
        s.fullscreen = fullscreen;
        self.win_state.set(s);
    }

    fn layout(self: &Rc<Self>) {
        self.window.remove_css_class("background");
        self.window.set_title(Some("aw-fm"));
        self.window.set_default_size(800, 600);


        // let cancellable = Cancellable::new();
        // let thumbnailer =
        // DesktopThumbnailFactory::new(gnome_desktop::DesktopThumbnailSize::Normal);

        // thumbnailer.can_thumbnail(f.uri().as_str(), mime_type, mtime)


        //
        // let scroll_clone = scroller.clone();
        // let last_frame_counter = RefCell::new(0);
        // controller.connect_scroll(move |_, _, _| {
        //     let mut last_frame_counter = last_frame_counter.borrow_mut();
        //     let new_frame_counter = scroll_clone.frame_clock().unwrap().frame_counter() + 1;
        //     if *last_frame_counter <= new_frame_counter {
        //         warn!("inhibit");
        //         gtk::Inhibit(false) // Inhibit scroll event to work around bug: https://gitlab.gnome.org/GNOME/gtk/-/issues/2971
        //     } else {
        //         *last_frame_counter = new_frame_counter;
        //         gtk::Inhibit(false)
        //     }
        // });
        //
        // scroller.add_controller(&controller);
        //
        //
        // if let Some(saved) = &*STATE {
        //     // Don't create very tiny windows.
        //     if saved.size.w >= 100 && saved.size.h >= 100 {
        //         self.window.set_default_size(saved.size.w as i32, saved.size.h as i32);
        //         let mut ws = self.win_state.get();
        //         ws.memorized_size = saved.size;
        //         self.win_state.set(ws);
        //     }
        //
        //     if saved.maximized {
        //         self.window.set_maximized(true);
        //     }
        // }

        // TODO -- three separate indicators?
        self.page_name.set_wrap(true);
        self.archive_name.set_wrap(true);

        // Left side -- right to left
        self.bottom_bar.prepend(&self.page_name);
        self.bottom_bar.prepend(&gtk::Label::new(Some("|")));
        self.bottom_bar.prepend(&self.archive_name);
        self.bottom_bar.prepend(&gtk::Label::new(Some("|")));
        self.bottom_bar.prepend(&self.page_num);

        // TODO -- replace with center controls ?
        // self.edge_indicator.set_hexpand(true);
        self.edge_indicator.set_halign(Align::End);

        // Right side - left to right
        self.bottom_bar.append(&self.edge_indicator);
        self.bottom_bar.append(&self.zoom_level);
        self.bottom_bar.append(&gtk::Label::new(Some("|")));
        self.bottom_bar.append(&self.mode);

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // vbox.set_vexpand(true);

        self.tabs.borrow_mut().layout(&vbox);

        // vbox.prepend(&self.overlay);

        self.window.set_child(Some(&vbox));
    }

    fn handle_update(self: &Rc<Self>, gu: GuiAction) -> glib::Continue {
        use crate::com::GuiAction::*;

        // println!("{gu:?}");

        match gu {
            // Action(a, fin) => {
            //     // self.run_command(&a, Some(fin));
            // }
            Snapshot(snap) => {
                let g = self.clone();
                g.tabs.borrow_mut().apply_snapshot(snap);

                // glib::timeout_add_local_once(Duration::from_secs(20), move || {
                //     g.tabs.borrow_mut().close_tab(0);
                // });
                // // g.tabs.borrow_mut().apply_snapshot(snap);
            }
            Update(update) => {
                self.tabs.borrow_mut().handle_update(update);
            }
            // IdleUnload => {}
            Quit => {
                self.window.close();
                closing::close();
                return glib::Continue(false);
            }
        }
        glib::Continue(true)
    }

    fn send_manager(&self, val: MAWithResponse) {
        if let Err(e) = self.manager_sender.send(val) {
            if !closing::closed() {
                // This should never happen
                error!("Sending to manager unexpectedly failed. {e}");
                closing::close();
                self.window.close();
            }
        }
    }

    fn convey_error(&self, msg: String) {
        error!("Unimplemented convey_error");
    }
}
