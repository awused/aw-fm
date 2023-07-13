use std::borrow::BorrowMut;
use std::cmp::Ordering;
use std::fmt::{self, Formatter};
use std::ops::Deref;
use std::path::PathBuf;

use gtk::gio::ffi::G_FILE_TYPE_DIRECTORY;
use gtk::gio::{
    self, Cancellable, FileInfo, FileQueryInfoFlags, Icon,
    FILE_ATTRIBUTE_STANDARD_FAST_CONTENT_TYPE, FILE_ATTRIBUTE_STANDARD_ICON,
    FILE_ATTRIBUTE_STANDARD_IS_SYMLINK, FILE_ATTRIBUTE_STANDARD_SIZE,
    FILE_ATTRIBUTE_STANDARD_SYMBOLIC_ICON, FILE_ATTRIBUTE_STANDARD_TYPE,
    FILE_ATTRIBUTE_TIME_CREATED, FILE_ATTRIBUTE_TIME_CREATED_USEC, FILE_ATTRIBUTE_TIME_MODIFIED,
    FILE_ATTRIBUTE_TIME_MODIFIED_USEC,
};
use gtk::glib::subclass::Signal;
use gtk::glib::{self, GStr, Object, Variant};
use gtk::prelude::{FileExt, IconExt, ObjectExt};
use gtk::subclass::prelude::{ObjectImpl, ObjectSubclass, ObjectSubclassIsExt};
use once_cell::sync::Lazy;

use super::{DirSettings, SortDir, SortMode, SortSettings};
use crate::natsort::{self, ParsedString};

// In theory could use standard::edit-name and standard::display-name instead of taking
// those from the path. In practice none of my own files are that broken.
static ATTRIBUTES: Lazy<String> = Lazy::new(|| {
    [
        FILE_ATTRIBUTE_STANDARD_TYPE,
        FILE_ATTRIBUTE_STANDARD_FAST_CONTENT_TYPE,
        FILE_ATTRIBUTE_STANDARD_ICON,
        FILE_ATTRIBUTE_STANDARD_IS_SYMLINK,
        FILE_ATTRIBUTE_STANDARD_SIZE,
        FILE_ATTRIBUTE_STANDARD_SYMBOLIC_ICON,
        FILE_ATTRIBUTE_TIME_MODIFIED,
        FILE_ATTRIBUTE_TIME_MODIFIED_USEC,
        FILE_ATTRIBUTE_TIME_CREATED,
        FILE_ATTRIBUTE_TIME_CREATED_USEC,
    ]
    .map(GStr::as_str)
    .join(",")
});

#[derive(Eq, PartialEq, Default, Clone, Copy)]
pub struct FileTime {
    pub sec: u64,
    pub usec: u32,
}

impl Ord for FileTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sec.cmp(&other.sec).then(self.usec.cmp(&other.usec))
    }
}

impl PartialOrd for FileTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for FileTime {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "time: {}, {}", self.sec, self.usec)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File {
        size: u64,
    },
    Directory {
        contents: u64,
    },
    #[default]
    Uninitialized, // Broken {}
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    // This is an absolute but NOT canonicalized path.
    pub abs_path: PathBuf,
    // It's kind of expensive to do this but necessary as an mtime/ctime tiebreaker anyway.
    // TODO -- is this really name, or should it be rel_path
    pub name: ParsedString,
    pub mtime: FileTime,
    pub btime: FileTime,

    // TODO -- Arc<> or some other mechanism for interning them, otherwise this is a large
    // number of wasted tiny allocations.
    // Could do String::into_boxed_str()::leak() to get &'static str
    pub mime: String,
    // pub info: String,
    pub icon: Variant,
}


impl Entry {
    pub fn cmp(&self, other: &Self, settings: SortSettings) -> Ordering {
        use EntryKind::*;

        let size_order = match (&self.kind, &other.kind) {
            (Uninitialized, _) | (_, Uninitialized) => unreachable!(),
            (File { .. }, Directory { .. }) => return Ordering::Greater,
            (Directory { .. }, File { .. }) => return Ordering::Less,
            (File { size: self_size }, File { size: other_size })
            | (Directory { contents: self_size }, Directory { contents: other_size }) => {
                self_size.cmp(other_size)
            }
        };

        let m_order = self.mtime.cmp(&other.mtime);
        let b_order = self.btime.cmp(&other.btime);
        let name_order = || self.name.cmp(&other.name);

        // Use the other options as tie breakers, with the abs_path as a final tiebreaker.
        let ordering = match settings.mode {
            SortMode::Name => name_order().then(m_order).then(b_order).then(size_order),
            SortMode::MTime => m_order.then(b_order).then_with(name_order).then(size_order),
            SortMode::Size => size_order.then_with(name_order).then(m_order).then(b_order),
            SortMode::BTime => b_order.then(m_order).then_with(name_order).then(size_order),
        }
        .then_with(|| self.abs_path.cmp(&other.abs_path));

        if settings.direction == SortDir::Ascending {
            ordering
        } else {
            ordering.reverse()
        }
    }

