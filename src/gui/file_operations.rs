// Until all the rest are implemented
#![allow(unused)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{remove_dir, ReadDir};
use std::os::unix::prelude::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use ahash::AHashMap;
use gtk::gio::{self, Cancellable, FileCopyFlags, FileInfo, FileQueryInfoFlags};
use gtk::glib::{self, SourceId};
use gtk::prelude::{CancellableExt, FileExt, FileExtManual};
use once_cell::unsync::Lazy;
use regex::bytes::{Captures, Match, Regex};

use super::tabs::id::TabId;
use super::{gui_run, Gui};
use crate::com::Update::Removed;
use crate::com::{GuiAction, Update};
use crate::config::{DirectoryCollision, FileCollision, CONFIG};
use crate::gui::{show_error, show_warning, tabs_run};

thread_local! {
    static COPY_REGEX: Lazy<Regex> = Lazy::new(||Regex::new(r"^(.*)( \(copy (\d+)\))(\.[^/]+)?$").unwrap());
    static COPIED_REGEX: Lazy<Regex> = Lazy::new(||Regex::new(r"^(.*)( \(copied (\d+)\))(\.[^/]+)?$").unwrap());
    static MOVED_REGEX: Lazy<Regex> = Lazy::new(||Regex::new(r"^(.*)( \(moved (\d+)\))(\.[^/]+)?$").unwrap());
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
    // Includes overwrites, undo -> move back
    Move { source: Arc<Path>, dest: PathBuf },
    // Does not include overwrite copies, undo -> delete with no confirmation
    Create(PathBuf),
    // Only overwrites from copy, undo -> delete with confirmation
    CopyOverwrite(PathBuf),
    // FileInfo needs to be restored after we populate the contents, which is awkward.
    // Could unconditionally store FileInfo to restore it, probably not worth it.
    RemoveSourceDir(Arc<Path>, Option<FileInfo>),
    CreateDestDir(PathBuf),
    Skip,
    Delete,
    DeleteDir,
    // Not undoable without dumb hacks: https://gitlab.gnome.org/GNOME/glib/-/issues/845
    Trash,
}

#[derive(Debug)]
pub enum Kind {
    Move(Arc<Path>),
    Copy(Arc<Path>),
    Rename(PathBuf),

