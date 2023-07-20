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
use gtk::glib::{Object, WeakRef};
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::Orientation::Horizontal;
use gtk::{
    gdk, gio, glib, Align, EventControllerScroll, EventControllerScrollFlags, GridView,
    MultiSelection, ScrolledWindow,
};
use path_clean::PathClean;
use tokio::sync::mpsc::UnboundedSender;

use self::main_window::MainWindow;
use self::tabs::TabsList;
use self::thumbnailer::Thumbnailer;
use super::com::*;
use crate::config::{CONFIG, OPTIONS};
use crate::database::DBCon;
use crate::{closing, config};

mod applications;
mod input;
mod main_window;
mod tabs;
mod thumbnailer;

pub static WINDOW_ID: OnceLock<String> = OnceLock::new();

// The Rc<> ends up more ergonomic in most cases but it's too much of a pain to pass things into
// GObjects.
// Rc<RefCell<Option<Gui>>> might work better in some cases.
thread_local!(static GUI: OnceCell<Rc<Gui>> = OnceCell::default());

fn gui_run<R, F: FnOnce(&Rc<Gui>) -> R>(f: F) -> R {
    GUI.with(|g| f(g.get().unwrap()))
}

fn tabs_run<R, F: FnOnce(&mut TabsList) -> R>(f: F) -> R {
    gui_run(|g| {
        let mut tabs = g.tabs.borrow_mut();
        f(&mut tabs)
    })
}

#[derive(Debug, Copy, Clone, Default)]
struct WindowState {
    maximized: bool,
    fullscreen: bool,
    // This stores the size of the window when it isn't fullscreen or maximized.
    memorized_size: crate::com::Res,
}

pub fn queue_high_priority_thumb(weak: WeakRef<EntryObject>) {
    gui_run(|g| g.thumbnailer.high_priority(weak));
}

pub fn queue_low_priority_thumb(weak: WeakRef<EntryObject>) {
    gui_run(|g| g.thumbnailer.low_priority(weak));
}


#[derive(Debug)]
struct Gui {
    window: MainWindow,
    win_state: Cell<WindowState>,
    overlay: gtk::Overlay,
    menu: OnceCell<menu::GuiMenu>,

    // Tabs can recursively look for each other.
    tabs: RefCell<TabsList>,

    database: DBCon,
    thumbnailer: Thumbnailer,

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
    open_dialogs: RefCell<input::OpenDialogs>,
    shortcuts: AHashMap<ModifierType, AHashMap<gdk::Key, String>>,

    manager_sender: UnboundedSender<ManagerAction>,

    #[cfg(windows)]
    win32: windows::WindowsEx,
}

pub fn run(
    manager_sender: UnboundedSender<ManagerAction>,
    gui_receiver: glib::Receiver<GuiAction>,
) {
    let start = Instant::now();
    println!("{:?}", gio::AppInfo::default_for_type("video/mp4", false).unwrap().name());
    println!("Listed applications in {:?}", start.elapsed());
    println!("{:?}", gio::AppInfo::default_for_type("video/mp4", false).unwrap().name());
    println!("Listed applications in {:?}", start.elapsed());

    let flags = if CONFIG.unique {
        gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::SEND_ENVIRONMENT
    } else {
        gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::NON_UNIQUE
    };

    let application = gtk::Application::new(Some("awused.aw-fm"), flags);

    let gui_to_manager = Cell::from(Some(manager_sender));
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
        Gui::new(a, gui_to_manager.take().unwrap(), gui_receiver.take().unwrap());
    });

    // This is a stupid hack around glib trying to exert exclusive control over the command line.
    application.connect_command_line(|a, _| {
        GUI.with(|g| match g.get() {
            None => a.activate(),
            Some(g) => println!("Handling command line from another process"),
        });
        0
    });

    let _cod = closing::CloseOnDrop::default();
    application.run();
}

impl Gui {
    pub fn new(
        application: &gtk::Application,
        manager_sender: UnboundedSender<ManagerAction>,
        gui_receiver: glib::Receiver<GuiAction>,
    ) -> Rc<Self> {
        let window = MainWindow::new(application);
        window.remove_css_class("background");


        let tabs = TabsList::new(&window);

        let rc = Rc::new(Self {
            window,
            win_state: Cell::default(),
            overlay: gtk::Overlay::new(),
            menu: OnceCell::default(),

            tabs: tabs.into(),

            database: DBCon::connect(),
            thumbnailer: Thumbnailer::new(),

            page_num: gtk::Label::new(None),
            page_name: gtk::Label::new(None),
            archive_name: gtk::Label::new(None),
            mode: gtk::Label::new(None),
            zoom_level: gtk::Label::new(Some("100%")),
            edge_indicator: gtk::Label::new(None),
            bottom_bar: gtk::Box::new(Horizontal, 15),
            label_updates: RefCell::default(),

            pad_scrolling: Cell::default(),
            drop_next_scroll: Cell::default(),
            animation_playing: Cell::new(true),

            last_action: Cell::default(),
            first_content_paint: OnceCell::default(),
            open_dialogs: RefCell::default(),
            shortcuts: Self::parse_shortcuts(),

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
        gui_receiver.attach(None, move |gu| g.handle_update(gu));

        rc.setup();

        // Hack around https://github.com/gtk-rs/gtk4-rs/issues/520
        #[cfg(windows)]
        rc.win32.setup(rc.clone());

        rc
    }

    fn setup(self: &Rc<Self>) {
        self.tabs.borrow_mut().setup();
        self.setup_interaction();

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

    fn handle_update(self: &Rc<Self>, gu: GuiAction) -> glib::Continue {
        use crate::com::GuiAction::*;

        match gu {
            Snapshot(snap) => {
                let g = self.clone();
                g.tabs.borrow_mut().apply_snapshot(snap);

                // glib::timeout_add_local_once(Duration::from_secs(20), move || {
                //     g.tabs.borrow_mut().close_tab(0);
                // });
                // // g.tabs.borrow_mut().apply_snapshot(snap);
            }
            Update(update) => {
                self.tabs.borrow_mut().update(update);
            }
            DirectoryOpenError(path, error) => {
                // This is a special case where we failed to open a directory or read it at all.
                // Treat it as if it were closed.
                self.convey_error(&error);
                error!("Treating {path:?} as closed");
                self.tabs.borrow_mut().directory_failure(path);
            }
            DirectoryError(_, error) | EntryReadError(_, _, error) | ConveyError(error) => {
                self.convey_error(&error);
            }
            Action(action) => self.run_command(&action),
            Quit => {
                self.window.close();
                closing::close();
                return glib::Continue(false);
            }
        }
        glib::Continue(true)
    }

    fn send_manager(&self, val: ManagerAction) {
        if let Err(e) = self.manager_sender.send(val) {
            if !closing::closed() {
                // This should never happen
                error!("Sending to manager unexpectedly failed. {e}");
                closing::close();
                self.window.close();
            }
        }
    }

    fn convey_error(&self, msg: &str) {
        self.window.imp().toast.set_text(&msg);
        self.window.imp().toast.set_visible(true);
    }
}
