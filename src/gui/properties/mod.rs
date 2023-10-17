use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::{Gui, Selected};
use crate::com::{ChildInfo, ManagerAction};


pub mod dialog;

impl Gui {
    pub(super) fn properties_dialog(
        self: &Rc<Self>,
        location: &Path,
        search: bool,
        selected: Selected<'_>,
    ) {
        let cancel: Arc<AtomicBool> = Arc::default();

        let (dirs, files): (Vec<_>, _) = selected.partition(|eo| eo.get().dir());

        info!(
            "Opening properties dialog for {} files and {} directories",
            files.len(),
            dirs.len()
        );

        if !dirs.is_empty() {
            let paths = dirs.iter().map(|eo| eo.get().abs_path.clone()).collect();
            self.send_manager(ManagerAction::GetChildren(paths, cancel.clone()));
        }

        let prop = dialog::PropDialog::show(self, location, search, cancel, files, dirs);
        self.open_dialogs.borrow_mut().properties.push(prop);
    }

    pub(super) fn handle_properties_update(&self, id: Arc<AtomicBool>, children: ChildInfo) {
        for d in &self.open_dialogs.borrow().properties {
            if d.matches(&id) {
                d.add_children(children);
                return;
            }
        }
    }
}