    // In theory, at least, it should be possible to redo an undo.
    // Probably won't support this, but keep the skeleton intact.
    Undo {
        prev: Box<Self>,
        prev_progress: Box<RefCell<Progress>>,
        // These should be processed FILO, just like outcomes from progress.log
        pending_dir_info: RefCell<Vec<(Arc<Path>, FileInfo)>>,
    },
    Trash,
    Delete,
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
            Self::Rename(_) => "rename",
            Self::Undo { .. } => "undo",
            Self::Trash => "trash",
            Self::Delete => "delete",
        }
    }

    const fn rename_fragment(&self) -> Fragment {
        match self {
            Self::Move(_) => Fragment::Moved,
            Self::Copy(_) => Fragment::Copied,
            Self::Rename(_) | Self::Undo { .. } | Self::Trash | Self::Delete => unreachable!(),
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
    abs_path: Arc<Path>,
    // Only set for copy/move
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
    dirs: Vec<Directory>,
    // Set for every operation except undo, which plays back outcomes instead.
    files: VecDeque<Arc<Path>>,

    log: Vec<Outcome>,

    finished: usize,
    // Would be nice to compute this more eagerly so it gets ahead of the processing
    total: usize,

    // For Ask
    pending_pair: Option<Asking>,

    directory_collisions: DirectoryCollision,
    file_collisions: FileCollision,

    // Maps prefix + to last highest existing number
    collision_cache: AHashMap<(OsString, OsString), usize>,

    // process_callback: Option<SourceId>,
    update_timeout: Option<SourceId>,
}

#[derive(Debug)]
enum NextCopyMove {
    FinishedDir(Directory),
    Files(Arc<Path>, PathBuf),
}

#[derive(Debug)]
enum NextRemove {
    FinishedDir(Directory),
    File(Arc<Path>),
}

impl Progress {
    fn next_copymove_pair(&mut self, dest_root: &Path) -> Option<NextCopyMove> {
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

                return Some(NextCopyMove::Files(
                    dir.abs_path.join(&name).into(),
                    dir.dest.join(name),
                ));
            }

            return Some(NextCopyMove::FinishedDir(self.dirs.pop().unwrap()));
        }

        let mut src = self.files.pop_front()?;

        while src.file_name().is_none() {
            error!("Tried to move file without filename");
            src = self.files.pop_front()?;
        }

        let name = src.file_name().unwrap();
        let dest = dest_root.to_path_buf().join(name);

        Some(NextCopyMove::Files(src, dest))
    }

    fn next_remove(&mut self) -> Option<NextRemove> {
        if let Some(dir) = &mut self.dirs.last_mut() {
            for next in dir.iter.by_ref() {
                let path = match next {
                    Ok(de) => de.path(),
                    Err(e) => {
                        show_warning(&format!(
                            "Failed to read contents of directory {:?}: {e}",
                            dir.abs_path
                        ));
                        continue;
                    }
                };

                return Some(NextRemove::File(path.into()));
            }

            return Some(NextRemove::FinishedDir(self.dirs.pop().unwrap()));
        }

        Some(NextRemove::File(self.files.pop_front()?))
    }

    fn push_outcome(&mut self, action: Outcome) {
        match &action {
            Outcome::Move { .. }
            | Outcome::Create(_)
            | Outcome::CopyOverwrite(_)
            | Outcome::Trash
            | Outcome::CreateDestDir(_)
            | Outcome::Delete
            | Outcome::DeleteDir => {
                self.total += 1;
                self.finished += 1
            }
            Outcome::Skip => self.total += 1,
            Outcome::RemoveSourceDir(..) => {}
        }

        self.log.push(action);
    }

    fn new_name_for(&mut self, path: &Path, fragment: Fragment) -> Option<PathBuf> {
        trace!("Finding new name for copy onto self for {path:?}");

        let Some(name) = path.file_name() else {
            show_warning(format!("Can't make new name for {path:?}"));
            return None;
        };

        let Some(parent) = path.parent() else {
            show_warning(format!("Can't make new name for {path:?}"));
            return None;
        };

        let (prefix, suffix, mut n) = if let Some(cap) = fragment.captures(name.as_bytes()) {
            let n: usize = OsStr::from_bytes(&cap[3]).to_string_lossy().parse().unwrap_or(0);
            let prefix = cap.get(1).unwrap().as_bytes();
            let suffix = cap.get(4).map_or(&[][..], |m| m.as_bytes());

            (OsStr::from_bytes(prefix), OsStr::from_bytes(suffix), n)
        } else if let Some(prefix) = path.file_stem() {
            let suffix = path.file_name().unwrap();
            let suffix = &suffix.as_bytes()[prefix.as_bytes().len()..];
            let suffix = OsStr::from_bytes(suffix);

            (prefix, suffix, 0)
        } else {
            (name, OsStr::new(""), 0)
        };

        let target = parent.join(prefix);
        let mut target = target.into_os_string();
        target.push(" (");
        target.push(fragment.str());


        // Wasteful allocations, but by this point we're already doing I/O
        if let Some(old_n) = self.collision_cache.get(&(target.clone(), suffix.to_os_string())) {
            n = *old_n;
        }


        let mut target = target.into_vec();
        let length = target.len();

        // Could do a gallop search here to avoid the worst case, at the cost of potentially
        // missing gaps we could have used. Probably an unrealistic use case.
        static MAX_LOOPS: usize = 2000;
        for _ in 0..MAX_LOOPS {
            n += 1;

            target.truncate(length);
            target.extend_from_slice(format!(" {n})").as_bytes());
            target.extend_from_slice(suffix.as_bytes());

            let new_path: &Path = Path::new(OsStr::from_bytes(&target));
            if !new_path.exists() {
                debug!("Found new name {new_path:?} for {path:?}");

                self.collision_cache
                    .insert((OsStr::from_bytes(&target[0..length]).into(), suffix.into()), n);
                return Some(OsString::from_vec(target).into());
            }
        }

        None
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
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
    cancellable: Cancellable,
    // Fast operations don't live long enough to display on-screen
    slow: Cell<bool>,
    // Just clone the paths directly instead of needing to convert everything to an Rc up front.
    progress: RefCell<Progress>,
}

