use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::os::unix::prelude::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use ahash::AHashMap;
use gtk::glib::{self, Object, SourceId};
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;

use super::ask::{DirChoice, FileChoice};
use super::{
    Conflict, ConflictKind, Directory, Fragment, NextCopyMove, NextRemove, Operation, Outcome,
};
use crate::config::{DirectoryCollision, FileCollision, CONFIG};
use crate::gui::{gui_run, show_warning};

#[derive(Debug)]
pub struct Progress {
    dirs: Vec<Directory>,
    // Set for every operation except undo, which plays back outcomes instead.
    source_files: VecDeque<Arc<Path>>,

    log: Vec<Outcome>,

    finished: usize,
    // Would be nice to compute this more eagerly so it gets ahead of the processing
    total: usize,

    pub(super) conflict: Option<Conflict>,

    directory_collisions: DirectoryCollision,
    file_collisions: FileCollision,
    // Used when "ask" is only setting a strategy for the current conflict.
    directory_override_next: Option<DirectoryCollision>,
    file_override_next: Option<FileCollision>,

    // Maps prefix + to last highest existing number
    collision_cache: AHashMap<(OsString, OsString), u64>,

    update_timeout: Option<SourceId>,
    tracker: Option<Tracker>,
}

impl Drop for Progress {
    fn drop(&mut self) {
        debug_assert!(self.update_timeout.is_none());
        debug_assert!(self.tracker.is_none());
    }
}

impl Progress {
    pub fn new(w: Weak<Operation>, source_files: VecDeque<Arc<Path>>) -> Self {
        // Show nothing for the first second.
        let update_timeout = glib::timeout_add_local_once(Duration::from_secs(1), move || {
            let Some(op) = w.upgrade() else {
                return show_warning("Operation update triggered after operation finished");
            };
            info!("Displaying progress bar for operation taking longer than one second");

            let ind = Tracker::new(&op);

            let mut prog = op.progress.borrow_mut();
            prog.update_timeout.take();
            prog.tracker = Some(ind);
        });

        Self {
            source_files,
            dirs: Vec::new(),

            log: Vec::new(),

            total: 0,
            finished: 0,

            conflict: None,
            directory_collisions: CONFIG.directory_collisions,
            file_collisions: CONFIG.file_collisions,
            directory_override_next: None,
            file_override_next: None,

            collision_cache: AHashMap::default(),

            update_timeout: Some(update_timeout),
            tracker: None,
        }
    }

    pub fn close(&mut self) {
        if let Some(ind) = self.tracker.take() {
            ind.parent().and_downcast::<gtk::Box>().unwrap().remove(&ind);
        }

        if let Some(timeout) = self.update_timeout.take() {
            timeout.remove();
        }

        self.collision_cache = AHashMap::new();
    }

    pub fn log(&self) -> &[Outcome] {
        &self.log
    }

    pub(super) fn push_dir(&mut self, dir: Directory) {
        self.dirs.push(dir);
    }

    pub fn pop_source(&mut self) -> Option<Arc<Path>> {
        self.source_files.pop_front()
    }

    pub(super) fn next_copymove_pair(&mut self, dest_root: &Path) -> Option<NextCopyMove> {
        if let Some(Conflict { src, dst, .. }) = self.conflict.take() {
            return Some(NextCopyMove::Files(src, dst));
        }

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

        let mut src = self.source_files.pop_front()?;

        while src.file_name().is_none() {
            error!("Tried to move file without filename");
            src = self.source_files.pop_front()?;
        }

        let name = src.file_name().unwrap();
        let dest = dest_root.to_path_buf().join(name);

        Some(NextCopyMove::Files(src, dest))
    }

    pub(super) fn next_remove(&mut self) -> Option<NextRemove> {
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

        Some(NextRemove::File(self.source_files.pop_front()?))
    }

