use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use ahash::AHashMap;
use gtk::gdk::ModifierType;
use gtk::glib::{ControlFlow, SourceId, WeakRef};
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{gdk, gio, glib, Bitset, MultiSelection};
use tokio::sync::mpsc::UnboundedSender;

use self::main_window::MainWindow;
use self::operations::Operation;
use self::tabs::list::TabsList;
use self::thumbnailer::Thumbnailer;
use super::com::*;
use crate::closing;
use crate::config::CONFIG;
use crate::database::DBCon;

mod applications;
mod clipboard;
mod input;
mod main_window;
mod menu;
mod operations;
mod tabs;
mod thumbnailer;

// The Rc<> ends up more ergonomic in most cases but it's too much of a pain to pass things into
// GObjects.
// Rc<RefCell<Option<Gui>>> might work better in some cases.
thread_local!(static GUI: OnceCell<Rc<Gui>> = OnceCell::default());

fn gui_run<R, F: FnOnce(&Rc<Gui>) -> R>(f: F) -> R {
    GUI.with(|g| f(g.get().unwrap()))
}

fn show_warning(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    warn!("{msg}");
    gui_run(|g| g.warning(msg))
}

fn show_error(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    error!("{msg}");
    gui_run(|g| g.error(msg))
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
    memorized_size: (u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbPriority {
    Low,
    // Bound, but not mapped (visible)
    Medium,
    // Visible
    High,
}

pub fn queue_thumb(weak: WeakRef<EntryObject>, p: ThumbPriority, from_event: bool) {
    gui_run(|g| g.thumbnailer.queue(weak, p, from_event));
}


#[derive(Debug)]
struct Gui {
    window: MainWindow,
    win_state: Cell<WindowState>,
    menu: OnceCell<menu::GuiMenu>,

    // Tabs can recursively look for each other.
    tabs: RefCell<TabsList>,

    database: DBCon,
    thumbnailer: Thumbnailer,

    open_dialogs: RefCell<input::OpenDialogs>,
    shortcuts: AHashMap<ModifierType, AHashMap<gdk::Key, String>>,

    ongoing_operations: RefCell<Vec<Rc<Operation>>>,

    manager_sender: UnboundedSender<ManagerAction>,

    warning_timeout: DebugIgnore<Cell<Option<SourceId>>>,
    idle_timeout: DebugIgnore<Cell<Option<SourceId>>>,
}

pub fn run(
    manager_sender: UnboundedSender<ManagerAction>,
    gui_receiver: glib::Receiver<GuiAction>,
) {
    let flags = if CONFIG.unique {
        gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::SEND_ENVIRONMENT
    } else {
        gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::NON_UNIQUE
    };

    let application = gtk::Application::new(Some("awused.aw-fm"), flags);

    let gui_to_manager = Cell::from(Some(manager_sender));
    let gui_receiver = Cell::from(Some(gui_receiver));

    application.connect_activate(move |a| {
        Gui::new(a, gui_to_manager.take().unwrap(), gui_receiver.take().unwrap());
    });

    // This is a stupid hack around glib trying to exert exclusive control over the command line.
    application.connect_command_line(|a, _| {
        GUI.with(|g| match g.get() {
            None => a.activate(),
            Some(_g) => todo!("TODO -- Handling command line from another process"),
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

        let provider = gtk::CssProvider::new();
        let style = include_str!("style.css");
        if let Some(bg) = CONFIG.background_colour {
            window.remove_css_class("background");
            window.imp().overlay.add_css_class("main-nobg");

            provider.load_from_data(&format!("{style}\n window.main {{ background: {bg}; }}"));
        } else {
            provider.load_from_data(style);
        }

        // We give the CssProvider to the default screen so the CSS rules we added
        // can be applied to our window.
        gtk::style_context_add_provider_for_display(
            &window.display(),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );


        let tabs = TabsList::new(&window);

        let rc = Rc::new(Self {
            window,
            win_state: Cell::default(),
            menu: OnceCell::default(),

            tabs: tabs.into(),

            database: DBCon::connect(),
            thumbnailer: Thumbnailer::new(),

            open_dialogs: RefCell::default(),
            shortcuts: Self::parse_shortcuts(),

            ongoing_operations: RefCell::default(),

            manager_sender,

            warning_timeout: DebugIgnore::default(),
            idle_timeout: DebugIgnore::default(),
        });

        let g = rc.clone();
        GUI.with(|cell| cell.set(g).unwrap());


        rc.menu.set(menu::GuiMenu::new(&rc)).unwrap();

        let g = rc.clone();
        application.connect_shutdown(move |_a| {
            info!("Shutting down application");

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

        rc
    }

    fn setup(self: &Rc<Self>) {
        self.tabs.borrow_mut().initial_setup();
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
        let g = self.clone();
        self.window.connect_close_request(move |_w| {
            g.cancel_operations();
            //     let s = g.win_state.get();
            //     let size = if s.maximized || s.fullscreen {
            //         s.memorized_size
            //     } else {
            //         (w.width(), w.height()).into()
            //     };
            //     save_settings(State { size, maximized: w.is_maximized() });
            glib::Propagation::Proceed
        });

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

        let fullscreen = self.window.is_fullscreen();

        let maximized = self.window.is_maximized();

        // These callbacks run after the state has changed.
        if !s.maximized && !s.fullscreen {
            s.memorized_size =
                (self.window.width().unsigned_abs(), self.window.height().unsigned_abs());
        }

        s.maximized = maximized;
        s.fullscreen = fullscreen;
        self.win_state.set(s);
    }

    fn handle_update(self: &Rc<Self>, gu: GuiAction) -> ControlFlow {
        use crate::com::GuiAction::*;

        match gu {
            Snapshot(snap) => self.tabs.borrow_mut().apply_snapshot(snap),
            Update(update) => self.tabs.borrow_mut().update(update),
            SearchSnapshot(snap) => self.tabs.borrow_mut().apply_search_snapshot(snap),
            SearchUpdate(update) => self.tabs.borrow_mut().search_update(update),
            DirectoryOpenError(path, error) => {
                // This is a special case where we failed to open a directory or read it at all.
                // Treat it as if it were closed.
                self.error(&error);
                error!("Treating {path:?} as closed due to: {error}");
                self.tabs.borrow_mut().directory_failure(path);
            }
            DirectoryError(_, error) | EntryReadError(_, _, error) | ConveyError(error) => {
                self.error(&error);
            }
            Action(action) => self.run_command(&action),
            Quit => {
                self.window.close();
                closing::close();
                self.cancel_operations();
                return ControlFlow::Break;
            }
        }
        ControlFlow::Continue
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

    // Shows a warning that times out and doesn't need to be dismissed.
    fn warning(self: &Rc<Self>, msg: &str) {
        let toast = &self.window.imp().toast;
        let last_warning = self.warning_timeout.take();

        if toast.is_visible() && last_warning.is_none() {
            // Warnings cannot preempt errors
            // TODO -- a queue?
            return;
        }

        if let Some(last_warning) = last_warning {
            last_warning.remove();
        }

        toast.set_text(msg);
        toast.set_visible(true);

        let g = self.clone();
        let timeout = glib::timeout_add_local_once(Duration::from_secs(10), move || {
            g.window.imp().toast.set_visible(false);
            g.warning_timeout.set(None);
        });

        self.warning_timeout.set(Some(timeout));
    }

    fn error(&self, msg: &str) {
        if let Some(warning) = self.warning_timeout.take() {
            warning.remove();
        }

        self.window.imp().toast.set_text(msg);
        self.window.imp().toast.set_visible(true);
    }
}


struct Selected<'a> {
    selection: &'a MultiSelection,
    selected: Bitset,
    pos: u32,
}

impl<'a> From<&'a MultiSelection> for Selected<'a> {
    fn from(selection: &'a MultiSelection) -> Self {
        let selected = selection.selection();
        Self { selection, selected, pos: 0 }
    }
}

impl<'a> Iterator for Selected<'a> {
    type Item = EntryObject;

    fn next(&mut self) -> Option<Self::Item> {
        if (self.pos as u64) < self.selected.size() {
            let index = self.selected.nth(self.pos);
            let obj = self.selection.item(index).unwrap();
            self.pos += 1;
            Some(obj.downcast().unwrap())
        } else {
            None
        }
    }
}

impl ExactSizeIterator for Selected<'_> {
    fn len(&self) -> usize {
        self.selected.size() as usize
    }
}

impl Selected<'_> {
    fn get(&self, i: u32) -> EntryObject {
        let index = self.selected.nth(i);
        self.selection.item(index).unwrap().downcast().unwrap()
    }
}
