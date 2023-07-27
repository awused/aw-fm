use std::convert::TryFrom;
use std::fmt;
use std::num::{NonZeroU64, NonZeroU8};
use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use gtk::gdk;
use once_cell::sync::Lazy;
use serde::{de, Deserialize, Deserializer};


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
}

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default, deserialize_with = "empty_path_is_none")]
    pub temp_directory: Option<PathBuf>,

    #[serde(default)]
    pub unique: bool,

    #[serde(default, deserialize_with = "empty_string_is_none")]
    pub background_colour: Option<gdk::RGBA>,

    #[serde(default, deserialize_with = "zero_is_none")]
    pub idle_timeout: Option<NonZeroU64>,

    #[serde(default, deserialize_with = "empty_path_is_none")]
    pub action_directory: Option<PathBuf>,

    #[serde(default, deserialize_with = "empty_path_is_none")]
    pub database: Option<PathBuf>,

    #[serde(default)]
    pub shortcuts: Vec<Shortcut>,

    #[serde(default)]
    pub skip_trash: bool,
    #[serde(default)]
    pub context_menu: Vec<ContextMenuEntry>,

    #[serde(default)]
    pub max_thumbnailers: u8,
    #[serde(default)]
    pub background_thumbnailers: u8,

    #[serde(default, deserialize_with = "zero_is_none")]
    pub search_max_depth: Option<NonZeroU64>,
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

static DEFAULT_CONFIG: &str = include_str!("../aw-fm.toml.sample");

pub static CONFIG: Lazy<Config> =
    Lazy::new(|| match awconf::load_config::<Config>("aw-fm", &OPTIONS.awconf) {
        Ok(conf) => conf,
        Err(awconf::Error::Deserialization(e)) => {
            if let Some(path) = &OPTIONS.awconf {
                if !path.is_file() && !path.is_dir() {
                    // It's not a regular file or a directory, use the default config.
                    warn!("Error loading config file, using default instead: {e:#?}");
                    return toml::from_str(DEFAULT_CONFIG).unwrap();
                }
            }
            error!("Error parsing config: {e}");
            panic!("Error parsing config: {e}");
        }
        Err(awconf::Error::Utf8Error(e)) => {
            error!("Error parsing config: {e}");
            panic!("Error parsing config: {e}");
        }
        Err(e) => {
            warn!("Error loading config file, using default instead: {e:#?}");
            toml::from_str(DEFAULT_CONFIG).unwrap()
        }
    });


pub fn init() {
    Lazy::force(&OPTIONS);
    Lazy::force(&CONFIG);
}
