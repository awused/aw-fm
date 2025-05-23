use std::cell::{Ref, RefCell};
use std::collections::VecDeque;
use std::fs::{ReadDir, remove_dir};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::gio::{self, Cancellable, FileCopyFlags, FileInfo, FileQueryInfoFlags};
use gtk::glib;
use gtk::prelude::*;
use once_cell::unsync::Lazy;
use regex::bytes::{Captures, Regex};

use self::progress::Progress;
use super::tabs::id::TabId;
use super::{Gui, gui_run};
use crate::config::{CONFIG, DirectoryCollision, FileCollision};
use crate::gui::operations::ask::AskDialog;
use crate::gui::{show_error, show_warning, tabs_run};

mod ask;
mod progress;
mod undo;

const OPERATIONS_HISTORY: usize = 10;

thread_local! {
    static COPY_REGEX: Lazy<Regex> =
        Lazy::new(||Regex::new(r"^(.*)( \(copy (\d+)\))(\.[^/]+)?$").unwrap());
    static COPIED_REGEX: Lazy<Regex> =
        Lazy::new(||Regex::new(r"^(.*)( \(copied (\d+)\))(\.[^/]+)?$").unwrap());
    static MOVED_REGEX: Lazy<Regex> =
        Lazy::new(||Regex::new(r"^(.*)( \(moved (\d+)\))(\.[^/]+)?$").unwrap());
}

// Whatever we add to a name to resolve collisions
#[derive(Debug, Clone, Copy)]
enum Fragment {
    Copy,
    Copied,
    Moved,
}

impl Fragment {
    fn captures(self, bytes: &[u8]) -> Option<Captures<'_>> {
        match self {
            Self::Copy => COPY_REGEX.with(|r| r.captures(bytes)),
            Self::Copied => COPIED_REGEX.with(|r| r.captures(bytes)),
            Self::Moved => MOVED_REGEX.with(|r| r.captures(bytes)),
        }
    }

    const fn str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Copied => "copied",
            Self::Moved => "moved",
        }
    }
}

// It should be possible to undo (best-effort) a file operation by reversing each completed action
// in roughly reverse order.
#[derive(Debug)]
pub enum Outcome {
    // Includes overwrites, undo -> move back if no conflict
    Move { source: Arc<Path>, dest: Arc<Path> },
    // Does not include overwrite copies, undo -> delete with no confirmation
    Copy(Arc<Path>),
    // Only overwrites from copy, undo -> delete with confirmation??
    CopyOverwrite(Arc<Path>),
    // Undo -> delete if still 0 sized
    NewFile(Arc<Path>),
    // FileInfo needs to be restored after we populate the contents, which is awkward.
    // Could unconditionally store FileInfo to restore it, probably not worth it.
    RemoveSourceDir(Arc<Path>, FileInfo),
    CreateDestDir(Arc<Path>),
    // Nothing really happened here
    MergeDestDir(Arc<Path>),
    Skip,
    Delete,
    // Not undoable, while the directory could be recreated that's not terrible useful.
    DeleteDir,
    // Not undoable without dumb hacks: https://gitlab.gnome.org/GNOME/glib/-/issues/845
    Trash,
}

impl Outcome {
    const fn undoable(&self) -> bool {
        match self {
            Self::Move { .. }
            | Self::Copy(_)
            | Self::CopyOverwrite(_)
            | Self::NewFile(_)
            | Self::RemoveSourceDir(..)
            | Self::CreateDestDir(_) => true,
            Self::MergeDestDir(_) | Self::Skip | Self::Delete | Self::DeleteDir | Self::Trash => {
                false
            }
        }
    }
}

#[derive(Debug)]
pub enum Kind {
    Move(Arc<Path>),
    Copy(Arc<Path>),
    Rename(Arc<Path>),

    MakeDir(Arc<Path>),
    MakeFile(Arc<Path>),

    // In theory, at least, it should be possible to redo an undo.
    // Probably won't support this, but keep the skeleton intact.
    Undo {
        prev: Rc<Operation>,
        // These should be processed FILO, just like outcomes from progress.log
        pending_dir_info: RefCell<Vec<(Arc<Path>, FileInfo)>>,
        // TODO
        // destroy_overwrites: Cell<bool>,
    },
    Trash(Arc<Path>),
    Delete(Arc<Path>),
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.str())
    }
}