struct ReadyCopyMove {
    src: Arc<Path>,
    dst: PathBuf,
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
    fn new(tab: TabId, kind: Kind, files: VecDeque<Arc<Path>>) -> Option<Rc<Self>> {
        if files.is_empty() {
            warn!("Got empty file operation {}, ignoring.", kind.str());
            return None;
        }

        // Abort if any of them are strictly invalid.
        // Moves into the same dir will be skipped later.
        match &kind {
            Kind::Move(p) | Kind::Copy(p) => {
                if let Some(invalid) = files.iter().find(|f| p.starts_with(f)) {
                    show_warning(format!("Invalid {kind} of {invalid:?} into {p:?}"));
                    return None;
                }
            }
            Kind::Rename(p) => {
                if files.len() != 1 {
                    show_warning(format!("Got invalid rename of {} files", files.len()));
                    return None;
                }

                if files[0].parent() != p.parent() {
                    show_warning("Got invalid rename: destination directory not the same");
                    return None;
                }
            }
            Kind::Undo { .. } | Kind::Trash | Kind::Delete => {}
        }


        let rc = Rc::new_cyclic(|weak: &Weak<Self>| {
            // Start with no timeout.
            let w = weak.clone();
            let update_timeout = glib::timeout_add_local_once(Duration::from_secs(1), move || {
                let Some(op) = w.upgrade() else {
                    return;
                };

                op.slow.set(true);

                op.progress.borrow_mut().update_timeout.take();
                error!("TODO -- show progress bar");
            });

            let progress = Progress {
                files,
                dirs: Vec::new(),

                log: Vec::new(),

                total: 0,
                finished: 0,

                pending_pair: None,
                directory_collisions: CONFIG.directory_collisions,
                file_collisions: CONFIG.file_collisions,

                collision_cache: AHashMap::default(),

                update_timeout: Some(update_timeout),
            };

            Self {
                tab,
                cancellable: Cancellable::new(),
                slow: Cell::default(),
                kind,
                progress: RefCell::new(progress),
            }
        });


        let s = rc.clone();
        glib::idle_add_local_once(move || s.process_next());

        Some(rc)
    }

