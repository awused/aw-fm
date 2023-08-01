use std::cell::RefCell;
use std::fs::{remove_dir, ReadDir};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use gtk::gio::{self, Cancellable, FileCopyFlags, FileInfo, FileQueryInfoFlags};
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

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.str())
    }
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
    abs_path: PathBuf,
    dest: PathBuf,
    // We'll open at most one directory at a time for each level of depth.
    iter: ReadDir,
    original_info: Option<FileInfo>,
}

impl Directory {
    fn apply_info(&self) {
        let Some(info) = &self.original_info else {
            return;
        };

        trace!("Setting saved attributes on destination directory {:?}", self.dest);
        // Synchronous is good enough here.

        let dest = gio::File::for_path(&self.dest);
        if let Err(e) = dest.set_attributes_from_info(
            info,
            FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Cancellable::NONE,
        ) {
            error!("Couldn't set saved attributes on {:?}: {e}", self.dest);
        }
    }
}

#[derive(Debug)]
enum Asking {
    Directory(PathBuf, PathBuf),
    File(PathBuf, PathBuf),
}

#[derive(Debug)]
pub struct Progress {
    // progress: Vec<Outcome>,
    // finished: usize,
    dirs: Vec<Directory>,
    files: Vec<PathBuf>,

    // undo_log: Vec<Undoable>,

    // For Ask
    pending_pair: Option<Asking>,

    directory_collisions: DirectoryCollision,
    file_collisions: FileCollision,

    // process_callback: Option<SourceId>,
    update_timeout: Option<SourceId>,
}

#[derive(Debug)]
enum Next {
    FinishedDir(Directory),
    Files(PathBuf, PathBuf),
}

