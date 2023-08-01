use std::borrow::BorrowMut;
use std::cell::{OnceCell, RefCell};
use std::fs::ReadDir;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use gtk::gio::{self, Cancellable, FileCopyFlags};
use gtk::glib::{self, SourceId};
use gtk::prelude::{CancellableExt, FileExt, FileExtManual};

use super::tabs::id::TabId;
use super::{gui_run, Gui};
use crate::config::{DirectoryCollision, FileCollision, CONFIG};
use crate::gui::{show_error, show_warning};

#[derive(Debug)]
enum Undoable {
    Move { source: PathBuf, dest: PathBuf },
    Copy(PathBuf),
    Trash { source: PathBuf, dest: PathBuf },
    RemoveSourceDir(PathBuf),
    CreateDestDir(PathBuf),
}

#[derive(Debug)]
pub enum Kind {
    Move(Arc<Path>),
    Copy(Arc<Path>),
    Delete { trash: bool },
}

impl Kind {
    const fn str(&self) -> &'static str {
        match self {
            Self::Move(_) => "move",
            Self::Copy(_) => "copy",
            Self::Delete { .. } => "delete",
        }
    }
}

// Basic directory state machine
// Flow is:
// Encounter a source directory.
// Create a destination directory or resolve collision.
// Push the directory onto the stack.
// Enter a directory-> process all its files -> exit a directory.
// We DFS and when we finish with a directory we process that directory.
#[derive(Debug)]
struct Directory {
    rel_path: PathBuf,
    dest_path: PathBuf,
    // We'll open at most one directory at a time for each level of depth.
    iter: ReadDir,
}

#[derive(Debug)]
pub struct Progress {
    // progress: Vec<Outcome>,
    // finished: usize,
    dirs: Vec<Directory>,
    files: Vec<PathBuf>,

    // undo_log: Vec<Undoable>,

    // For Ask
    pending_pair: Option<(PathBuf, PathBuf)>,

    directory_collisions: DirectoryCollision,
    file_collisions: FileCollision,

    // process_callback: Option<SourceId>,
    update_timeout: Option<SourceId>,
}

#[derive(Debug)]
enum Next {
    FinishedDir(PathBuf),
    Files(PathBuf, PathBuf),
}

impl Progress {
    fn next_pair(&mut self, dest_dir: &Path) -> Option<Next> {
        if let Some(dir) = self.dirs.last() {
            todo!();
        }

        let mut src = self.files.pop()?;

        while src.file_name().is_none() {
            error!("Tried to move file without filename");
            src = self.files.pop()?;
        }

        let name = src.file_name().unwrap();
        let dest = dest_dir.to_path_buf().join(name);

        Some(Next::Files(src, dest))
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // if let Some(s) = self.process_callback.take() {
        //     s.remove();
        // }

        if let Some(s) = self.update_timeout.take() {
            s.remove();
        }
    }
}

#[derive(Debug)]
pub struct Operation {
    // May become dangling while this is ongoing.
    tab: TabId,
    kind: Kind,
    cancel: Cancellable,
    // Just clone the paths directly instead of needing to convert everything to an Rc up front.
    progress: RefCell<Progress>,
}

impl Operation {
    fn new(tab: TabId, kind: Kind, files: Vec<PathBuf>) -> Option<Rc<Self>> {
        if files.is_empty() {
            warn!("Got empty file operation {}, ignoring.", kind.str());
            return None;
        }

        // Abort if any of them are strictly invalid.
        // Moves into the same dir will be skipped later.
        match &kind {
            Kind::Move(p) | Kind::Copy(p) => {
                if let Some(invalid) = files.iter().find(|f| p.starts_with(f)) {
                    show_warning(&format!("Invalid {} of {invalid:?} into {p:?}", kind.str()));
                    return None;
                }
            }
            Kind::Delete { .. } => todo!(),
        }

        // TODO -- when updating progress, do not allow for ref cycles
        let rc = Rc::new_cyclic(|weak: &Weak<Self>| {
            // Start with no timeout.
            let w = weak.clone();
            let update_timeout = glib::idle_add_local_once(move || {
                let Some(op) = w.upgrade() else {
                    return;
                };

                op.progress.borrow_mut().update_timeout.take();
                error!("TODO -- update progress bar");
            });

            let progress = Progress {
                // finished: 0,
                files,
                dirs: Vec::new(),
                pending_pair: None,
                directory_collisions: CONFIG.directory_collisions,
                file_collisions: CONFIG.file_collisions,
                // process_callback: Some(process_callback),
                update_timeout: Some(update_timeout),
            };

            Self {
                tab,
                cancel: Cancellable::new(),
                kind,
                progress: RefCell::new(progress),
            }
        });


        let o = rc.clone();
        glib::idle_add_local_once(move || o.process_one());

        Some(rc)
    }

    fn process_one(self: Rc<Self>) {
        if self.cancel.is_cancelled() {
            info!("Cancelled operation {:?}", self.kind);

            return gui_run(|g| g.finish_operation(self));
        }

        let done = match &self.kind {
            Kind::Move(p) => self.process_next_move(p),
            Kind::Copy(p) => self.process_next_copy(p),
            Kind::Delete { .. } => todo!(),
        };

        if done {
            return gui_run(|g| g.finish_operation(self));
        }

        //     glib::idle_add_local_once(move || {
        //         self.process_one();
        //     });
    }