    pub fn push_outcome(&mut self, action: Outcome) {
        match &action {
            Outcome::Move { .. }
            | Outcome::Create(_)
            | Outcome::CopyOverwrite(_)
            | Outcome::Trash
            | Outcome::CreateDestDir(_)
            | Outcome::MergeDestDir(_) // does this really count?
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

    pub(super) fn new_name_for(&mut self, path: &Path, fragment: Fragment) -> Option<PathBuf> {
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
            let n: u64 = OsStr::from_bytes(&cap[3]).to_string_lossy().parse().unwrap_or(0);
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

        let mut n_available = |n| {
            target.truncate(length);
            target.extend_from_slice(format!(" {n})").as_bytes());
            target.extend_from_slice(suffix.as_bytes());

            let new_path: &Path = Path::new(OsStr::from_bytes(&target));
            if !new_path.exists() {
                return true;
            }
            false
        };

        // Small linear search first, so we avoid easily found gaps
        static MAX_LINEAR: usize = 64;
        for _ in 0..MAX_LINEAR {
            n += 1;

            if n_available(n) {
                self.collision_cache
                    .insert((OsStr::from_bytes(&target[0..length]).into(), suffix.into()), n);
                let new_path = OsString::from_vec(target).into();
                debug!("Found new name {new_path:?} for {path:?}");
                return Some(new_path);
            }
        }

        let mut start = n;
        let mut end = u64::MAX;

        // This is vulnerable to maliciously or mischievously crafted directories where we could
        // believe all numbers are taken based on intentionally seeded files.
        //
        // But that's a stupid, self-sabotaging thing to do.
        while start < end {
            let mid = start + (end - start) / 2;

            if n_available(mid) {
                end = mid;
            } else {
                start = mid + 1;
            }
        }

        if n_available(start) {
            self.collision_cache
                .insert((OsStr::from_bytes(&target[0..length]).into(), suffix.into()), start);
            let new_path = OsString::from_vec(target).into();
            debug!("Found new name {new_path:?} for {path:?}");
            Some(new_path)
        } else {
            None
        }
    }

    pub fn directory_strat(&mut self) -> DirectoryCollision {
        self.directory_override_next.take().unwrap_or(self.directory_collisions)
    }

    pub(super) fn set_directory_strat(&mut self, choice: DirChoice) {
        match choice {
            DirChoice::Skip(true) => {
                self.directory_collisions = DirectoryCollision::Skip;
            }
            DirChoice::Merge(true) => {
                self.directory_collisions = DirectoryCollision::Merge;
            }
            DirChoice::Skip(false) => {
                self.directory_override_next = Some(DirectoryCollision::Skip);
            }
            DirChoice::Merge(false) => {
                self.directory_override_next = Some(DirectoryCollision::Merge);
            }
        }
    }

    pub fn file_strat(&mut self) -> FileCollision {
        self.file_override_next.take().unwrap_or(self.file_collisions)
    }

    pub(super) fn set_file_strat(&mut self, choice: FileChoice) {
        match choice.collision() {
            (true, c) => self.file_collisions = c,
            (false, c) => self.file_override_next = Some(c),
        }
    }

    pub fn conflict_rename(&mut self, name: &str) {
        let c = self.conflict.as_mut().unwrap();
        let Some(parent) = c.dst.parent() else {
            show_warning(format!(
                "Could not rename destination {}, choose a different strategy",
                match c.kind {
                    ConflictKind::DirDir => "directory",
                    ConflictKind::FileFile => "file",
                },
            ));

            return;
        };

        c.dst = parent.join(name);
    }
}

glib::wrapper! {
    pub struct Tracker(ObjectSubclass<imp::Tracker>)
        @extends gtk::Widget, gtk::Window;
}

impl Tracker {
    pub(super) fn new(op: &Rc<Operation>) -> Self {
        let s: Self = Object::new();

        let imp = s.imp();
        imp.operation.set(op.clone()).unwrap();

        imp.title.set_text(op.kind.str());
        imp.subtitle.set_text(&op.kind.dir().to_string_lossy());

        let o = op.clone();
        imp.cancel.connect_clicked(move |_| o.cancel());

        gui_run(|g| g.window.imp().progress_trackers.prepend(&s));

        s
    }
}


mod imp {
    use std::cell::OnceCell;
    use std::rc::Rc;

    use gtk::subclass::prelude::*;
    use gtk::{glib, CompositeTemplate};

    use crate::gui::operations::Operation;

    #[derive(Default, CompositeTemplate)]
    #[template(file = "progress.ui")]
    pub struct Tracker {
        #[template_child]
        pub title: TemplateChild<gtk::Label>,

        #[template_child]
        pub subtitle: TemplateChild<gtk::Label>,

        #[template_child]
        pub current: TemplateChild<gtk::Label>,

        #[template_child]
        pub cancel: TemplateChild<gtk::Button>,

        pub operation: OnceCell<Rc<Operation>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Tracker {
        type ParentType = gtk::Box;
        type Type = super::Tracker;

        const NAME: &'static str = "AwFmProgressTracker";

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Tracker {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl BoxImpl for Tracker {}
    impl WidgetImpl for Tracker {}
}