impl Kind {
    const fn str(&self) -> &'static str {
        match self {
            Self::Move(_) => "Move",
            Self::Copy(_) => "Copy",
            Self::Rename(_) => "Rename",
            Self::MakeDir(_) => "MakeDir",
            Self::MakeFile(_) => "MakeFile",
            Self::Undo { .. } => "Undo",
            Self::Trash(_) => "Trash",
            Self::Delete(_) => "Delete",
        }
    }

    // Some of these should never be displayed unless something is seriously wrong
    fn dir(&self) -> &Path {
        let mut s = self;
        while let Self::Undo { prev, .. } = s {
            s = &prev.kind;
        }

        match s {
            Self::Move(d)
            | Self::Copy(d)
            | Self::Rename(d)
            | Self::Trash(d)
            | Self::Delete(d)
            | Self::MakeDir(d)
            | Self::MakeFile(d) => d,
            Self::Undo { .. } => unreachable!(),
        }
    }

    const fn rename_fragment(&self) -> Fragment {
        match self {
            Self::Move(_) => Fragment::Moved,
            Self::Copy(_) => Fragment::Copied,
            Self::Rename(_)
            | Self::MakeDir(_)
            | Self::MakeFile(_)
            | Self::Undo { .. }
            | Self::Trash(_)
            | Self::Delete(_) => unreachable!(),
        }
    }
}

// Basic directory state machine, processing is depth-first.
// Flow for copy/move is:
// Encounter a source directory.
// Create a destination directory or resolve collision.
// Push the directory onto the stack.
// Enter a directory-> process all its files -> exit a directory.
// Apply original_info if the destination directory did not already exist.
// Remove the source directory if it is a move and it is empty.
//
// For delete/trash:
// Encounter a source directory.
// Push the directory onto the stack.
// Enter a directory-> process all its files -> exit a directory.
#[derive(Debug)]
struct SourceDirectory {
    abs_path: Arc<Path>,
    // We'll open at most one directory at a time for each level of depth.
    iter: ReadDir,
}

#[derive(Debug)]
struct DestinationDirectory {
    source: SourceDirectory,
    dest: Arc<Path>,
    // If we allow restores for deletions, move this to SourceDirectory
    //
    // This is currently useless if copying to an existing directory, but not worth optimizing out
    // because it could be used for restoring attributes when undoing a Copy in the future.
    original_info: FileInfo,
    already_existed: bool,
}

