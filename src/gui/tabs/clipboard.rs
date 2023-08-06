use std::path::Path;
use std::str::{from_utf8, FromStr};
use std::sync::Arc;
use std::thread;

use gtk::gdk::{Display, DragAction};
use gtk::gio::{Cancellable, InputStream, MemoryOutputStream, OutputStreamSpliceFlags};
use gtk::glib::{GString, Priority};
use gtk::prelude::{DisplayExt, FileExt, MemoryOutputStreamExt, OutputStreamExt};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{gdk, gio, glib, MultiSelection};
use strum_macros::{EnumString, IntoStaticStr};

use super::id::TabId;
use crate::gui::{file_operations, gui_run, Selected};

pub const SPECIAL: &str = "x-special/aw-fm-copied-files";
pub const SPECIAL_MATE: &str = "x-special/mate-copied-files";
pub const SPECIAL_GNOME: &str = "x-special/gnome-copied-files";
pub const URIS: &str = "text/uri-list";

glib::wrapper! {
    pub struct SelectionProvider(ObjectSubclass<imp::ClipboardProvider>)
        @extends gdk::ContentProvider;
}


#[derive(Debug, PartialEq, Eq, Clone, Copy, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum Operation {
    Copy,
    Cut,
}

impl Operation {
    const fn verb(self) -> &'static str {
        match self {
            Self::Copy => "copied",
            Self::Cut => "moved",
        }
    }
}

impl SelectionProvider {
    // It's fine if the selection is empty.
    pub fn new(operation: Operation, selection: &MultiSelection) -> Self {
        let s: Self = glib::Object::new();

        let files: Vec<_> = Selected::from(selection).collect();

        s.imp().operation.set(operation).unwrap();
        s.imp().entries.set(files.into()).unwrap();

        s
    }

    pub fn display_string(&self) -> String {
        let verb = self.imp().operation.get().unwrap().verb();
        let files = self.imp().entries.get().unwrap();

        if files.is_empty() {
            format!("Selection was empty, nothing will be {verb}")
        } else if files.len() == 1 {
            format!("\"{}\" will be {verb}", files[0].get().name.to_string_lossy())
        } else {
            format!("{} items will be {verb}", files.len())
        }
    }
}

fn bytes_to_operation(tab: TabId, path: Arc<Path>, uri_list: bool, bytes: &[u8]) {
    let Ok(text) = from_utf8(bytes) else {
        return error!("Invalid utf-8 in contents");
    };

    let mut lines = text.lines();
    let operation = if !uri_list {
        let Some(first) = lines.next() else {
            error!("Empty contents");
            return;
        };

        match Operation::from_str(first) {
            Ok(o) => o,
            Err(e) => {
                error!("Got invalid operation from contents: {e}");
                return;
            }
        }
    } else {
        Operation::Cut
    };

    // for_uri can panic
    let files: thread::Result<Option<Vec<_>>> =
        std::panic::catch_unwind(|| lines.map(|s| gio::File::for_uri(s).path()).collect());
    let Ok(Some(files)) = files else {
        return error!("Got URI for file without a local path. Aborting paste.");
    };

    glib::idle_add_once(move || {
        let kind = match operation {
            Operation::Copy => file_operations::Kind::Copy(path),
            Operation::Cut => file_operations::Kind::Move(path),
        };

        gui_run(|g| g.start_operation(tab, kind, files));
    });
}

// Takes an InputStream from a clipboard read or drag and drop operation and
fn stream_to_operation(
    tab: TabId,
    path: Arc<Path>,
    uri_list: bool,
    finished: impl FnOnce() + 'static,
) -> impl FnOnce(Result<(InputStream, GString), glib::Error>) {
    move |res| {
        let in_stream = match res {
            Ok((is, _)) => is,
            Err(e) => {
                error!("Failed to read contents: {e}");
                finished();
                return;
            }
        };

        let output = MemoryOutputStream::new_resizable();
        output.clone().splice_async(
            &in_stream,
            OutputStreamSpliceFlags::CLOSE_SOURCE | OutputStreamSpliceFlags::CLOSE_TARGET,
            Priority::LOW,
            Cancellable::NONE,
            move |res| {
                match res {
                    Ok(_bytes) => {
                        let bytes = output.steal_as_bytes();
                        bytes_to_operation(tab, path, uri_list, &bytes)
                    }
                    Err(e) => {
                        error!("Failed to read contents: {e}");
                    }
                }
                finished();
            },
        )
    }
}

pub fn handle_clipboard(display: Display, tab: TabId, path: Arc<Path>) {
    let formats = display.clipboard().formats();

    let mime = if formats.contain_mime_type(SPECIAL) {
        SPECIAL
    } else if formats.contain_mime_type(SPECIAL_MATE) {
        SPECIAL_MATE
    } else if formats.contain_mime_type(SPECIAL_GNOME) {
        SPECIAL_GNOME
    } else {
        warn!("Paste with no recognized mimetype. Got {:?}", formats.mime_types());
        return;
    };


    display.clipboard().read_async(
        &[mime],
        Priority::LOW,
        Cancellable::NONE,
        stream_to_operation(tab, path, false, || {}),
    );
}

