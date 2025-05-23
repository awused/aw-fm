use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use gtk::gio::{Cancellable, File, FileInfo, FileQueryInfoFlags};
use gtk::prelude::FileExt;

use super::{Operation, Status};
use crate::gui::operations::{Outcome, ReadyCopyMove};
use crate::gui::show_error;

impl Operation {
    pub(super) fn process_next_undo(
        self: &Rc<Self>,
        prev: &Rc<Self>,
        pending_dirs: &RefCell<Vec<(Arc<Path>, FileInfo)>>,
    ) -> Status {
        let Some(next) = prev.progress.borrow_mut().pop_next_undoable() else {
            if let Some((dir, info)) = pending_dirs.borrow_mut().pop() {
                if let Err(e) = File::for_path(&dir).set_attributes_from_info(
                    &info,
                    FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                    Cancellable::NONE,
                ) {
                    error!("Couldn't set saved attributes on {dir:?}: {e}");
                }
                return Status::CallAgain;
            }
            return Status::Done;
        };

        trace!("Undoing {next:?}");

        match next {
            Outcome::Move { source, dest } => {
                let ready = ReadyCopyMove { src: dest, dst: source, overwrite: false };

                // This allows for fallbacks for move, but will try a rename first, so we don't
                // explicitly need to handle optimistic renames.
                self.do_move(ready);
                Status::AsyncScheduled
            }
            Outcome::Copy(path) | Outcome::CopyOverwrite(path) => {
                // TODO -- ask for confirmation on deletion?
                self.do_delete(path, false);
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

                self.do_delete(path, false);
                Status::AsyncScheduled
            }
            Outcome::RemoveSourceDir(path, file_info) => {
                if path.exists() {
                    info!("Not recreating {path:?} since it already exists");
                    return Status::CallAgain;
                }

                match std::fs::create_dir(&path) {
                    Ok(_) => {
                        trace!("Recreated directory {path:?}");

                        self.progress
                            .borrow_mut()
                            .push_outcome(Outcome::CreateDestDir(path.clone()));

                        pending_dirs.borrow_mut().push((path, file_info));
                    }
                    Err(e) => {
                        let msg = format!("Failed to recreate directory {path:?}: {e}");
                        show_error(msg);
                    }
                }

                Status::CallAgain
            }
            Outcome::CreateDestDir(path) => {
                // TODO -- save file_info for a redo?
                // This will only delete empty directories
                self.do_delete(path, true);
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
