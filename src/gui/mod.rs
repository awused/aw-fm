use std::cell::{Cell, OnceCell, RefCell};
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use ahash::AHashMap;
use gnome_desktop::DesktopThumbnailSize;
use gtk::gdk::{ModifierType, Surface};
use gtk::glib::{ControlFlow, SourceId, WeakRef};
use gtk::pango::{AttrInt, AttrList};
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{gdk, gio, glib, Bitset, MultiSelection};
use path_clean::PathClean;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use self::main_window::MainWindow;
use self::operations::Operation;
use self::tabs::list::TabsList;
use self::thumbnailer::Thumbnailer;
use super::com::*;
use crate::closing;
use crate::config::CONFIG;
use crate::database::DBCon;
use crate::gui::tabs::list::TabPosition;
use crate::state_cache::{save_settings, State, STATE};

mod applications;
mod clipboard;
mod input;
mod main_window;
mod menu;
mod operations;
mod properties;
mod tabs;
mod thumbnailer;

pub use tabs::id::TabId;

// The Rc<> ends up more ergonomic in most cases but it's too much of a pain to pass things into
// GObjects.
// Rc<RefCell<Option<Gui>>> might work better in some cases.
thread_local! {
    static GUI: OnceCell<Rc<Gui>> = OnceCell::default();

    static PANGO_ATTRIBUTES: AttrList = {
        let pango_list = AttrList::new();
        pango_list.insert(AttrInt::new_insert_hyphens(false));
        pango_list
    }
}

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

// #[derive(Debug, Copy, Clone, Default)]
// struct WindowState {
//     maximized: bool,
//     fullscreen: bool,
//     // This stores the size of the window when it isn't fullscreen or maximized.
//     memorized_size: (u32, u32),
// }

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

pub fn thumb_size() -> DesktopThumbnailSize {
    gui_run(|g| g.thumbnailer.size.get())
}

#[derive(Debug)]
struct Gui {
    window: MainWindow,
    // win_state: Cell<WindowState>,
    menu: OnceCell<menu::GuiMenu>,

    // Tabs can recursively look for each other.
    tabs: RefCell<TabsList>,

    database: DBCon,
    thumbnailer: Thumbnailer,

    open_dialogs: RefCell<input::OpenDialogs>,
    shortcuts: AHashMap<ModifierType, AHashMap<gdk::Key, String>>,

    ongoing_operations: RefCell<Vec<Rc<Operation>>>,
    finished_operations: RefCell<VecDeque<Rc<Operation>>>,

    manager_sender: UnboundedSender<ManagerAction>,

    warning_timeout: DebugIgnore<Cell<Option<SourceId>>>,
    idle_timeout: DebugIgnore<Cell<Option<SourceId>>>,
}