    pub fn new(abs_path: PathBuf) -> Result<Self, gtk::glib::Error> {
        let name = abs_path.file_name().unwrap_or(abs_path.as_os_str());
        let name = natsort::key(name);

        let info = gio::File::for_path(&abs_path).query_info(
            ATTRIBUTES.as_str(),
            FileQueryInfoFlags::empty(),
            Option::<&Cancellable>::None,
        )?;

        let mtime = FileTime {
            sec: info.attribute_uint64(FILE_ATTRIBUTE_TIME_MODIFIED),
            usec: info.attribute_uint32(FILE_ATTRIBUTE_TIME_MODIFIED_USEC),
        };

        // Doesn't work over NFS, could fall back to "changed" time but that's not what we really
        // want.
        let btime = FileTime {
            sec: info.attribute_uint64(FILE_ATTRIBUTE_TIME_CREATED),
            usec: info.attribute_uint32(FILE_ATTRIBUTE_TIME_CREATED_USEC),
        };

        let mime = info
            .attribute_string(FILE_ATTRIBUTE_STANDARD_FAST_CONTENT_TYPE)
            .unwrap()
            .to_string();

        let size = info.attribute_uint64(FILE_ATTRIBUTE_STANDARD_SIZE);

        let file_type = info.attribute_uint32(FILE_ATTRIBUTE_STANDARD_TYPE);
        let kind = if file_type == G_FILE_TYPE_DIRECTORY as u32 {
            EntryKind::Directory { contents: size - 1 }
        } else {
            EntryKind::File { size }
        };


        let icon = info.icon().unwrap().serialize().unwrap();

        Ok(Self {
            kind,
            abs_path,
            name,
            mtime,
            btime,
            mime,
            icon,
        })
    }

    // This could be a compact string/small string but possibly not worth the dependency on its own
    pub fn size_string(&self) -> String {
        match self.kind {
            EntryKind::File { size } => humansize::format_size(size, humansize::WINDOWS),
            EntryKind::Directory { contents } => format!("{contents}"),
            EntryKind::Uninitialized => unreachable!(),
        }
    }

    pub fn long_size_string(&self) -> String {
        match self.kind {
            EntryKind::File { size } => humansize::format_size(size, humansize::WINDOWS),
            EntryKind::Directory { contents } => format!("{contents} items"),
            EntryKind::Uninitialized => unreachable!(),
        }
    }
}


// All the ugly GTK wrapper code below this

mod internal {
    use std::cell::{OnceCell, Ref, RefCell};
    use std::ops::Deref;

    use gtk::gio::Icon;
    use gtk::glib;
    use gtk::glib::subclass::Signal;
    use gtk::prelude::ObjectExt;
    use gtk::subclass::prelude::{ObjectImpl, ObjectSubclass, ObjectSubclassExt};
    use once_cell::sync::Lazy;

    #[derive(Debug)]
    struct Entry {
        entry: super::Entry,
        // icon: Icon,
        // thumbnail: Option<>
    }

    #[derive(Debug, Default)]
    pub struct Wrapper(RefCell<Option<Entry>>);


    #[glib::object_subclass]
    impl ObjectSubclass for Wrapper {
        type Type = super::EntryObject;

        const NAME: &'static str = "awfmEntry";
    }

    impl ObjectImpl for Wrapper {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: Lazy<Vec<Signal>> =
                Lazy::new(|| vec![Signal::builder("update").build()]);
            SIGNALS.as_ref()
        }
    }

    impl Wrapper {
        pub(super) fn init(&self, entry: super::Entry) {
            // let icon = Icon::deserialize(&entry.icon).unwrap();

            // let wrapped = Entry { entry, icon };
            let wrapped = Entry { entry };
            assert!(self.0.replace(Some(wrapped)).is_none());
        }

        pub(super) fn update_inner(&self, entry: super::Entry) {
            let wrapped = Entry { entry };
            self.0.replace(Some(wrapped)).unwrap();
            self.obj().emit_by_name::<()>("update", &[]);
        }

        pub fn get(&self) -> Ref<super::Entry> {
            let b = self.0.borrow();
            Ref::map(b, |o| &o.as_ref().unwrap().entry)
        }

        pub fn icon(&self) -> Icon {
            Icon::deserialize(&self.get().icon).unwrap()
        }
    }
}

glib::wrapper! {
    pub struct EntryObject(ObjectSubclass<internal::Wrapper>);
}

impl Deref for EntryObject {
    type Target = internal::Wrapper;

    fn deref(&self) -> &Self::Target {
        self.imp()
    }
}

impl EntryObject {
    pub fn new(entry: Entry) -> Self {
        let obj: Self = Object::new();
        obj.imp().init(entry);

        obj
    }

    // Returns true if resorting is required
    pub fn update(&self, entry: Entry, settings: SortSettings) -> bool {
        if *self.imp().get() == entry {
            // No change we care about
            trace!("Update for {:?} was unimportant", entry.abs_path);
            return false;
        }

        let needs_resort = self.imp().get().cmp(&entry, settings).is_ne();

        self.update_inner(entry);
        // self.notify(pspec);
        // self.dispatch_properties_changed(pspecs)
        // let old = borrow.0.take();

        // if
        needs_resort
    }

    pub(super) fn cmp(&self, other: &Self, settings: SortSettings) -> Ordering {
        self.imp().get().cmp(&other.imp().get(), settings)
    }
}
