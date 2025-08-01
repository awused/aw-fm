use std::rc::Rc;

use gtk::gio::{BusNameOwnerFlags, DBusError, DBusNodeInfo, bus_own_name, bus_unown_name};
use gtk::glib::{self, VariantStrIter, filename_from_uri};

use super::Gui;
use crate::config::CONFIG;
use crate::gui::tabs::NavTarget;
use crate::gui::tabs::list::TabPosition;

thread_local! {
    static DBUS_INFO: DBusNodeInfo =
        DBusNodeInfo::for_xml(include_str!("FileManager1.xml")).unwrap();
}

impl Gui {
    pub(super) fn dbus_register(self: &Rc<Self>) {
        if CONFIG.disable_filemanager_dbus {
            return;
        }

        let dbus_info = DBusNodeInfo::for_xml(include_str!("FileManager1.xml")).unwrap();

        let g = self.clone();
        let owner = bus_own_name(
            gtk::gio::BusType::Session,
            "org.freedesktop.FileManager1",
            // None -> queues if more than one is running
            BusNameOwnerFlags::NONE,
            move |con, s| {
                debug!("dbus: bus acquired {s:?}");

                let g = g.clone();
                con.register_object("/org/freedesktop/FileManager1", &dbus_info.interfaces()[0])
                    .method_call(
                        move |_con,
                              _sender,
                              _object_path,
                              _interface_name,
                              method_name,
                              parameters,
                              invocation| {
                            debug!("Got dbus method call: {method_name} args: {parameters:?}");

                            // All of the methods take the same arguments
                            if let Some(list) = parameters.try_child_value(0)
                                && let Ok(list) = list.array_iter_str()
                            {
                                invocation.return_result(g.handle_dbus_method(method_name, list));
                            } else {
                                invocation.return_gerror(glib::Error::new(
                                    DBusError::InvalidArgs,
                                    "Couldn't read first argument",
                                ));
                            }
                        },
                    )
                    .build()
                    .unwrap();
            },
            |_con, s| debug!("dbus: name acquired {s}"),
            |_con, _s| {},
        );

        self.dbus_owner.set(Some(owner));
    }

    pub(super) fn dbus_unregister(&self) {
        if let Some(owner) = self.dbus_owner.take() {
            bus_unown_name(owner);
        }
    }

    fn handle_dbus_method(
        &self,
        name: &str,
        uris: VariantStrIter,
    ) -> Result<Option<glib::Variant>, glib::Error> {
        let mut tabs = self.tabs.borrow_mut();

        match name {
            "ShowItems" => {
                for uri in uris {
                    match filename_from_uri(uri) {
                        Ok((path, _)) => {
                            let target = if let Some(parent) = path.parent() {
                                NavTarget::assume_jump(parent.to_path_buf(), path.into())
                            } else {
                                NavTarget::assume_dir(path)
                            };
                            tabs.create_tab(TabPosition::End, target, true);
                        }
                        Err(e) => {
                            error!("Failed to jump to file from URI {uri}: {e}");
                        }
                    }
                }
            }
            "ShowFolders" => {
                for uri in uris {
                    match filename_from_uri(uri) {
                        Ok((path, _)) => {
                            let target = NavTarget::assume_dir(path);
                            tabs.create_tab(TabPosition::End, target, true);
                        }
                        Err(e) => {
                            error!("Failed to open folder from URI {uri}: {e}");
                        }
                    }
                }
            }
            "ShowItemProperties" => {
                info!("ShowItemProperties not implemented");
            }
            _ => {
                return Err(glib::Error::new(
                    DBusError::UnknownMethod,
                    &format!("Unknown method {name}"),
                ));
            }
        }

        Ok(None)
    }
}
