use std::convert::TryFrom;
use std::fmt;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use clap::Parser;
use dirs::config_dir;
use gtk::gdk;
use once_cell::sync::Lazy;
use serde::{de, Deserialize, Deserializer};
use strum_macros::{AsRefStr, EnumString};


#[derive(Debug, Parser)]
#[command(name = "aw-fm", about = "Awused's file manager.")]
pub struct Opt {
    #[arg(short, long, value_parser)]
    awconf: Option<PathBuf>,

    #[arg(value_parser)]
    pub file_name: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct Shortcut {
    pub action: String,
    pub key: String,
    pub modifiers: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Bookmark {
    pub action: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextMenuGroup {
    Section(String),
    Submenu(String),
}

#[derive(Debug, Deserialize)]
pub struct ContextMenuEntry {
    pub action: String,
    pub name: String,
    #[serde(default, flatten)]
    pub group: Option<ContextMenuGroup>,
    #[serde(default)]
    pub selection: Selection,
}

#[derive(Debug, Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum DirectoryCollision {
    #[default]
    Ask,
    Merge,
    Skip,
}

#[derive(Debug, Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum FileCollision {
    #[default]
    Ask,
    Overwrite,
    Rename,
    Newer,
    Skip,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, EnumString, AsRefStr, Deserialize)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    #[default]
    Any,
    Zero,
    MaybeOne,
    One,
    AtLeastOne,
    Multiple,
}

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub unique: bool,

    #[serde(default, deserialize_with = "empty_string_is_none")]
    pub background_colour: Option<gdk::RGBA>,

    #[serde(default, deserialize_with = "zero_is_none")]
    pub idle_timeout: Option<NonZeroU64>,

    #[serde(default)]
    pub shortcuts: Vec<Shortcut>,
    #[serde(default)]
    pub bookmarks: Vec<Bookmark>,
    #[serde(default)]
    pub context_menu: Vec<ContextMenuEntry>,

    #[serde(default)]
    pub skip_trash: bool,

    #[serde(default)]
    pub directory_collisions: DirectoryCollision,
    #[serde(default)]
    pub file_collisions: FileCollision,

    #[serde(default, deserialize_with = "empty_path_is_none")]
    pub actions_directory: Option<Arc<PathBuf>>,

    #[serde(default)]
    pub normalize_names: bool,

    #[serde(default, deserialize_with = "empty_path_is_none")]
    pub database: Option<PathBuf>,

    #[serde(default)]
    pub search_max_depth: Option<u8>,
    #[serde(default)]
    pub search_show_all: bool,
    #[serde(default)]
    pub paste_into_search: bool,

    #[serde(default)]
    pub max_thumbnailers: u8,
    #[serde(default)]
    pub background_thumbnailers: u8,
}

// Serde seems broken with OsString for some reason
fn empty_path_is_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: From<PathBuf>,
{
    let s = PathBuf::deserialize(deserializer)?;
    if s.as_os_str().is_empty() { Ok(None) } else { Ok(Some(s.into())) }
}

fn empty_string_is_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    <T as FromStr>::Err: fmt::Debug,
{
    let s = <String>::deserialize(deserializer)?;
    if s.is_empty() {
        Ok(None)
    } else {
        match FromStr::from_str(&s) {
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(de::Error::custom(format!("{e:?}"))),
        }
    }
}

fn zero_is_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64>,
    <T as TryFrom<u64>>::Error: fmt::Display,
{
    let u = u64::deserialize(deserializer)?;
    if u == 0 {
        Ok(None)
    } else {
        match T::try_from(u) {
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(de::Error::custom(format!("{e}"))),
        }
    }
}

pub static OPTIONS: Lazy<Opt> = Lazy::new(Opt::parse);
pub static ACTIONS_DIR: Lazy<Arc<PathBuf>> = Lazy::new(|| {
    CONFIG.actions_directory.clone().unwrap_or_else(|| {
        config_dir()
            .unwrap_or_else(|| {
                panic!("Could not read default config directory, set actions_directory manually.")
            })
            .join("aw-fm")
            .join("actions")
            .into()
    })
});

static DEFAULT_CONFIG: &str = include_str!("../aw-fm.toml.sample");

pub static CONFIG: Lazy<Config> = Lazy::new(|| {
    match awconf::load_config::<Config>("aw-fm", OPTIONS.awconf.as_ref(), Some(DEFAULT_CONFIG)) {
        Ok((conf, Some(path))) => {
            info!("Loaded config from {path:?}");
            conf
        }
        Ok((conf, None)) => {
            info!("Loaded default config");
            conf
        }
        Err(e) => {
            error!("Error loading config: {e}");
            panic!("Error loading config: {e}");
        }
    }
});


pub fn init() {
    Lazy::force(&OPTIONS);
    Lazy::force(&CONFIG);
    Lazy::force(&ACTIONS_DIR);
}