pub fn run(
    manager_sender: UnboundedSender<ManagerAction>,
    gui_receiver: UnboundedReceiver<GuiAction>,
) {
    let flags = if CONFIG.unique {
        gio::ApplicationFlags::HANDLES_COMMAND_LINE
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
    application.connect_command_line(|a, cl| {
        GUI.with(|g| match g.get() {
            None => a.activate(),
            Some(g) => {
                let args = cl.arguments();

                let mut path = if args.len() < 2 {
                    let Some(cwd) = cl.cwd() else {
                        return show_warning("Got request to open tab with no directory");
                    };

                    cwd.clean()
                } else {
                    Path::new(&args[1]).clean()
                };


                if path.is_relative() {
                    let Some(cwd) = cl.cwd() else {
                        return show_warning(format!(
                            "Got request to open tab with relative path {path:?} and no working \
                             directory"
                        ));
                    };

                    path = cwd.join(path).clean();
                }

                if path.is_relative() {
                    return show_warning(format!("Could not make {path:?} absolute"));
                }

                g.tabs.borrow_mut().open_tab(path, TabPosition::End, true);
            }
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
        mut gui_receiver: UnboundedReceiver<GuiAction>,
    ) -> Rc<Self> {
        let window = MainWindow::new(application);

        let provider = gtk::CssProvider::new();
        let style = include_str!("style.css");
        if let Some(bg) = CONFIG.background_colour {
            window.remove_css_class("background");
            window.imp().overlay.add_css_class("main-nobg");

            provider.load_from_string(&format!("{style}\n window.main {{ background: {bg}; }}"));
        } else {
            provider.load_from_string(style);
        }

        // We give the CssProvider to the default screen so the CSS rules we added
        // can be applied to our window.
        gtk::style_context_add_provider_for_display(
            &window.display(),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        if let Some(saved) = &*STATE {
            // Don't create very tiny windows.
            if saved.size.0 >= 100 && saved.size.1 >= 100 {
                window.set_default_size(saved.size.0 as i32, saved.size.1 as i32);
            }

            if saved.maximized {
                window.set_maximized(true);
            }
        }


        let tabs = TabsList::new(&window);

        let rc = Rc::new(Self {
            window,
            // win_state: Cell::default(),
            menu: OnceCell::default(),

            tabs: tabs.into(),

            database: DBCon::connect(),
            thumbnailer: Thumbnailer::new(),

            open_dialogs: RefCell::default(),
            shortcuts: Self::parse_shortcuts(),

            ongoing_operations: RefCell::default(),
            finished_operations: RefCell::default(),

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

            g.tabs.borrow_mut().cancel_loads();
            closing::close();
        });

        // We only support running once so this should never panic.
        // If there is a legitimate use for activating twice, send on the other channel.
        // There are also cyclical references that are annoying to clean up so this Gui object will
        // live forever, but that's fine since the application will exit when the Gui exits.
        let ctx = glib::MainContext::ref_thread_default();
        let g = rc.clone();
        ctx.spawn_local_with_priority(glib::Priority::HIGH_IDLE, async move {
            while let Some(gu) = gui_receiver.recv().await {
                g.handle_update(gu);
            }
        });

        rc.setup();

        rc
    }

    fn setup(self: &Rc<Self>) {
        self.tabs.borrow_mut().initial_setup();
        self.setup_interaction();

        let g = self.clone();
        self.window.connect_close_request(move |w| {
            g.cancel_operations();

            save_settings(State {
                // Does not handle fullscreen state, probably fine
                size: (w.width() as u32, w.height() as u32),
                maximized: w.is_maximized(),
            });
            glib::Propagation::Proceed
        });

        self.window.set_visible(true);

        if !CONFIG.force_small_thumbnails {
            let g = self.clone();
            let check_dpi = move |surface: &Surface| {
                let scale = surface.scale();
                let size = if scale <= 1.2 {
                    DesktopThumbnailSize::Normal
                } else {
                    DesktopThumbnailSize::Large
                };

                if g.thumbnailer.size.get() == size {
                    return;
                };

                info!("Detected DPI change, switching to {size:?} thumbnails");
                g.thumbnailer.size.set(size);
                EntryObject::change_thumb_size(size);
            };

            if let Some(suf) = self.window.native().unwrap().surface() {
                check_dpi(&suf);
                suf.connect_scale_notify(check_dpi);
            } else {
                error!("Could not check DPI when window was set as visible");
            }
        }
    }

    fn handle_update(self: &Rc<Self>, gu: GuiAction) -> ControlFlow {
        use crate::com::GuiAction::*;

        match gu {
            Watching(id) => self.tabs.borrow_mut().mark_watching(id),
            Snapshot(snap) => self.tabs.borrow_mut().apply_snapshot(snap),
            Update(update) => self.tabs.borrow_mut().update(update),

            SearchSnapshot(snap) => self.tabs.borrow_mut().apply_search_snapshot(snap),
            SearchUpdate(update) => self.tabs.borrow_mut().search_update(update),
            DirChildren(id, children) => self.handle_properties_update(id, children),

            DirectoryOpenError(path, error) => {
                // This is a special case where we failed to open a directory or read it at all.
                // Treat it as if it were closed.
                self.error(&error);
                error!("Treating {path:?} as closed due to: {error}");
                self.tabs.borrow_mut().directory_failure(path);
            }
            DirectoryError(_, error) | EntryReadError(_, _, error) | ConveyError(error) => {
                self.error(error);
            }
            ConveyWarning(warning) => self.warning(warning),
            Action(action, target) => self.run_command(target, &action),
            Completion(completed) => self.tabs.borrow_mut().handle_completion(completed),
            Quit => {
                self.window.close();
                closing::close();
                self.cancel_operations();
                self.tabs.borrow_mut().cancel_loads();
                return ControlFlow::Break;
            }
        }
        ControlFlow::Continue
    }

    fn send_manager(&self, val: ManagerAction) {
        if let Err(e) = self.manager_sender.send(val) {
            if !closing::closed() {
                // This should never happen
                closing::fatal(format!("Sending to manager unexpectedly failed. {e}"));
                self.window.close();
            }
        }
    }

    // Shows a warning that times out and doesn't need to be dismissed.
    fn warning(self: &Rc<Self>, msg: impl AsRef<str>) {
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

        toast.set_text(msg.as_ref());
        toast.set_visible(true);

        let g = self.clone();
        let timeout = glib::timeout_add_local_once(Duration::from_secs(10), move || {
            g.window.imp().toast.set_visible(false);
            g.warning_timeout.set(None);
        });

        self.warning_timeout.set(Some(timeout));
    }

    fn error(&self, msg: impl AsRef<str>) {
        if let Some(warning) = self.warning_timeout.take() {
            warning.remove();
        }

        self.window.imp().toast.set_text(msg.as_ref());
        self.window.imp().toast.set_visible(true);
    }
}


struct Selected<'a> {
    selection: &'a MultiSelection,
    selected: Bitset,
    pos: u32,
    end: u32,
}

impl<'a> From<&'a MultiSelection> for Selected<'a> {
    fn from(selection: &'a MultiSelection) -> Self {
        let selected = selection.selection();
        let size = selected.size();
        if size > 0 && size <= u32::MAX as u64 {
            Self {
                selection,
                pos: selected.nth(0),
                end: selected.nth(size as u32 - 1),
                selected,
            }
        } else {
            Self { selection, pos: 1, end: 0, selected }
        }
    }
}

impl<'a> Iterator for Selected<'a> {
    type Item = EntryObject;

    fn next(&mut self) -> Option<Self::Item> {
        while self.pos <= self.end && !self.selected.contains(self.pos) {
            self.pos += 1;
        }

        if self.pos <= self.end {
            let obj = self.selection.item(self.pos).unwrap();
            self.pos += 1;
            Some(obj.downcast().unwrap())
        } else {
            None
        }

        // The old code, iterating through the bitset and looking up items by index, was much
        // slower during deletions for some reason - probably devolving into random access during
        // the item removal signal handler.
        //
        // The new code is only negligibly slower in the worst case where sparse selections are
        // made in a large set. Even selecting the first and last in 300k items is only ~1ms.
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

fn label_attributes(label: &gtk::Label) {
    PANGO_ATTRIBUTES.with(|pa| label.set_attributes(Some(pa)));
}