impl Progress {
    fn next_pair(&mut self, dest_root: &Path) -> Option<Next> {
        if let Some(dir) = &mut self.dirs.last_mut() {
            for next in dir.iter.by_ref() {
                let name = match next {
                    Ok(de) => de.file_name(),
                    Err(e) => {
                        show_warning(&format!(
                            "Failed to read contents of directory {:?}: {e}",
                            dir.abs_path
                        ));
                        continue;
                    }
                };

                println!("{dir:?} {name:?}");
                return Some(Next::Files(dir.abs_path.join(&name), dir.dest.join(name)));
            }

            return Some(Next::FinishedDir(self.dirs.pop().unwrap()));
        }

        let mut src = self.files.pop()?;

        while src.file_name().is_none() {
            error!("Tried to move file without filename");
            src = self.files.pop()?;
        }

        let name = src.file_name().unwrap();
        let dest = dest_root.to_path_buf().join(name);

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

struct CopyMovePrep {
    src: PathBuf,
    dst: PathBuf,
    overwrite: bool,
}


enum Status {
    AsyncScheduled,
    CallAgain,
    Done,
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
                    show_warning(&format!("Invalid {kind} of {invalid:?} into {p:?}"));
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
        glib::idle_add_local_once(move || o.process_next());

        Some(rc)
    }

    fn process_next(self: Rc<Self>) {
        if self.cancel.is_cancelled() {
            info!("Cancelled operation {:?}", self.kind);

            return gui_run(|g| g.finish_operation(self));
        }

        let status = match &self.kind {
            Kind::Move(p) => self.process_next_move(p),
            Kind::Copy(p) => self.process_next_copy(p),
            Kind::Delete { .. } => todo!(),
        };

        match status {
            Status::AsyncScheduled => {}
            Status::CallAgain => {
                glib::idle_add_local_once(move || self.process_next());
            }
            Status::Done => gui_run(|g| g.finish_operation(self)),
        }
    }

    fn process_next_move(self: &Rc<Self>, dest: &Path) -> Status {
        let (src, dst) = loop {
            let (src, dst) = match self.progress.borrow_mut().next_pair(dest) {
                Some(Next::Files(src, dst)) => (src, dst),
                Some(Next::FinishedDir(dir)) => {
                    dir.apply_info();

                    info!("Removing source directory {dir:?}");
                    match remove_dir(&dir.abs_path) {
                        Ok(_) => error!("TODO -- undoable"),
                        Err(e) => error!("Failed to remove source directory {dir:?}: {e}"),
                    }

                    return Status::CallAgain;
                }
                None => return Status::Done,
            };

            if src == dst {
                info!("Skipping no-op move for {src:?}");
                continue;
            }

            if !src.exists() {
                error!("Could not {} {src:?} as it no longer exists", self.kind);
                continue;
            }

            break (src, dst);
        };

        let Some(CopyMovePrep { src, dst, overwrite }) = self.prepare_copymove(src, dst) else {
            return Status::CallAgain;
        };

        let mut flags = FileCopyFlags::NOFOLLOW_SYMLINKS | FileCopyFlags::ALL_METADATA;
        if overwrite {
            flags |= FileCopyFlags::OVERWRITE;
        }

        trace!("move from {:?} to {:?} with {flags}", src, dst);
        let source = gio::File::for_path(&src);
        let dest = gio::File::for_path(&dst);

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
                    error!("TODO -- undoable");
                }

                s.process_next()
            },
        );

        Status::AsyncScheduled
    }

    fn process_next_copy(self: &Rc<Self>, dest: &Path) -> Status {
        let (src, dst) = loop {
            let (src, dst) = match self.progress.borrow_mut().next_pair(dest) {
                Some(Next::Files(src, dst)) => (src, dst),
                Some(Next::FinishedDir(dir)) => {
                    dir.apply_info();
                    return Status::CallAgain;
                }
                None => return Status::Done,
            };

            if src == dst {
                info!("Finding new name for copy onto self for {src:?}");
                todo!()
            }

            if !src.exists() {
                error!("Could not copy {:?} as it no longer exists", src);
                continue;
            }

            break (src, dst);
        };


        let Some(CopyMovePrep { src, dst, overwrite }) = self.prepare_copymove(src, dst) else {
            return Status::CallAgain;
        };

        let mut flags = FileCopyFlags::NOFOLLOW_SYMLINKS;
        if overwrite {
            flags |= FileCopyFlags::OVERWRITE;
        }

        trace!("copy from {:?} to {:?} with {flags}", src, dst);
        let source = gio::File::for_path(&src);
        let dest = gio::File::for_path(&dst);


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
                    error!("TODO -- undoable");
                }

                s.process_next();
            },
        );

        Status::AsyncScheduled
    }

    fn prepare_copymove(self: &Rc<Self>, src: PathBuf, dst: PathBuf) -> Option<CopyMovePrep> {
        if src.is_dir() && !src.is_symlink() {
            self.prepare_dest_dir(src, dst);
            return None;
        }

        let progress = self.progress.borrow_mut();
        let mut overwrite = false;

        if dst.exists() {
            if !dst.is_file() {
                // Probably just warn and fail
                todo!("Handle file onto non-file collision");
            }

            match progress.file_collisions {
                FileCollision::_Ask => todo!(),
                FileCollision::_Overwrite => {
                    trace!("Overwriting target file {dst:?}");
                    overwrite = true;
                }
                FileCollision::Skip => {
                    info!("Skipping {} of {src:?} to existing {dst:?}", self.kind.str());
                    return None;
                }
            }
        }

        Some(CopyMovePrep { src, dst, overwrite })
    }

    fn prepare_dest_dir(self: &Rc<Self>, src: PathBuf, dst: PathBuf) {
        let mut progress = self.progress.borrow_mut();

        let original_info = if dst.exists() {
            if !dst.is_dir() {
                // Probably just warn and fail
                todo!("Handle directory onto non-directory");
            }

            match progress.directory_collisions {
                DirectoryCollision::_Ask => todo!(),
                DirectoryCollision::Merge => {
                    debug!("Merging {src:?} into existing directory {dst:?}");
                }
                DirectoryCollision::Skip => {
                    debug!("Skipping {} of directory {src:?} since {dst:?} exists", self.kind);
                    return;
                }
            }

            None
        } else {
            // Just do all this synchronously, it'll be fine.
            trace!("Creating destination directory {dst:?}");

            let source = gio::File::for_path(&src);

            // Should only fail if cancelled, which we aren't doing.
            let attributes = match source.build_attribute_list_for_copy(
                FileCopyFlags::NOFOLLOW_SYMLINKS | FileCopyFlags::ALL_METADATA,
                Cancellable::NONE,
            ) {
                Ok(attr) => attr,
                Err(e) => {
                    show_error(&format!("Failed to read source directory {src:?}: {e}"));
                    return self.cancel.cancel();
                }
            };

            let info = match source.query_info(
                &attributes,
                FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                Cancellable::NONE,
            ) {
                Ok(info) => info,
                Err(e) => {
                    show_error(&format!("Failed to create destination directory {dst:?}: {e}"));
                    return self.cancel.cancel();
                }
            };

            if let Err(e) = std::fs::create_dir(&dst) {
                show_error(&format!("Failed to create destination directory {dst:?}: {e}"));
                return self.cancel.cancel();
            }
            error!("TODO -- undoable");

            Some(info)
        };

        trace!("Entering source directory {src:?}, destination: {dst:?}");
        let read_dir = match std::fs::read_dir(&src) {
            Ok(read_dir) => read_dir,
            Err(e) => {
                show_error(&format!("Failed to {} directory {src:?}: {e}", self.kind));
                self.cancel.cancel();
                return;
            }
        };

        progress.dirs.push(Directory {
            abs_path: src,
            dest: dst,
            iter: read_dir,
            original_info,
        });
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
