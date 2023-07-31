use std::cell::{OnceCell, RefCell};
use std::cmp::Reverse;
use std::fs::ReadDir;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use gtk::gio::{self, Cancellable, FileCopyFlags};
use gtk::glib::{self, ControlFlow, SourceId};
use gtk::prelude::{CancellableExt, FileExt};

use super::gui_run;
use super::tabs::id::TabId;
use crate::config::{DirectoryCollision, FileCollision, CONFIG};

// Only needs to be stored to allow for undo operations later.
// #[derive(Debug, Clone, Copy)]
// enum Outcome {
//     Move,
//     Copy,
//     Trash,
//     Delete,
//     Skip,
// }

#[derive(Debug)]
pub enum Kind {
    Move(Arc<Path>),
    Copy(Arc<Path>),
    Delete(bool),
}

impl Kind {
    const fn str(&self) -> &'static str {
        match self {
            Self::Move(_) => "move",
            Self::Copy(_) => "copy",
            Self::Delete(_) => "delete",
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
#[derive(Debug, Default)]
struct Directory {
    rel_path: PathBuf,
    dest_path: PathBuf,
    // We'll open at most one directory at a time for each level of depth.
    iter: OnceCell<ReadDir>,
}

#[derive(Debug)]
pub struct Progress {
    // progress: Vec<Outcome>,
    // finished: usize,
    dirs: Vec<Directory>,
    files: Vec<PathBuf>,
    // For Ask
    pending_pair: Option<(PathBuf, PathBuf)>,

    directory_collisions: DirectoryCollision,
    file_collisions: FileCollision,

    // process_callback: Option<SourceId>,
    update_timeout: Option<SourceId>,
}

impl Progress {
    fn next_file_pair(&mut self, dest_dir: &Path) -> Option<(PathBuf, PathBuf)> {
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

        Some((src, dest))
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.update_timeout.take().unwrap().remove();
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
    pub fn new(tab: TabId, kind: Kind, mut files: Vec<PathBuf>) -> Option<Rc<Self>> {
        if files.is_empty() {
            warn!("Got empty file operation {}, ignoring.", kind.str());
            return None;
        }

        // files.sort_unstable_by(|a, b| b.cmp(a));

        match &kind {
            Kind::Move(p) | Kind::Copy(p) => {
                if p.starts_with(&files[0]) {
                    warn!("Cannot {} ancestor {:?} into {p:?}, ignoring.", kind.str(), files[0]);
                    return None;
                }
            }
            Kind::Delete(_) => todo!(),
        }

        // TODO -- when updating progress, do not allow for ref cycles
        let rc = Rc::new_cyclic(|weak: &Weak<Self>| {
            // let w = weak.clone();

            // Start with no timeout.
            let w = weak.clone();
            let update_timeout = glib::idle_add_local_once(move || {
                let Some(_op) = w.upgrade() else {
                    return;
                };

                error!("TODO -- update progress bar");
            });

            let progress = Progress {
                // finished: 0,
                files,
                dirs: Vec::new(),
                pending_pair: None,
                directory_collisions: CONFIG.directory_collisions,
                file_collisions: CONFIG.file_collisions,
                update_timeout: Some(update_timeout),
                // process_callback: Some(process_callback),
            };

            Self {
                tab,
                cancel: Cancellable::new(),
                kind,
                progress: RefCell::new(progress),
            }
        });

        Some(rc)
    }

    fn process_one(weak: &Weak<Self>) {
        let Some(op) = weak.upgrade() else {
            return;
        };

        if op.cancel.is_cancelled() {
            // gui_run(|g| g.finish_operation(op));
            return;
        }

        let done = match &op.kind {
            Kind::Move(p) => op.process_next_move(p),
            Kind::Copy(_) => todo!(),
            Kind::Delete(_) => todo!(),
        };

        if done {
            op.cancel.cancel();
            // gui_run(|g| g.finish_operation(op));
            return;
        }

        let weak = weak.clone();
        glib::idle_add_local_once(move || {
            error!("TODO -- remove logging callback, it's wrong");
            Self::process_one(&weak);
        });
        // op.process_next();
        // op.process()
        // ControlFlow::Continue
    }

    // Returns true when all processing is done.
    fn process_next_move(self: &Rc<Self>, dest: &Path) -> bool {
        let mut progress = self.progress.borrow_mut();

        loop {
            let Some((src, dst)) = progress.next_file_pair(dest) else {
                return true;
            };
            // if let Some(dir) = &progress.stack.last() {
            //     todo!()
            // }

            if !src.exists() {
                error!("Could not {} {:?} as it no longer exists", self.kind.str(), src);
                continue;
                // } else if path.is_dir() && !path.is_symlink() {
                // todo!()
            }

            if dst.exists() {
                if !dst.is_file() {
                    todo!("Handle collision");
                }
                todo!("Handle collision");
            }

            let file = gio::File::for_path(&src);

            let mut flags = FileCopyFlags::NOFOLLOW_SYMLINKS | FileCopyFlags::ALL_METADATA;

            error!("Would move from {:?} to {:?}", src, dst);
            // For Ask
            // progress.pending_pair = Some((src, dst));
            break;
        }

        false
    }
}