    fn process_next(self: Rc<Self>) {
        if self.cancellable.is_cancelled() {
            info!("Cancelled operation {:?}", self.kind);

            return gui_run(|g| g.finish_operation(self));
        }

        let status = match &self.kind {
            Kind::Move(p) => self.process_next_move(p),
            Kind::Copy(p) => self.process_next_copy(p),
            Kind::Rename(p) => self.process_rename(p),
            Kind::Undo { .. } => todo!(),
            Kind::Trash => self.process_next_trash(),
            Kind::Delete => self.process_next_delete(),
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
            let (src, dst) = match self.progress.borrow_mut().next_copymove_pair(dest) {
                Some(NextCopyMove::Files(src, dst)) => (src, dst),
                Some(NextCopyMove::FinishedDir(dir)) => {
                    dir.apply_info();

                    info!("Removing source directory {dir:?}");
                    match remove_dir(&dir.abs_path) {
                        Ok(_) => self.progress.borrow_mut().push_outcome(Outcome::RemoveSourceDir(
                            dir.abs_path,
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
                self.progress.borrow_mut().push_outcome(Outcome::Skip);
                continue;
            }

            if !src.exists() {
                error!("Could not move {src:?} as it no longer exists");
                // Doesn't count as a skip? or should it?
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
                error!("Could not copy {:?} as it no longer exists", src);
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
            let s = self.clone();
            glib::timeout_add_local_once(Duration::from_secs(1), move || s.do_copy(prep));
        }
        #[cfg(not(feature = "debug-forced-slow"))]
        self.do_copy(prep);

        Status::AsyncScheduled
    }

    fn do_copy(self: &Rc<Self>, prep: ReadyCopyMove) {
        let ReadyCopyMove { dst, src, overwrite } = prep;

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
                        s.progress.borrow_mut().push_outcome(Outcome::Create(dst));
                    }
                }

                s.process_next();
            },
        );
    }

    fn prepare_copymove(self: &Rc<Self>, src: Arc<Path>, mut dst: PathBuf) -> CopyMovePrep {
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

            match progress.file_collisions {
                FileCollision::_Ask => todo!(),
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

    fn prepare_dest_dir(self: &Rc<Self>, src: Arc<Path>, dst: PathBuf) -> CopyMovePrep {
        let mut progress = self.progress.borrow_mut();

        let original_info = if dst.exists() {
            if !dst.is_dir() {
                // Could potentially allow more nuance here.
                return CopyMovePrep::Abort(format!(
                    "Tried to {} {src:?} onto non-folder {dst:?}, aborting",
                    self.kind
                ));
            }

            match progress.directory_collisions {
                DirectoryCollision::_Ask => todo!(),
                DirectoryCollision::Merge => {
                    debug!("Merging {src:?} into existing directory {dst:?}");
                }
                DirectoryCollision::Skip => {
                    debug!("Skipping {} of directory {src:?} since {dst:?} exists", self.kind);
                    progress.push_outcome(Outcome::Skip);
                    return CopyMovePrep::CallAgain;
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
                    return CopyMovePrep::Abort(format!(
                        "Failed to read source directory {src:?}: {e}"
                    ));
                }
            };

            let info = match source.query_info(
                &attributes,
                FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                Cancellable::NONE,
            ) {
                Ok(info) => info,
                Err(e) => {
                    return CopyMovePrep::Abort(format!(
                        "Failed to create destination directory {dst:?}: {e}"
                    ));
                }
            };

            if let Err(e) = std::fs::create_dir(&dst) {
                show_error(format!("Failed to create destination directory {dst:?}: {e}"));
                return CopyMovePrep::CallAgain;
            }

            progress.push_outcome(Outcome::CreateDestDir(dst.clone()));

            Some(info)
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

        progress.dirs.push(Directory {
            abs_path: src,
            dest: dst,
            iter: read_dir,
            original_info,
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
                    progress.dirs.push(Directory {
                        abs_path: p,
                        dest: PathBuf::new(),
                        iter,
                        original_info: None,
                    });
                    return Status::CallAgain;
                }

                (p, false)
            }
            Some(NextRemove::FinishedDir(dir)) => (dir.abs_path, true),
            None => return Status::Done,
        };


        #[cfg(feature = "debug-forced-slow")]
        {
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
                    // Just deleted something, if it's on NFS a silly rename could happen.
                    // There's a tiny chance this is wrong but it's unlikely for a deletion to be
                    // reversed by a creation instantly.
                    //
                    // For a search-only deletion and silly rename this won't be enough, but that's
                    // enough of an edge case to not be a major concern.
                    gui_run(|g| g.handle_update(GuiAction::Update(Update::Removed(path.into()))));

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

    fn process_rename(self: &Rc<Self>, new_path: &Path) -> Status {
        if new_path.exists() {
            show_warning(format!("{new_path:?} already exists"));
            self.progress.borrow_mut().push_outcome(Outcome::Skip);
            return Status::Done;
        }

        let source = self.progress.borrow_mut().files.pop_front().unwrap();
        let dest = new_path.to_path_buf();

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

                gui_run(|g| g.finish_operation(s))
            },
        );

        Status::AsyncScheduled
    }

    fn cancel(&self) {
        info!("Cancelling operation {:?}", self.kind,);
        self.cancellable.cancel();
    }
}

impl Gui {
    fn finish_operation(self: &Rc<Self>, finished: Rc<Operation>) {
        let mut ops = self.ongoing_operations.borrow_mut();
        let Some(index) = ops.iter().position(|o| Rc::ptr_eq(o, &finished)) else {
            return;
        };
        let op = ops.swap_remove(index);

        // Allow file system + notifies to settle for 10 + 2ms
        // We dedupe notifications for at most 10ms, plus some margin
        glib::timeout_add_local_once(Duration::from_millis(12), move || {
            tabs_run(|tlist| {
                tlist.scroll_to_completed(op.tab, &op.kind, &op.progress.borrow().log)
            });
        });
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