pub fn handle_drop(drop_ev: &gdk::Drop, tab: TabId, path: Arc<Path>) -> bool {
    let formats = drop_ev.formats();

    let (mime, uris) = if formats.contain_mime_type(SPECIAL) {
        (SPECIAL, false)
    } else if formats.contain_mime_type(SPECIAL_MATE) {
        (SPECIAL_MATE, false)
    } else if formats.contain_mime_type(SPECIAL_GNOME) {
        (SPECIAL_GNOME, false)
    } else if formats.contain_mime_type(URIS) {
        (URIS, true)
    } else {
        warn!("Paste with no recognized mimetype. Got {:?}", formats.mime_types());
        unreachable!();
    };


    let actions = drop_ev.actions();
    let action = if actions.contains(DragAction::MOVE) {
        DragAction::MOVE
    } else if actions.contains(DragAction::COPY) {
        DragAction::COPY
    } else {
        actions
    };
    println!("{actions}");

    let dr = drop_ev.clone();
    drop_ev.read_async(
        &[mime],
        Priority::LOW,
        Cancellable::NONE,
        stream_to_operation(tab, path, uris, move || dr.finish(action)),
    );
    true
}


mod imp {
    use std::ffi::OsString;
    use std::future::Future;
    use std::os::unix::prelude::OsStringExt;
    use std::pin::Pin;
    use std::rc::Rc;

    use gtk::prelude::{FileExt, OutputStreamExt};
    use gtk::subclass::prelude::*;
    use gtk::{gdk, gio, glib};
    use once_cell::unsync::OnceCell;

    use super::{Operation, SPECIAL, SPECIAL_GNOME, SPECIAL_MATE, URIS};
    use crate::com::EntryObject;


    // TODO -- application/vnd.portal.filetransfer, if it ever comes up
    const UTF8: &str = "text/plain;charset=utf-8";
    const PLAIN: &str = "text/plain";

    #[derive(Default)]
    pub struct ClipboardProvider {
        pub operation: OnceCell<Operation>,
        // This needs to outlive the tab it came from, hence the copy.
        pub entries: OnceCell<Rc<[EntryObject]>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ClipboardProvider {
        type ParentType = gdk::ContentProvider;
        type Type = super::SelectionProvider;

        const NAME: &'static str = "ClipboardProvider";
    }

    impl ObjectImpl for ClipboardProvider {}

    impl ContentProviderImpl for ClipboardProvider {
        fn formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormatsBuilder::new()
                .add_mime_type(SPECIAL)
                .add_mime_type(SPECIAL_MATE)
                .add_mime_type(SPECIAL_GNOME)
                .add_mime_type(URIS)
                .add_mime_type(UTF8)
                .add_mime_type(PLAIN)
                .build()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let stream = stream.clone();
            let mime_type = mime_type.to_string();
            let entries = self.entries.get().unwrap().clone();
            let operation = *self.operation.get().unwrap();
            Box::pin(async move {
                match &*mime_type {
                    SPECIAL | SPECIAL_MATE | SPECIAL_GNOME => {
                        stream
                            .write_bytes_future(
                                &glib::Bytes::from_static(
                                    <&'static str>::from(operation).as_bytes(),
                                ),
                                priority,
                            )
                            .await
                            .map(|_| ())?;

                        stream
                            .write_bytes_future(&glib::Bytes::from_static(b"\n"), priority)
                            .await
                            .map(|_| ())?;

                        Self::write_uris(&stream, priority, &entries).await
                    }
                    URIS => Self::write_uris(&stream, priority, &entries).await,
                    UTF8 => Self::write_paths(&stream, priority, &entries).await,
                    PLAIN => Self::write_ascii_paths(&stream, priority, &entries).await,
                    _ => {
                        Err(glib::Error::new(gio::IOErrorEnum::InvalidData, "Unhandled mime type"))
                    }
                }
            })
        }
    }

    impl ClipboardProvider {
        async fn write_uris(
            stream: &gio::OutputStream,
            priority: glib::Priority,
            entries: &[EntryObject],
        ) -> Result<(), glib::Error> {
            let mut output = String::new();
            let mut iter = entries.iter();
            if let Some(first) = iter.next() {
                output += &gio::File::for_path(&first.get().abs_path).uri();

                for f in iter {
                    output.push('\n');
                    output += &gio::File::for_path(&f.get().abs_path).uri();
                }
            }
            stream
                .write_bytes_future(&glib::Bytes::from_owned(output.into_bytes()), priority)
                .await
                .map(|_| ())
        }

        async fn write_paths(
            stream: &gio::OutputStream,
            priority: glib::Priority,
            entries: &[EntryObject],
        ) -> Result<(), glib::Error> {
            let mut output = OsString::new();
            let mut iter = entries.iter();
            if let Some(first) = iter.next() {
                output.push(first.get().abs_path.as_os_str());

                for f in iter {
                    output.push("\n");
                    output.push(f.get().abs_path.as_os_str());
                }
            }
            stream
                .write_bytes_future(&glib::Bytes::from_owned(output.into_vec()), priority)
                .await
                .map(|_| ())
        }

        // Doesn't match C-style escape sequences, but nothing should really use this
        async fn write_ascii_paths(
            stream: &gio::OutputStream,
            priority: glib::Priority,
            entries: &[EntryObject],
        ) -> Result<(), glib::Error> {
            let mut output = String::new();
            let mut iter = entries.iter();
            if let Some(first) = iter.next() {
                output.extend(first.get().abs_path.to_string_lossy().escape_default());

                for f in iter {
                    output.push('\n');
                    output.extend(f.get().abs_path.to_string_lossy().escape_default());
                }
            }
            stream
                .write_bytes_future(&glib::Bytes::from_owned(output.into_bytes()), priority)
                .await
                .map(|_| ())
        }
    }
}
