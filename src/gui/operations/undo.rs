use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use gtk::gio::FileInfo;

use super::{Operation, Status};
use crate::gui::operations::{Outcome, ReadyCopyMove};
use crate::gui::show_warning;

impl Operation {
    pub(super) fn process_next_undo(
        self: &Rc<Self>,
        prev: &Rc<Self>,
        pending_dirs: &RefCell<Vec<(Arc<Path>, FileInfo)>>,
    ) -> Status {
        let Some(next) = prev.progress.borrow_mut().pop_next_undoable() else {
            if let Some(_dir_info) = pending_dirs.borrow_mut().pop() {
                show_warning("Not implemented");
                return Status::AsyncScheduled;
            }
            return Status::Done;
        };

        trace!("Undoing {next:?}");

        match next {
            Outcome::Move { source, dest } => {
                let ready = ReadyCopyMove {
                    src: dest.into(),
                    dst: source.to_path_buf(),
                    overwrite: false,
                };

                self.do_move(ready);
                Status::AsyncScheduled
            }
            Outcome::Copy(path) | Outcome::CopyOverwrite(path) => {
                // TODO -- ask for confirmation on deletion?
                self.do_delete(path.into(), false);
                Status::AsyncScheduled
            }
            Outcome::NewFile(path) => {
                let Ok(metadata) = path.metadata() else {
                    info!("Couldn't stat file {path:?}, not attempting to remove");
                    return Status::CallAgain;
                };

                if metadata.len() != 0 {
                    info!("File {path:?} is not empty, it has been modified. Not removing.");
                    return Status::CallAgain;
                }

                self.do_delete(path.into(), false);
                Status::AsyncScheduled
            }
            Outcome::RemoveSourceDir(_path, _file_info) => {
                // Should only push file_info onto pending if we actually recreate it
                show_warning("Not implemented");
                Status::Done
            }
            Outcome::CreateDestDir(path) => {
                // This will only delete empty directories
                self.do_delete(path.into(), true);
                Status::AsyncScheduled
            }
            Outcome::MergeDestDir(_)
            | Outcome::Skip
            | Outcome::Delete
            | Outcome::DeleteDir
            | Outcome::Trash => unreachable!(),
        }
    }
}
