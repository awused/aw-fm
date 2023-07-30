use std::path::Path;
use std::str::{from_utf8, FromStr};
use std::sync::Arc;

use gtk::gdk::ffi::{
    gdk_content_deserializer_return_success, gdk_content_deserializer_set_task_data,
    GdkContentDeserializer,
};
use gtk::gdk::Display;
use gtk::gio::{Cancellable, MemoryOutputStream, OutputStreamSpliceFlags};
use gtk::glib::error::ErrorDomain;
use gtk::glib::translate::{IntoGlibPtr, ToGlibPtr};
use gtk::prelude::{
    CastNone, DisplayExt, InputStreamExt, ListModelExt, ObjectExt, OutputStreamExt, StaticType,
    StaticTypeExt,
};
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::SelectionModelExt;
use gtk::{gdk, glib, MultiSelection};
use strum_macros::{EnumString, IntoStaticStr};

use super::id::TabId;
use crate::com::EntryObject;
use crate::gui::gui_run;

const SPECIAL: &str = "x-special/aw-fm-copied-files";
const SPECIAL_MATE: &str = "x-special/mate-copied-files";
const SPECIAL_GNOME: &str = "x-special/gnome-copied-files";

glib::wrapper! {
    pub struct ClipboardProvider(ObjectSubclass<imp::ClipboardProvider>)
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

impl ClipboardProvider {
    // It's fine if the selection is empty.
    pub fn new(operation: Operation, selection: &MultiSelection) -> Self {
        let s: Self = glib::Object::new();
        let selected = selection.selection();

        let mut files = Vec::with_capacity(selected.size() as usize);
        for i in 0..selected.size() as u32 {
            let file = selection.item(selected.nth(i)).and_downcast::<EntryObject>().unwrap();
            files.push(file);
        }

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

glib::wrapper! {
    pub struct SpecialClipboardReader(ObjectSubclass<imp::ClipboardReader>);
}

impl Default for SpecialClipboardReader {
    fn default() -> Self {
        glib::Object::new()
    }
}

pub fn register_types() {
    SpecialClipboardReader::ensure_type();

    gdk::content_register_deserializer(
        SPECIAL,
        SpecialClipboardReader::static_type(),
        |deserializer, user_data: &mut Option<SpecialClipboardReader>| {
            println!("{deserializer:?}");
            let reader = SpecialClipboardReader::default();

            deserializer.set_value(reader.into());

            deserializer.return_success();
        },
    );

    gdk::content_register_deserializer(
        SPECIAL_MATE,
        SpecialClipboardReader::static_type(),
        |d, out: &mut Option<SpecialClipboardReader>| {
            // let text = d.input_stream().close(Cancellable::NONE);
            // println!("{text}");
            let outstream = MemoryOutputStream::new_resizable();
            let stream = outstream.clone();
            let d = d.clone();

            let reader = SpecialClipboardReader::default();
            d.set_value(reader.clone().into());
            let a: *mut GdkContentDeserializer = d.to_glib_none().0;

            unsafe {
                gdk_content_deserializer_return_success(a);
            }


            outstream.splice_async(
                &d.input_stream(),
                OutputStreamSpliceFlags::CLOSE_SOURCE,
                d.priority(),
                Cancellable::NONE,
                move |output| {
                    stream.close(Cancellable::NONE).unwrap();
                    // let data = stream.data(key)
                    // println!("{:?}", d.value());
                    println!("{:?}", reader.imp().operation.get());
                    // d.set_value(reader.into());
                    // let a: *mut GdkContentDeserializer = d.to_glib_none().0;
                    //
                    // unsafe {
                    //     gdk_content_deserializer_return_success(a);
                    // }
                    // d.set_value(reader.into());
                    // println!("{:?}", d.value());
                    // println!("{}", out.is_some());
                    // d.set_value(reader.into());
                    println!("output {output:?}");
                },
            );
        },
    );
}

pub fn read_clipboard(display: Display, tab: TabId, path: Arc<Path>) {
    let formats = display.clipboard().formats();

    if formats.contains_type(SpecialClipboardReader::static_type()) {
        println!("Reading");
        display.clipboard().read_value_async(
            SpecialClipboardReader::static_type(),
            glib::Priority::DEFAULT,
            Cancellable::NONE,
            |text| {
                println!("{text:?}");
                let text = match text {
                    Ok(text) => text,
                    Err(e) => {
                        let msg = format!("Error reading clipboard: {e}");
                        error!("{msg}");
                        return gui_run(|g| g.error(&msg));
                    }
                };

                let Ok(text) = text.get::<SpecialClipboardReader>() else {
                    return info!("Got paste but clipboard was empty");
                };

                error!(
                    "TODO remove got clipboard -- {:?}, {:?}",
                    text.imp().operation.get(),
                    text.imp().entries.get()
                );
            },
        );
    }
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

    use super::{Operation, SPECIAL, SPECIAL_GNOME, SPECIAL_MATE};
    use crate::com::EntryObject;


    // TODO -- application/vnd.portal.filetransfer, if it ever comes up
    const URIS: &str = "text/uri-list";
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
        type Type = super::ClipboardProvider;

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
            output.push('\0'); // Probably unnecessary
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
            output.push("\0"); // Probably unnecessary
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
            output.push('\0'); // Probably unnecessary
            stream
                .write_bytes_future(&glib::Bytes::from_owned(output.into_bytes()), priority)
                .await
                .map(|_| ())
        }
    }


    #[derive(Default)]
    pub struct ClipboardReader {
        pub operation: OnceCell<Operation>,
        pub entries: OnceCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ClipboardReader {
        type Type = super::SpecialClipboardReader;

        const NAME: &'static str = "ClipboardReader";
    }

    impl ObjectImpl for ClipboardReader {}
}