impl DestinationDirectory {
    fn apply_info(&self) {
        if self.already_existed {
            return;
        }

        trace!("Setting saved attributes on destination directory {:?}", self.dest);
        // Synchronous is good enough here.

        let dest = gio::File::for_path(&self.dest);
        if let Err(e) = dest.set_attributes_from_info(
            &self.original_info,
            FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Cancellable::NONE,
        ) {
            error!("Couldn't set saved attributes on {:?}: {e}", self.dest);
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum ConflictKind {
    DirDir,
    // DirFile
    FileFile,
    // FileDir
}

impl ConflictKind {
    const fn dst_str(self) -> &'static str {
        match self {
            Self::DirDir => "directory",
            Self::FileFile => "file",
        }
    }
}

#[derive(Debug)]
struct Conflict {
    kind: ConflictKind,
    src: Arc<Path>,
    dst: Arc<Path>,
}


#[derive(Debug)]
enum NextCopyMove {
    FinishedDir(DestinationDirectory),
    Files(Arc<Path>, Arc<Path>),
}

#[derive(Debug)]
enum NextRemove {
    FinishedDir(SourceDirectory),
    File(Arc<Path>),
}


#[derive(Debug)]
pub struct Operation {
    // May become dangling while this is ongoing.
    pub tab: TabId,
    pub kind: Kind,
    cancellable: Cancellable,
    // Just clone the paths directly instead of needing to convert everything to an Rc up front.
    progress: RefCell<Progress>,
}

struct ReadyCopyMove {
    src: Arc<Path>,
    dst: Arc<Path>,
    overwrite: bool,
}

enum CopyMovePrep {
    Asking,
    Ready(ReadyCopyMove),
    Abort(String),
    CallAgain,
}

enum Status {
    AsyncScheduled,
    CallAgain,
    Done,
}

impl Operation {
    fn new(tab: TabId, kind: Kind, source_files: VecDeque<Arc<Path>>) -> Option<Rc<Self>> {
        // Abort if any of them are strictly invalid.
        // Moves into the same dir will be skipped later.
        match &kind {
            Kind::Move(p) | Kind::Copy(p) => {
                if source_files.is_empty() {
                    warn!("Got empty file operation {}, ignoring.", kind.str());
                    return None;
                }

                if let Some(invalid) = source_files.iter().find(|f| p.starts_with(f)) {
                    show_warning(format!("Invalid {kind} of {invalid:?} into {p:?}"));
                    return None;
                }
            }
            Kind::Rename(p) => {
                if source_files.len() != 1 {
                    show_warning(format!("Got invalid rename of {} files", source_files.len()));
                    return None;
                }

                if source_files[0].parent() != p.parent() {
                    show_warning("Got invalid rename: destination directory not the same");
                    return None;
                }
            }
            Kind::MakeDir(_) | Kind::MakeFile(_) => {
                if !source_files.is_empty() {
                    show_warning(format!(
                        "Got invalid MakeFile/MakeFolder with {} source files",
                        source_files.len()
                    ));
                    return None;
                }
            }
            Kind::Undo { .. } | Kind::Trash(_) | Kind::Delete(_) => {}
        }


        let rc = Rc::new_cyclic(|weak: &Weak<Self>| {
            let progress = Progress::new(weak.clone(), source_files);

            Self {
                tab,
                cancellable: Cancellable::new(),
                kind,
                progress: RefCell::new(progress),
            }
        });


        let s = rc.clone();
        glib::idle_add_local_once(move || s.process_next());

        Some(rc)
    }

    pub fn outcomes(&self) -> Ref<'_, [Outcome]> {
        let p = self.progress.borrow();
        Ref::map(p, Progress::log)
    }

    fn process_next(self: Rc<Self>) {
        if self.cancellable.is_cancelled() {
            info!("Cancelled operation {:?}", self.kind);

            return gui_run(|g| g.finish_operation(&self));
        }

        let status = match &self.kind {
            Kind::Move(p) => self.process_next_move(p),
            Kind::Copy(p) => self.process_next_copy(p),
            Kind::Rename(p) => self.process_rename(p),
            Kind::MakeDir(p) => self.process_make_dir(p),
            Kind::MakeFile(p) => self.process_make_file(p),
            Kind::Undo { prev, pending_dir_info } => self.process_next_undo(prev, pending_dir_info),
            Kind::Trash(_) => self.process_next_trash(),
            Kind::Delete(_) => self.process_next_delete(),
        };

        match status {
            Status::AsyncScheduled => {}
            Status::CallAgain => {
                glib::idle_add_local_once(move || self.process_next());
            }
            Status::Done => gui_run(|g| g.finish_operation(&self)),
        }
    }

    fn process_next_move(self: &Rc<Self>, dest: &Arc<Path>) -> Status {
        let (src, dst) = loop {
            let mut progress = self.progress.borrow_mut();

            let (src, dst) = match progress.next_copymove_pair(dest) {
                Some(NextCopyMove::Files(src, dst)) => (src, dst),
                Some(NextCopyMove::FinishedDir(dir)) => {
                    dir.apply_info();

                    info!("Removing source directory {dir:?}");
                    match remove_dir(&dir.source.abs_path) {
                        Ok(_) => progress.push_outcome(Outcome::RemoveSourceDir(
                            dir.source.abs_path,
                            dir.original_info,
                        )),
                        Err(e) => error!("Failed to remove source directory {dir:?}: {e}"),
                    }

                    return Status::CallAgain;
                }
                None => return Status::Done,
            };

            if *src == *dst {
                info!("Skipping no-op move for {src:?}");
                progress.push_outcome(Outcome::Skip);
                continue;
            }

            if !src.exists() {
                error!("Could not move {src:?} as it no longer exists");
                // Doesn't count as a skip? or should it?
                continue;
            }

            break (src, dst);
        };

        // Symlinks get treated as files in prepare_copymove, so don't handle them here.
        if src.is_dir() && !src.is_symlink() && !dst.exists() {
            // Attempt a rename with no fallbacks.
            // This will be fast enough to just try synchronously.
            let source = gio::File::for_path(&src);
            let dest = gio::File::for_path(&dst);
            let flags = FileCopyFlags::NOFOLLOW_SYMLINKS
                | FileCopyFlags::ALL_METADATA
                | FileCopyFlags::NO_FALLBACK_FOR_MOVE;

            let start = Instant::now();
            match source.move_(&dest, flags, Some(&self.cancellable), None) {
                Ok(_) => {
                    debug!("Moved directory {src:?} via rename {:?}", start.elapsed());
                    self.progress
                        .borrow_mut()
                        .push_outcome(Outcome::Move { source: src, dest: dst });
                    return Status::CallAgain;
                }
                Err(e) => {
                    trace!(
                        "Failed to rename directory in {:?}, falling back to normal move: {e}",
                        start.elapsed()
                    );
                }
            }
        }

        let prep = match self.prepare_copymove(src, dst) {
            CopyMovePrep::Asking => return Status::AsyncScheduled,
            CopyMovePrep::Ready(prep) => prep,
            CopyMovePrep::Abort(e) => {
                show_error(e);
                self.cancel();
                return Status::Done;
            }
            CopyMovePrep::CallAgain => return Status::CallAgain,
        };


        #[cfg(feature = "debug-forced-slow")]
        {
            use std::time::Duration;
            let s = self.clone();
            glib::timeout_add_local_once(Duration::from_secs(1), move || s.do_move(prep));
        }
        #[cfg(not(feature = "debug-forced-slow"))]
        self.do_move(prep);

        Status::AsyncScheduled
    }

    fn do_move(self: &Rc<Self>, prep: ReadyCopyMove) {
        let ReadyCopyMove { dst, src, overwrite } = prep;

        let source = gio::File::for_path(&src);
        let dest = gio::File::for_path(&dst);
        let s = self.clone();

        let mut flags = FileCopyFlags::NOFOLLOW_SYMLINKS;
        if overwrite {
            flags |= FileCopyFlags::OVERWRITE;
        }

        source.move_async(
            &dest,
            flags,
            glib::Priority::LOW,
            Some(&self.cancellable),
            None,
            move |result| {
                if let Err(e) = result {
                    if !s.cancellable.is_cancelled() {
                        show_error(format!("{e}, aborting operation"));
                        s.cancel();
                    }
                } else {
                    trace!("Finished moving {src:?} to {dst:?}");
                    s.progress.borrow_mut().push_outcome(Outcome::Move { source: src, dest: dst });
                }

                s.process_next()
            },
        );
    }

    fn process_next_copy(self: &Rc<Self>, dest: &Path) -> Status {
        let (src, dst) = loop {
            let (src, mut dst) = match self.progress.borrow_mut().next_copymove_pair(dest) {
                Some(NextCopyMove::Files(src, dst)) => (src, dst),
                Some(NextCopyMove::FinishedDir(dir)) => {
                    dir.apply_info();
                    return Status::CallAgain;
                }
                None => return Status::Done,
            };

            if *src == *dst {
                let Some(new) = self.progress.borrow_mut().new_name_for(&dst, Fragment::Copy)
                else {
                    return Status::Done;
                };
                dst = new;
            }

            if !src.exists() {
                error!("Could not copy {src:?} as it no longer exists");
                // Doesn't count as a skip for now.
                continue;
            }

            break (src, dst);
        };


        let prep = match self.prepare_copymove(src, dst) {
            CopyMovePrep::Asking => return Status::AsyncScheduled,
            CopyMovePrep::Ready(prep) => prep,
            CopyMovePrep::Abort(e) => {
                show_error(e);
                self.cancel();
                return Status::Done;
            }
            CopyMovePrep::CallAgain => return Status::CallAgain,
        };


        #[cfg(feature = "debug-forced-slow")]
        {
            use std::time::Duration;
            let s = self.clone();
            glib::timeout_add_local_once(Duration::from_secs(1), move || s.do_copy(prep));
        }
        #[cfg(not(feature = "debug-forced-slow"))]
        self.do_copy(prep);

        Status::AsyncScheduled
    }

    fn do_copy(self: &Rc<Self>, ReadyCopyMove { dst, src, overwrite }: ReadyCopyMove) {
        let source = gio::File::for_path(&src);
        let dest = gio::File::for_path(&dst);
        let s = self.clone();

        let mut flags = FileCopyFlags::NOFOLLOW_SYMLINKS;
        if overwrite {
            flags |= FileCopyFlags::OVERWRITE;
        }

        source.copy_async(
            &dest,
            flags,
            glib::Priority::LOW,
            Some(&self.cancellable),
            None,
            move |result| {
                if let Err(e) = result {
                    if !s.cancellable.is_cancelled() {
                        show_error(format!("{e}, aborting operation"));
                        s.cancel();
                    }
                } else {
                    trace!("Finished copying {src:?} to {dst:?}");
                    if overwrite {
                        s.progress.borrow_mut().push_outcome(Outcome::CopyOverwrite(dst));
                    } else {
                        s.progress.borrow_mut().push_outcome(Outcome::Copy(dst));
                    }
                }

                s.process_next();
            },
        );
    }

    fn prepare_copymove(self: &Rc<Self>, src: Arc<Path>, mut dst: Arc<Path>) -> CopyMovePrep {
        if src.is_dir() && !src.is_symlink() {
            return self.prepare_dest_dir(src, dst);
        }

        let mut progress = self.progress.borrow_mut();
        let mut overwrite = false;

        if dst.exists() {
            if !dst.is_file() {
                // Could potentially allow more nuance here.
                return CopyMovePrep::Abort(format!(
                    "Tried to {} {src:?} onto non-file {dst:?}, aborting",
                    self.kind
                ));
            }

            match progress.file_strat() {
                FileCollision::Ask => {
                    debug!("Conflict: {src:?}, existing file {dst:?}");
                    progress.conflict = Some(Conflict { kind: ConflictKind::FileFile, src, dst });
                    drop(progress);
                    gui_run(|g| AskDialog::show(g, self.clone()));

                    return CopyMovePrep::Asking;
                }
                FileCollision::Overwrite => {
                    trace!("Overwriting target file {dst:?}");
                    overwrite = true;
                }
                FileCollision::Rename => {
                    let Some(new) = progress.new_name_for(&dst, self.kind.rename_fragment()) else {
                        return CopyMovePrep::Abort(format!("Failed to find new name for {src:?}"));
                    };
                    dst = new;
                }
                FileCollision::Newer => {
                    let Ok(src_m) = src.metadata() else {
                        info!("Skipping {} of {src:?}, couldn't get mtime", self.kind);
                        progress.push_outcome(Outcome::Skip);
                        return CopyMovePrep::CallAgain;
                    };
                    let Ok(dst_m) = dst.metadata() else {
                        info!("Skipping {} of {src:?}, couldn't get destination mtime", self.kind);
                        progress.push_outcome(Outcome::Skip);
                        return CopyMovePrep::CallAgain;
                    };
                    match (src_m.modified(), dst_m.modified()) {
                        (Ok(s), Ok(d)) if s > d => {
                            info!("Overwriting older destination file {dst:?}");
                            overwrite = true;
                        }
                        _ => {
                            info!("Skipping {} of {src:?}, {dst:?} is newer", self.kind);
                            progress.push_outcome(Outcome::Skip);
                            return CopyMovePrep::CallAgain;
                        }
                    }
                }
                FileCollision::Skip => {
                    info!("Skipping {} of {src:?} to existing {dst:?}", self.kind.str());
                    progress.push_outcome(Outcome::Skip);
                    return CopyMovePrep::CallAgain;
                }
            }
        }

        CopyMovePrep::Ready(ReadyCopyMove { src, dst, overwrite })
    }

    fn prepare_dest_dir(self: &Rc<Self>, src: Arc<Path>, dst: Arc<Path>) -> CopyMovePrep {
        let mut progress = self.progress.borrow_mut();


        let already_existed = dst.exists();

        if already_existed {
            if !dst.is_dir() {
                // Could potentially allow more nuance here.
                return CopyMovePrep::Abort(format!(
                    "Tried to {} {src:?} onto non-folder {dst:?}, aborting",
                    self.kind
                ));
            }

            match progress.directory_strat() {
                DirectoryCollision::Ask => {
                    debug!("Conflict: {src:?}, existing directory {dst:?}");
                    progress.conflict = Some(Conflict { kind: ConflictKind::DirDir, src, dst });
                    drop(progress);
                    gui_run(|g| AskDialog::show(g, self.clone()));

                    return CopyMovePrep::Asking;
                }
                DirectoryCollision::Merge => {
                    debug!("Merging {src:?} into existing directory {dst:?}");
                    progress.push_outcome(Outcome::MergeDestDir(dst.clone()));
                }
                DirectoryCollision::Skip => {
                    debug!("Skipping {} of directory {src:?} since {dst:?} exists", self.kind);
                    progress.push_outcome(Outcome::Skip);
                    return CopyMovePrep::CallAgain;
                }
            }
        } else {
            // Just do all this synchronously, it'll be fine.
            trace!("Creating destination directory {dst:?}");


            if let Err(e) = std::fs::create_dir(&dst) {
                show_error(format!("Failed to create destination directory {dst:?}: {e}"));
                return CopyMovePrep::CallAgain;
            }

            progress.push_outcome(Outcome::CreateDestDir(dst.clone()));
        }


        let source = gio::File::for_path(&src);

        // Should only fail if cancelled, which we aren't doing.
        let attributes = match source.build_attribute_list_for_copy(
            FileCopyFlags::NOFOLLOW_SYMLINKS | FileCopyFlags::ALL_METADATA,
            Cancellable::NONE,
        ) {
            Ok(attr) => attr,
            Err(e) => {
                return CopyMovePrep::Abort(format!(
                    "Failed to read source directory {src:?}: {e}"
                ));
            }
        };

        let original_info = match source.query_info(
            &attributes,
            FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Cancellable::NONE,
        ) {
            Ok(info) => info,
            Err(e) => {
                return CopyMovePrep::Abort(format!(
                    "Failed to read source directory {source:?}: {e}"
                ));
            }
        };

        trace!("Entering source directory {src:?}, destination: {dst:?}");
        let read_dir = match std::fs::read_dir(&src) {
            Ok(read_dir) => read_dir,
            Err(e) => {
                return CopyMovePrep::Abort(format!(
                    "Failed to {} directory {src:?}: {e}",
                    self.kind
                ));
            }
        };


        progress.push_dest_dir(DestinationDirectory {
            source: SourceDirectory { abs_path: src, iter: read_dir },
            dest: dst,
            original_info,
            already_existed,
        });
        CopyMovePrep::CallAgain
    }

    fn process_next_trash(self: &Rc<Self>) -> Status {
        let mut progress = self.progress.borrow_mut();
        let next = match progress.next_remove() {
            Some(NextRemove::File(p)) => p,
            Some(NextRemove::FinishedDir(_dir)) => unreachable!(),
            None => return Status::Done,
        };

        #[cfg(feature = "debug-forced-slow")]
        {
            use std::time::Duration;
            let s = self.clone();
            glib::timeout_add_local_once(Duration::from_secs(1), move || s.do_trash(next));
        }
        #[cfg(not(feature = "debug-forced-slow"))]
        self.do_trash(next);

        Status::AsyncScheduled
    }

    fn do_trash(self: &Rc<Self>, path: Arc<Path>) {
        let s = self.clone();
        gio::File::for_path(&path).trash_async(
            glib::Priority::LOW,
            Some(&self.cancellable),
            move |result| {
                if let Err(e) = result {
                    if !s.cancellable.is_cancelled() {
                        show_warning(format!("{e}"));
                        // Could choose to keep going, but probably too niche
                        s.cancel();
                    }
                } else {
                    trace!("Finished trashing {path:?}");
                    s.progress.borrow_mut().push_outcome(Outcome::Trash);
                }
                s.process_next();
            },
        );
    }

    fn process_next_delete(self: &Rc<Self>) -> Status {
        let mut progress = self.progress.borrow_mut();
        let (next, was_dir) = match progress.next_remove() {
            Some(NextRemove::File(p)) => {
                if p.is_dir() && !p.is_symlink() {
                    let iter = match std::fs::read_dir(&p) {
                        Ok(iter) => iter,
                        Err(e) => {
                            show_error(format!(
                                "Could not read directory {p:?}, aborting delete: {e}"
                            ));
                            self.cancel();
                            return Status::Done;
                        }
                    };
                    progress.push_removal_dir(SourceDirectory { abs_path: p, iter });
                    return Status::CallAgain;
                }

                (p, false)
            }
            Some(NextRemove::FinishedDir(dir)) => (dir.abs_path, true),
            None => return Status::Done,
        };


        #[cfg(feature = "debug-forced-slow")]
        {
            use std::time::Duration;
            let s = self.clone();
            glib::timeout_add_local_once(Duration::from_secs(1), move || {
                s.do_delete(next, was_dir)
            });
        }
        #[cfg(not(feature = "debug-forced-slow"))]
        self.do_delete(next, was_dir);

        Status::AsyncScheduled
    }

    fn do_delete(self: &Rc<Self>, path: Arc<Path>, was_dir: bool) {
        let s = self.clone();
        gio::File::for_path(&path).delete_async(
            glib::Priority::LOW,
            Some(&self.cancellable),
            move |result| {
                if let Err(e) = result {
                    if !s.cancellable.is_cancelled() {
                        show_warning(format!("{e}"));
                        // Could choose to keep going, but probably too niche
                        s.cancel();
                    }
                } else {
                    trace!("Finished deleting {path:?}");
                    // TODO -- remove this comment once I'm confident in the other silly rename
                    // handling.
                    //
                    // Just deleted something, if it's on NFS a silly rename could happen.
                    // There's a tiny chance this is wrong but it's unlikely for a deletion to be
                    // reversed by a creation instantly.
                    //
                    // For a search-only deletion and silly rename this won't be enough, but that's
                    // enough of an edge case to not be a major concern.
                    //
                    // gui_run(|g| g.handle_update(GuiAction::Update(Update::Removed(path))));

                    s.progress.borrow_mut().push_outcome(if was_dir {
                        Outcome::DeleteDir
                    } else {
                        Outcome::Delete
                    });
                }
                s.process_next();
            },
        );
    }

    fn process_rename(self: &Rc<Self>, new_path: &Arc<Path>) -> Status {
        if new_path.exists() {
            show_warning(format!("{new_path:?} already exists"));
            self.progress.borrow_mut().push_outcome(Outcome::Skip);
            return Status::Done;
        }

        let source = self.progress.borrow_mut().pop_source().unwrap();
        let dest = new_path.clone();

        let s = self.clone();
        gio::File::for_path(&source).move_async(
            &gio::File::for_path(new_path),
            FileCopyFlags::NOFOLLOW_SYMLINKS | FileCopyFlags::ALL_METADATA,
            glib::Priority::LOW,
            Some(&self.cancellable),
            None,
            move |result| {
                if let Err(e) = result {
                    if !s.cancellable.is_cancelled() {
                        show_error(format!("{e}"));
                        s.cancel();
                    }
                } else {
                    trace!("Finished renameing {source:?} to {dest:?}");
                    s.progress.borrow_mut().push_outcome(Outcome::Move { source, dest });
                }

                gui_run(|g| g.finish_operation(&s))
            },
        );

        Status::AsyncScheduled
    }

    fn process_make_dir(self: &Rc<Self>, new_path: &Arc<Path>) -> Status {
        if new_path.exists() {
            show_warning(format!("{new_path:?} already exists"));
            self.progress.borrow_mut().push_outcome(Outcome::Skip);
            return Status::Done;
        }

        match std::fs::create_dir(new_path) {
            Ok(_) => {
                trace!("Created directory {new_path:?}");
                self.progress
                    .borrow_mut()
                    .push_outcome(Outcome::CreateDestDir(new_path.clone()));
            }
            Err(e) => {
                show_warning(format!("Failed to create {new_path:?}: {e}"));
                self.progress.borrow_mut().push_outcome(Outcome::Skip);
            }
        }

        Status::Done
    }

    fn process_make_file(self: &Rc<Self>, new_path: &Arc<Path>) -> Status {
        if new_path.exists() {
            show_warning(format!("{new_path:?} already exists"));
            self.progress.borrow_mut().push_outcome(Outcome::Skip);
            return Status::Done;
        }

        match std::fs::File::create(new_path) {
            Ok(_) => {
                trace!("Created file {new_path:?}");
                self.progress.borrow_mut().push_outcome(Outcome::NewFile(new_path.clone()));
            }
            Err(e) => {
                show_warning(format!("Failed to create {new_path:?}: {e}"));
                self.progress.borrow_mut().push_outcome(Outcome::Skip);
            }
        }

        Status::Done
    }

    fn cancel(self: &Rc<Self>) {
        info!("Cancelling operation {:?}", self.kind);
        // Nothing was done for these, cancel them
        self.progress.borrow_mut().conflict.take();
        self.cancellable.cancel();
        gui_run(|g| g.finish_operation(self));
    }
}

impl Gui {
    // This is idempotent and can be called multiple times depending on exactly when an operation
    // is cancelled.
    fn finish_operation(self: &Rc<Self>, finished: &Rc<Operation>) {
        finished.progress.borrow_mut().close();

        tabs_run(|tlist| tlist.scroll_to_completed(finished));

        let mut ops = self.ongoing_operations.borrow_mut();
        let Some(index) = ops.iter().position(|o| Rc::ptr_eq(o, finished)) else {
            return;
        };
        let finished = ops.swap_remove(index);

        if matches!(finished.kind, Kind::Undo { .. }) {
            error!("TODO -- Redo log")
        } else {
            let mut finished_operations = self.finished_operations.borrow_mut();
            if finished_operations.len() >= OPERATIONS_HISTORY {
                finished_operations.pop_front();
            }
            finished_operations.push_back(finished);
        }
    }