    fn process_next_move(self: &Rc<Self>, dest: &Path) -> bool {
        let mut progress = self.progress.borrow_mut();

        let (src, dst) = loop {
            let (src, dst) = match progress.next_pair(dest) {
                Some(Next::Files(src, dst)) => (src, dst),
                Some(Next::FinishedDir(_dir)) => todo!(),
                None => return true,
            };

            if src == dst {
                info!("Skipping no-op move for {src:?}");
                continue;
            }

            if !src.exists() {
                error!("Could not {} {:?} as it no longer exists", self.kind.str(), src);
                continue;
            } else if src.is_dir() && !src.is_symlink() {
                todo!("move directory");
            }

            break (src, dst);
        };

        let source = gio::File::for_path(&src);
        let dest = gio::File::for_path(&dst);

        let mut flags = FileCopyFlags::NOFOLLOW_SYMLINKS | FileCopyFlags::ALL_METADATA;

        if dst.exists() {
            if !dst.is_file() {
                // Probably just warn and fail
                todo!("Handle file onto non-file collision");
            }

            match progress.file_collisions {
                FileCollision::_Ask => todo!(),
                FileCollision::_Overwrite => {
                    trace!("Overwriting target file {dst:?}");
                    flags |= FileCopyFlags::OVERWRITE;
                }
                FileCollision::Skip => {
                    info!("Skipping moving {src:?} since {dst:?} exists");
                    let s = self.clone();
                    // We did enough IO to justify a new task
                    glib::idle_add_local_once(move || s.process_one());
                    return false;
                }
            }
        }

        error!("Would move from {:?} to {:?}", src, dst);
        return false;
        let s = self.clone();
        source.move_async(
            &dest,
            flags,
            glib::Priority::LOW,
            Some(&self.cancel),
            None,
            move |result| {
                if let Err(e) = result {
                    if !s.cancel.is_cancelled() {
                        show_error(&format!(
                            "Failed to move file {src:?} to {dst:?}, aborting operation: {e}"
                        ));

                        s.cancel.cancel();
                    }
                } else {
                    trace!("Finished moving {src:?} to {dst:?}");
                }

                s.process_one()
            },
        );

        false
    }

    fn process_next_copy(self: &Rc<Self>, dest: &Path) -> bool {
        let mut progress = self.progress.borrow_mut();

        let (src, dst) = loop {
            let (src, dst) = match progress.next_pair(dest) {
                Some(Next::Files(src, dst)) => (src, dst),
                Some(Next::FinishedDir(_dir)) => todo!(),
                None => return true,
            };

            if src == dst {
                info!("Finding new name for copy onto self for {src:?}");
                todo!()
            }

            if !src.exists() {
                error!("Could not copy {:?} as it no longer exists", src);
                continue;
            } else if src.is_dir() && !src.is_symlink() {
                todo!("copy directory");
            }

            break (src, dst);
        };

        let source = gio::File::for_path(&src);
        let dest = gio::File::for_path(&dst);

        let mut flags = FileCopyFlags::NOFOLLOW_SYMLINKS;

        if dst.exists() {
            if !dst.is_file() {
                // Probably just warn and fail
                todo!("Handle file onto non-file collision");
            }

            match progress.file_collisions {
                FileCollision::_Ask => todo!(),
                FileCollision::_Overwrite => {
                    trace!("Overwriting target file {dst:?}");
                    flags |= FileCopyFlags::OVERWRITE;
                }
                FileCollision::Skip => {
                    info!("Skipping copying {src:?} since {dst:?} exists");
                    let s = self.clone();
                    // We did enough IO to justify a new task
                    glib::idle_add_local_once(move || s.process_one());
                    return false;
                }
            }
        }

        error!("Would copy from {:?} to {:?}", src, dst);
        return false;
        let s = self.clone();
        source.copy_async(
            &dest,
            flags,
            glib::Priority::LOW,
            Some(&self.cancel),
            None,
            move |result| {
                if let Err(e) = result {
                    if !s.cancel.is_cancelled() {
                        show_error(&format!(
                            "Failed to copy file {src:?} to {dst:?}, aborting operation: {e}"
                        ));
                        s.cancel.cancel();
                    }
                } else {
                    trace!("Finished copying {src:?} to {dst:?}");
                }

                s.process_one();
            },
        );

        false
    }
}

impl Gui {
    fn finish_operation(self: &Rc<Self>, finished: Rc<Operation>) {
        let mut ops = self.ongoing_operations.borrow_mut();
        if let Some(index) = ops.iter().position(|o| Rc::ptr_eq(o, &finished)) {
            ops.swap_remove(index);
        }
    }

    pub(super) fn start_operation(self: &Rc<Self>, tab: TabId, kind: Kind, files: Vec<PathBuf>) {
        let Some(op) = Operation::new(tab, kind, files) else {
            return error!("Failed to start operation");
        };

        self.ongoing_operations.borrow_mut().push(op);
    }
}
