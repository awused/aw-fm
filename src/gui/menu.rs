use std::borrow::Cow;
use std::collections::hash_map;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::unix::prelude::{OsStrExt, PermissionsExt};
use std::path::Path;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use ahash::AHashMap;
use gtk::gio::{Menu, MenuItem, SimpleAction, SimpleActionGroup};
use gtk::glib::{ToVariant, Variant, VariantTy};
use gtk::prelude::{ActionExt, ActionMapExt};
use gtk::traits::{GtkWindowExt, PopoverExt, WidgetExt};
use gtk::{PopoverMenu, PositionType};
use once_cell::unsync::Lazy;
use regex::bytes::Regex;

use super::Gui;
use crate::com::{DirSettings, Entry, EntryObject, ManagerAction};
use crate::config::{ContextMenuEntry, ContextMenuGroup, Selection, ACTIONS_DIR, CONFIG};
use crate::gui::clipboard;


#[derive(Eq, PartialEq)]
enum GC {
    Display(Variant),
    SortMode(Variant),
    SortDir(Variant),
    Paste,
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


        if command == "Paste" {
            Self::Paste
        } else {
            Self::Action(command.to_variant())
        }
    }
}

impl GC {
    const fn action(&self) -> &'static str {
        match self {
            Self::Display(_) => "Display",
            Self::SortMode(_) => "SortBy",
            Self::SortDir(_) => "SortDir",
            Self::Paste => "Paste",
            Self::Action(_) => "action",
        }
    }

    const fn variant(&self) -> Option<&Variant> {
        match self {
            Self::Display(v) | Self::SortMode(v) | Self::SortDir(v) | Self::Action(v) => Some(v),
            Self::Paste => None,
        }
    }

    const fn hide_if_disabled(&self) -> bool {
        match self {
            Self::Display(_) | Self::SortMode(_) | Self::SortDir(_) | Self::Action(_) => false,
            Self::Paste => true,
        }
    }

    fn simple_action(&self, g: &Rc<Gui>) -> SimpleAction {
        if *self == Self::Paste {
            let sa = SimpleAction::new(self.action(), None);

            let g = g.clone();
            sa.connect_activate(move |a, _v| {
                g.run_command(&a.name());
            });

            return sa;
        }

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


#[derive(Debug)]
struct ActionSettings {
    name: Option<String>,
    directories: bool,
    files: bool,
    mimetypes: Option<Vec<String>>,
    extensions: Option<Vec<String>>,
    regex: Option<Regex>,
    selection: Selection,
    priority: i32,
    parse_output: bool,
}

impl PartialEq for ActionSettings {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for ActionSettings {}
impl PartialOrd for ActionSettings {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ActionSettings {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority).then_with(|| self.name.cmp(&other.name))
    }
}

thread_local! {
    static SETTINGS_RE: Lazy<regex::Regex> = Lazy::new(||
        regex::Regex::new(
            r"(name|directories|files|mimetypes|extensions|regex|selection|priority|parse_output)=(.*)$")
                .unwrap());
}

impl ActionSettings {
    fn parse_script(path: &Path, read: impl Read) -> Option<Self> {
        // No more than 1MB
        let mut read = read.take(1024 * 1024);

        let mut contents = Vec::with_capacity(1024 * 1024);
        read.read_to_end(&mut contents)
            .map_err(|e| error!("Failed to read custom action in {path:?}: {e}"))
            .ok()?;

        let lossy = OsStr::from_bytes(&contents).to_string_lossy();
        let mut lines = lossy.lines();

        if !lines.any(|l| l[0..50.min(l.len())].contains("**aw-fm-settings-begin**")) {
            error!("Found no beginning of settings line in {path:?}");
            return None;
        }

        let mut name = None;
        let mut directories = true;
        let mut files = true;
        let mut mimetypes = None;
        let mut extensions = None;
        let mut regex = None;
        let mut selection = Selection::Any;
        let mut priority = 0;
        let mut parse_output = false;

        for line in lines {
            if line.contains("**aw-fm-settings-end**") {
                let s = Self {
                    name,
                    directories,
                    files,
                    mimetypes,
                    extensions,
                    regex,
                    selection,
                    priority,
                    parse_output,
                };
                debug!("Read script from {path:?}: {s:#?}");
                return Some(s);
            }

            let Some(cap) = SETTINGS_RE.with(|re| re.captures(line)) else {
                continue;
            };

            let rest = cap[2].trim();
            if rest.is_empty() {
                continue;
            }
            match &cap[1] {
                "name" => name = Some(rest.into()),
                "directories" => {
                    directories = rest
                        .parse::<bool>()
                        .map_err(|_e| error!("Invalid settings block in {path:?}: got \"{line}\""))
                        .ok()?
                }
                "files" => {
                    files = rest
                        .parse::<bool>()
                        .map_err(|_e| error!("Invalid settings block in {path:?}: got \"{line}\""))
                        .ok()?
                }
                "mimetypes" => {
                    mimetypes =
                        Some(rest.trim_matches(';').split(';').map(str::to_string).collect())
                }
                "extensions" => extensions = Some(rest.split(';').map(str::to_string).collect()),
                "regex" => {
                    let re = Regex::new(rest)
                        .map_err(|e| error!("Invalid regex in {path:?}: {e}"))
                        .ok()?;
                    regex = Some(re);
                }
                "selection" => {
                    selection = Selection::from_str(rest)
                        .map_err(|_e| error!("Invalid settings block in {path:?}: got \"{line}\""))
                        .ok()?
                }
                "priority" => {
                    priority = rest
                        .parse::<i32>()
                        .map_err(|_e| error!("Invalid settings block in {path:?}: got \"{line}\""))
                        .ok()?;
                }
                "parse_output" => {
                    parse_output = rest
                        .parse::<bool>()
                        .map_err(|_e| error!("Invalid settings block in {path:?}: got \"{line}\""))
                        .ok()?
                }
                _ => {}
            }
        }

        error!("Found no end of settings line in {path:?}");
        None
    }

    fn for_action(name: String, selection: Selection) -> Self {
        let s = Self {
            name: Some(name),
            directories: true,
            files: true,
            mimetypes: None,
            extensions: None,
            regex: None,
            selection,
            priority: 0,
            // Not functional at all here
            parse_output: false,
        };
        debug!("Constructed filterable action from context menu entry: {s:#?}");
        s
    }

    // Whether we can safely skip per-entry checks.
    // Multiple is not checked here, that's trivial to check earlier
    const fn permissive(&self) -> bool {
        self.directories
            && self.files
            && self.mimetypes.is_none()
            && self.extensions.is_none()
            && self.regex.is_none()
    }

    const fn rejects_count(&self, count: usize) -> bool {
        match self.selection {
            Selection::Any => false,
            Selection::Zero => count != 0,
            Selection::MaybeOne => count > 1,
            Selection::One => count != 1,
            Selection::AtLeastOne => count == 0,
            Selection::Multiple => count < 2,
        }
    }

    fn rejects_extension(&self, file: &Path) -> bool {
        let Some(exts) = &self.extensions else {
            return false;
        };

        let Some(f_ext) = file.extension() else {
            return true;
        };

        !exts.iter().any(|e| OsStr::new(e) == f_ext)
    }

    fn rejects_mime(&self, mime: &str) -> bool {
        let Some(mimes) = &self.mimetypes else {
            return false;
        };

        !mimes.iter().any(|m| mime.starts_with(m))
    }

    fn rejects(&self, entry: &Entry) -> bool {
        let dir = entry.dir();
        if (!self.files && !dir) || (!self.directories && dir) {
            return true;
        }

        if !dir {
            let rejected_by_ext = self.rejects_extension(&entry.abs_path);
            let rejected_by_mime = self.rejects_mime(entry.mime);

            #[allow(clippy::nonminimal_bool)]
            if (rejected_by_ext && rejected_by_mime)
                || (rejected_by_ext && self.mimetypes.is_none())
                || (rejected_by_mime && self.extensions.is_none())
            {
                return true;
            }
        }

        if self.regex.as_ref().map(|r| r.is_match(entry.abs_path.as_os_str().as_bytes()))
            == Some(false)
        {
            return true;
        }
        false
    }

    // We only check this if there's no selection
    fn accepts_parent_dir(&self, path: &Path) -> bool {
        if !self.directories {
            false
        } else if let Some(regex) = &self.regex {
            regex.is_match(path.as_os_str().as_bytes())
        } else {
            true
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CustomAction {
    path: Option<Arc<Path>>,

    settings: ActionSettings,

    action: SimpleAction,
}

impl PartialOrd for CustomAction {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CustomAction {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.settings.cmp(&other.settings).then_with(|| self.path.cmp(&other.path))
    }
}

impl CustomAction {
    fn create_script(
        path: Arc<Path>,
        g: &Rc<Gui>,
        group: &SimpleActionGroup,
        n: usize,
    ) -> Option<Self> {
        if !path.exists() || !path.is_file() {
            error!("Failed to read custom action in {path:?}: not a regular file");
            return None;
        }

        let m = path
            .metadata()
            .map_err(|e| error!("Failed to read custom action in {path:?}: {e}"))
            .ok()?
            .permissions()
            .mode();

        if m & 0o111 == 0 {
            error!("Failed to read custom action in {path:?}: not executable");
            return None;
        }

        let read = File::open(&path)
            .map_err(|e| error!("Failed to read custom action in {path:?}: {e}"))
            .ok()?;

        let settings = ActionSettings::parse_script(&path, read)?;


        let action = SimpleAction::new(&format!("custom-{n}"), None);
        let g = g.clone();
        let p = path.clone();
        action.connect_activate(move |_a, _v| {
            if settings.parse_output {
                g.send_manager(ManagerAction::Script(p.clone(), g.get_env()))
            } else {
                g.send_manager(ManagerAction::Execute(p.clone(), g.get_env()))
            }
        });

        group.add_action(&action);

        Some(Self { path: Some(path), settings, action })
    }

    fn create_action(
        context: &ContextMenuEntry,
        g: &Rc<Gui>,
        group: &SimpleActionGroup,
        n: usize,
    ) -> Self {
        let settings = ActionSettings::for_action(context.name.clone(), context.selection);


        let action = SimpleAction::new(&format!("custom-context-{n}"), None);
        let g = g.clone();
        let cmd = context.action.clone();
        action.connect_activate(move |_a, _v| {
            g.run_command(&cmd);
        });

        group.add_action(&action);

        Self { path: None, settings, action }
    }

    fn display_name(&self) -> Cow<'_, str> {
        self.settings.name.as_ref().map_or_else(
            || {
                self.path.as_ref().map_or(Cow::Borrowed(""), |p| {
                    p.file_name().unwrap_or(p.as_os_str()).to_string_lossy()
                })
            },
            |s| Cow::Borrowed(&**s),
        )
    }

    fn menuitem(&self) -> MenuItem {
        let menuitem = MenuItem::new(Some(&self.display_name()), None);
        menuitem.set_action_and_target_value(
            Some(&format!("context-menu.{}", self.action.name())),
            None,
        );
        menuitem.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));

        menuitem
    }
}

#[derive(Debug)]
pub(super) struct GuiMenu {
    // Checkboxes

    // Radio buttons
    display: SimpleAction,
    sort_mode: SimpleAction,
    sort_dir: SimpleAction,

    // Special handling
    paste: SimpleAction,

    menu: PopoverMenu,
    custom: Vec<CustomAction>,
    custom_context: Vec<CustomAction>,
}

impl GuiMenu {
    pub(super) fn new(gui: &Rc<Gui>) -> Self {
        let display = GC::Display(().to_variant()).simple_action(gui);
        let sort_mode = GC::SortMode(().to_variant()).simple_action(gui);
        let sort_dir = GC::SortDir(().to_variant()).simple_action(gui);
        let paste = GC::Paste.simple_action(gui);


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
        action_group.add_action(&paste);

        gui.window.insert_action_group("context-menu", Some(&action_group));

        let custom = Self::parse_custom_actions(gui, &action_group);

        let (custom_context, menu) = Self::rebuild_menu(gui, &custom, &action_group);

        Self {
            display,
            sort_mode,
            sort_dir,
            paste,

            menu,
            custom,
            custom_context,
        }
    }

    fn parse_custom_actions(g: &Rc<Gui>, group: &SimpleActionGroup) -> Vec<CustomAction> {
        let iter = match ACTIONS_DIR.read_dir() {
            Ok(rd) => rd,
            Err(e) => {
                warn!("Failed to read custom actions directory: {e}");
                return Vec::new();
            }
        };

        let mut actions: Vec<_> = iter
            .filter_map(|r| match r {
                Ok(de) => {
                    let p = de.path();
                    if !p.is_dir() { Some(p) } else { None }
                }
                Err(e) => {
                    error!("Failed to read custom actions directory: {e}");
                    None
                }
            })
            .enumerate()
            .filter_map(|(n, f)| CustomAction::create_script(f.into(), g, group, n))
            .collect();

        actions.sort();
        actions
    }

    fn rebuild_menu(
        gui: &Rc<Gui>,
        actions: &[CustomAction],
        group: &SimpleActionGroup,
    ) -> (Vec<CustomAction>, PopoverMenu) {
        let mut custom = actions.iter().fuse().peekable();

        let menu = Menu::new();

        while let Some(peeked) = custom.peek() {
            if peeked.settings.priority >= 0 {
                break;
            }

            menu.append_item(&custom.next().unwrap().menuitem())
        }

        let mut submenus = AHashMap::new();
        let mut sections = AHashMap::new();
        let mut filterable_entries = Vec::new();

        for c_entry in &CONFIG.context_menu {
            let menuitem = if c_entry.selection != Selection::Any {
                let action =
                    CustomAction::create_action(c_entry, gui, group, filterable_entries.len());
                let menuitem = action.menuitem();

                filterable_entries.push(action);

                menuitem
            } else {
                let menuitem = MenuItem::new(Some(&c_entry.name), None);
                let cmd = GC::from(c_entry.action.trim_start());

                menuitem.set_action_and_target_value(
                    Some(&format!("context-menu.{}", cmd.action())),
                    cmd.variant(),
                );

                if cmd.hide_if_disabled() {
                    menuitem
                        .set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
                }

                menuitem
            };

            let menu = match &c_entry.group {
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

            menu.append_item(&menuitem);
        }

        for a in custom {
            menu.append_item(&a.menuitem());
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

        (filterable_entries, menu)
    }

    pub fn prepare(
        &self,
        g: &Gui,
        settings: DirSettings,
        entries: Vec<EntryObject>,
        dir: &Path,
    ) -> PopoverMenu {
        let start = Instant::now();
        self.display.change_state(&settings.display_mode.as_ref().to_variant());
        self.sort_mode.change_state(&settings.sort.mode.as_ref().to_variant());
        self.sort_dir.change_state(&settings.sort.direction.as_ref().to_variant());
        self.paste.set_enabled(clipboard::contains_mimetype(g.window.display()));

        for cme in &self.custom_context {
            if cme.settings.rejects_count(entries.len()) {
                trace!("Disabled context menu entry {} due to count", cme.display_name());
                cme.action.set_enabled(false);
            } else {
                cme.action.set_enabled(true);
            }
        }

        let mut custom: Vec<_> = self
            .custom
            .iter()
            .filter(|ca| {
                if ca.settings.rejects_count(entries.len()) {
                    trace!("Disabled custom action {} due to count", ca.display_name());
                    ca.action.set_enabled(false);
                    false
                } else if ca.settings.permissive() {
                    // No need to check these any further.
                    ca.action.set_enabled(true);
                    false
                } else {
                    true
                }
            })
            .collect();

        // We're running against the directory of a tab, not any selection.
        if entries.is_empty() {
            for ca in custom {
                if ca.settings.accepts_parent_dir(dir) {
                    ca.action.set_enabled(true);
                } else {
                    trace!("Disabled {} due to parent dir {dir:?}", ca.display_name());
                    ca.action.set_enabled(false)
                }
            }
            return self.menu.clone();
        }

        for eo in entries {
            if custom.is_empty() {
                break;
            }

            let entry = eo.get();

            // Swap remove is likely faster, but the bottleneck isn't going to be removing things
            // but iteration instead.
            custom.retain(|ca| {
                if ca.settings.rejects(&entry) {
                    trace!(
                        "Disabled {} due to {:?} {}",
                        ca.display_name(),
                        &*entry.name,
                        entry.mime
                    );
                    ca.action.set_enabled(false);
                    false
                } else {
                    true
                }
            });
        }

        for ca in custom {
            ca.action.set_enabled(true);
        }

        trace!("Finished filtering custom actions in {:?}", start.elapsed());
        self.menu.clone()
    }
}