    pub(super) fn undo_operation(self: &Rc<Self>) {
        let Some(op) = self.finished_operations.borrow_mut().pop_back() else {
            return info!("Undo called with no completed operations");
        };

        if let Some(limit) = CONFIG.max_undo_minutes {
            // This must be set by close()
            if op.progress.borrow().done_time.unwrap().elapsed()
                > Duration::from_secs(limit.get() * 60)
            {
                info!("Last operation {:?} was too old to undo", op.kind);
                self.warning(format!(
                    "All operations are too old to undo: last operation was {:?}",
                    op.kind
                ));
                // All other operations are at least as old as the most recent
                self.finished_operations.borrow_mut().clear();
                return;
            }
        }

        if !op.progress.borrow().has_any_undoable() {
            info!("Last operation {:?} had nothing to undo", op.kind);
            self.warning(format!("Nothing to undo with last operation {:?}", op.kind));
            // TODO -- redo
            return;
        }

        let tab = op.tab;
        let kind = Kind::Undo {
            prev: op,
            pending_dir_info: RefCell::default(),
        };

        let op = Operation::new(tab, kind, VecDeque::new()).unwrap();
        self.ongoing_operations.borrow_mut().push(op);
    }

    pub(super) fn start_operation(
        self: &Rc<Self>,
        tab: TabId,
        kind: Kind,
        files: VecDeque<Arc<Path>>,
    ) {
        let Some(op) = Operation::new(tab, kind, files) else {
            return error!("Failed to start operation");
        };

        self.ongoing_operations.borrow_mut().push(op);
    }

    pub(super) fn cancel_operations(&self) {
        self.ongoing_operations.take().into_iter().for_each(|op| op.cancel());
    }
}
