use std::cmp::Ordering;

use gtk::SortType;
use gtk::glib::Object;
use gtk::prelude::Cast;
use strum_macros::{AsRefStr, EnumString};

use super::EntryObject;

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy, EnumString, AsRefStr)]
#[strum(serialize_all = "lowercase")]
pub enum DisplayMode {
    #[default]
    Icons,
    Columns,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy, EnumString, AsRefStr)]
#[strum(serialize_all = "lowercase")]
pub enum SortMode {
    #[default]
    Name,
    MTime,
    Size,
}

#[derive(Debug, PartialEq, Eq, Default, Clone, Copy, EnumString, AsRefStr)]
#[strum(serialize_all = "lowercase")]
pub enum SortDir {
    #[default]
    Ascending,
    Descending,
}

impl From<SortDir> for SortType {
    fn from(value: SortDir) -> Self {
        match value {
            SortDir::Ascending => Self::Ascending,
            SortDir::Descending => Self::Descending,
        }
    }
}

impl From<SortType> for SortDir {
    fn from(value: SortType) -> Self {
        match value {
            SortType::Ascending => Self::Ascending,
            SortType::Descending => Self::Descending,
            _ => unreachable!(),
        }
    }
}

// #[derive(Debug, Default, Clone, Copy, EnumString, AsRefStr)]
// #[strum(serialize_all = "lowercase")]
// pub enum DisplayHidden {
//     // Default, -- would be the global setting, if/when we have one
//     False,
//     #[default]
//     True,
// }

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct SortSettings {
    pub mode: SortMode,
    pub direction: SortDir,
}

impl SortSettings {
    pub fn comparator(self) -> impl Fn(&Object, &Object) -> Ordering + 'static {
        move |a, b| {
            let a = a.downcast_ref::<EntryObject>().unwrap();
            let b = b.downcast_ref::<EntryObject>().unwrap();
            a.cmp(b, self)
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct DirSettings {
    pub display_mode: DisplayMode,
    pub sort: SortSettings,
    // pub display_hidden: DisplayHidden,
}

impl DirSettings {
    pub fn allow_stale(self, old: Self) -> bool {
        self.display_mode == old.display_mode
    }
}
