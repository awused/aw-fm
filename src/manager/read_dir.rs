use std::future::ready;
use std::io::ErrorKind;
use std::os::unix::prelude::OsStrExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use constants::*;
use gtk::gio::ffi::G_FILE_TYPE_DIRECTORY;
use gtk::gio::{
    self, Cancellable, FILE_ATTRIBUTE_STANDARD_ALLOCATED_SIZE, FILE_ATTRIBUTE_STANDARD_SIZE,
    FILE_ATTRIBUTE_STANDARD_TYPE, FileQueryInfoFlags,
};
use gtk::glib::GStr;
use gtk::prelude::FileExt;
use ignore::{WalkBuilder, WalkState};
use rayon::slice::ParallelSliceMut;
use rayon::{ThreadPool, ThreadPoolBuilder};
use tokio::select;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::task::spawn_local;
use tokio::time::{Instant, sleep, sleep_until};

use super::Manager;
use crate::com::{
    ChildInfo, DirSnapshot, Entry, GuiAction, SearchSnapshot, SearchUpdate, SnapshotKind,
    SortSettings, Update,
};
use crate::config::CONFIG;
use crate::{closing, handle_panic};

#[cfg(not(feature = "debug-forced-slow"))]
mod constants {
    use std::time::Duration;

    // Experimentally, a directory that takes more than 1s is probably going to take a lot more.
    // The same can be said for 500ms but large NFS directories can get close enough that it
    // sometimes causes two snapshots instead of one.
    pub static FAST_TIMEOUT: Duration = Duration::from_millis(1000);
    // Aim to send batches at least this large to the gui.
    // Subsequent batches grow larger to avoid taking quadratic time.
    //
    // May raise to 10k+ as in practice directories are either very fast or very large and very
    // slow.
    pub static INITIAL_BATCH: usize = 1000;
    // How fast batches grow in size.
    pub static BATCH_GROWTH_FACTOR: f64 = 1.1;
    // The timeout after which we send a completed batch as soon as no more items are immediately
    // available.
    pub static BATCH_TIMEOUT: Duration = Duration::from_millis(1000);
}

// For testing, force directories to load very slowly
#[cfg(feature = "debug-forced-slow")]
mod constants {
    use std::time::Duration;

    pub static FAST_TIMEOUT: Duration = Duration::from_millis(0);
    pub static INITIAL_BATCH: usize = 1;
    pub static BATCH_GROWTH_FACTOR: f64 = 1.1;
    pub static BATCH_TIMEOUT: Duration = Duration::from_millis(2500);
}


// Use a separate pool for directory readers so that large slow directories don't stall out other
// directories, and entries don't end up stalling the ReadDir.
// Still limit how many are read at once instead of using spawn_blocking.
// Rayon is not really being utilized here, but it already exists as a dependency.
static READ_POOL: LazyLock<ThreadPool> = LazyLock::new(|| {
    ThreadPoolBuilder::new()
        .thread_name(|u| format!("read-dir-{u}"))
        .panic_handler(handle_panic)
        .num_threads(4)
        .build()
        .expect("Error creating directory read threadpool")
});


fn spawn_entry_pool() -> ThreadPool {
    ThreadPoolBuilder::new()
        .thread_name(|u| format!("read-entry-{u}"))
        .panic_handler(handle_panic)
        // This shouldn't be too large - the returns diminish fast and there's a risk of
        // overwhelming slow network drives.
        // For benchmarks on one large directory with a warm cache, excluding unusually long runs
        // 1 - 400-500ms
        // 2 - 200-330ms
        // 4 - 140-200ms (180 typical)
        // 8 - 105-140ms
        // 16 - 120-130ms (hitting some kind of limit?)
        // 32 - ~130ms
        .num_threads(4)
        .build()
        .expect("Error creating entry read threadpool")
}

// Keep one reusable ENTRY_POOL for most directory reads.
// In the case this one is in use, we spin up another pool as needed.
static ENTRY_POOL: LazyLock<Mutex<ThreadPool>> = LazyLock::new(|| Mutex::new(spawn_entry_pool()));

static SORT_POOL: LazyLock<ThreadPool> = LazyLock::new(|| {
    ThreadPoolBuilder::new()
        .thread_name(|u| format!("sort-{u}"))
        .panic_handler(handle_panic)
        // 8 threads -> 40k items usually under 6ms
        .num_threads(8)
        .build()
        .expect("Error creating directory sort threadpool")
});

// This will rarely be spun up
static COUNT_POOL: LazyLock<ThreadPool> = LazyLock::new(|| {
    ThreadPoolBuilder::new()
        .thread_name(|u| format!("count-{u}"))
        .panic_handler(handle_panic)
        .num_threads(1)
        .build()
        .expect("Error creating directory contents count threadpool")
});

#[derive(Debug)]
enum ReadResult {
    DirUnreadable(std::io::Error),
    DirError(std::io::Error),
    EntryError(Arc<Path>, gtk::glib::Error),
    Entry(Entry),
}


fn read_dir_sync(
    path: Arc<Path>,
    cancel: Arc<AtomicBool>,
    sender: UnboundedSender<ReadResult>,
    gui_sender: UnboundedSender<GuiAction>,
) -> oneshot::Receiver<()> {
    let (send_done, recv_done) = oneshot::channel();

    READ_POOL.spawn_fifo(move || {
        let rdir = match std::fs::read_dir(&path) {
            Ok(rdir) => rdir,
            Err(e) => {
                error!("Unexpected error opening directory {path:?} {e}");
                drop(sender.send(ReadResult::DirUnreadable(e)));
                return;
            }
        };

        let run_on_pool = |pool: &ThreadPool| {
            pool.in_place_scope(|scope| {
                let cancel = &cancel;
                let path = &path;
                let sender = &sender;
                let gui_sender = &gui_sender;

                rdir.take_while(|_| !cancel.load(Relaxed) && !closing::closed()).for_each(
                    |dirent| {
                        let dirent = match dirent {
                            Ok(e) => e,
                            Err(e) => {
                                error!("Unexpected error reading directory {path:?} {e}");
                                drop(sender.send(ReadResult::DirError(e)));
                                return;
                            }
                        };

                        scope.spawn(move |_| {
                            if cancel.load(Relaxed) || closing::closed() {
                                return;
                            }

                            let (entry, needs_full_count) = match Entry::new(dirent.path().into()) {
                                Ok(entry) => entry,
                                Err((path, e)) => {
                                    error!("Unexpected error reading file info {path:?} {e}");
                                    drop(sender.send(ReadResult::EntryError(path, e)));
                                    return;
                                }
                            };

                            if needs_full_count {
                                flat_dir_count(entry.abs_path.clone(), gui_sender.clone());
                            }

                            if sender.send(ReadResult::Entry(entry)).is_err()
                                && !closing::closed()
                                && !cancel.load(Relaxed)
                            {
                                closing::fatal(format!(
                                    "Channel unexpectedly closed while reading {path:?}"
                                ));
                            }
                        });
                    },
                )
            })
        };

        if let Ok(p) = ENTRY_POOL.try_lock() {
            run_on_pool(&p)
        } else {
            debug!("Spawning new entry pool for {path:?}");
            run_on_pool(&spawn_entry_pool())
        }

        let _ignored = send_done.send(());
    });

    recv_done
}

fn recurse_dir_sync(
    root: Arc<Path>,
    cancel: Arc<AtomicBool>,
    sender: UnboundedSender<ReadResult>,
    gui_sender: UnboundedSender<GuiAction>,
) -> oneshot::Receiver<()> {
    let (send_done, recv_done) = oneshot::channel();

    if Some(0) == CONFIG.search_max_depth {
        let _ignored = send_done.send(());
        return recv_done;
    }

    READ_POOL.spawn(move || {
        let show_all = CONFIG.search_show_all;
        let walker = WalkBuilder::new(&root)
            // Uncertain.
            .follow_links(true)
            .max_depth(CONFIG.search_max_depth.map(|n|n as usize + 1))
            .hidden(!show_all)
            .ignore(!show_all)
            .git_ignore(!show_all)
            .git_global(!show_all)
            .git_exclude(!show_all)
            .parents(!show_all)
            .build_parallel();

        // Must defer
        let (dir_send, mut dir_read) = unbounded_channel();

        walker.run(|| {
            let visitor = |res: Result<ignore::DirEntry, ignore::Error>| {
                if cancel.load(Relaxed) || closing::closed() {
                    return WalkState::Quit;
                }

                let path = match res {
                    Ok(dirent) => dirent.into_path(),
                    Err(e) => {
                        error!("Unexpected error reading directory {root:?}: {e}");
                        if let Some(io) = e.into_io_error() {
                            // Ignore broken symlinks
                            if io.kind() != ErrorKind::NotFound {
                                drop(sender.send(ReadResult::DirError(io)));
                            }
                        }
                        return WalkState::Continue;
                    }
                };

                let Some(parent) = path.parent() else {
                    return WalkState::Continue;
                };

                if root.as_os_str().len() >= parent.as_os_str().len() {
                    return WalkState::Continue;
                }

                // TODO -- move onto a new rayon task to unblock this walker thread?
                let (entry, needs_full_count) = match Entry::new(path.into()) {
                    Ok(entry) => entry,
                    Err((path, e)) => {
                        error!("Unexpected error reading file info {path:?} {e}");
                        drop(sender.send(ReadResult::EntryError(path, e)));
                        return WalkState::Continue;
                    }
                };

                if needs_full_count {
                    dir_send.send(entry.abs_path.clone()).unwrap();
                }

                if sender.send(ReadResult::Entry(entry)).is_err()
                    && !closing::closed()
                    && !cancel.load(Relaxed)
                {
                    closing::fatal(format!(
                        "Channel unexpectedly closed while recursively reading {root:?}"
                    ));
                }
                WalkState::Continue
            };

            Box::new(visitor)
        });

        if !cancel.load(Relaxed) && !closing::closed() {
            while let Ok(dir) = dir_read.try_recv() {
                search_dir_count(dir, gui_sender.clone(), cancel.clone());
            }
        }

        let _ignored = send_done.send(());
    });

    recv_done
}

async fn read_dir(
    path: Arc<Path>,
    cancel: Arc<AtomicBool>,
    sort: SortSettings,
    gui_sender: UnboundedSender<GuiAction>,
) {
    debug!("Starting to read directory {path:?}");

    let start = Instant::now();
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

    let h = read_dir_sync(path.clone(), cancel.clone(), sender, gui_sender.clone());

    consume_entries(path.clone(), cancel.clone(), gui_sender, receiver, flat_snap(sort)).await;

    // Technically this blocks, but it's more a formality by this point. Still want to wait so we
    // can be sure it has been cleaned up.
    let finished = h.await.is_ok();
    debug!(
        "Done reading directory {path:?} in {:?}. finished: {finished}, cancelled: {}",
        start.elapsed(),
        cancel.load(Relaxed)
    );
}

async fn recurse_dir(
    path: Arc<Path>,
    cancel: Arc<AtomicBool>,
    gui_sender: UnboundedSender<GuiAction>,
) {
    debug!("Starting to recursively walk {path:?}");

    let start = Instant::now();
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

    let h = recurse_dir_sync(path.clone(), cancel.clone(), sender, gui_sender.clone());

    consume_entries(path.clone(), cancel.clone(), gui_sender, receiver, search_snap).await;

    let finished = h.await.is_ok();
    debug!(
        "Done recursively walking {path:?} in {:?}. finished: {finished}, cancelled: {}",
        start.elapsed(),
        cancel.load(Relaxed)
    );
}

trait SnapFn: Fn(&Arc<Path>, &Arc<AtomicBool>, SnapshotKind, Vec<Entry>) -> GuiAction {}
impl<T: Fn(&Arc<Path>, &Arc<AtomicBool>, SnapshotKind, Vec<Entry>) -> GuiAction> SnapFn for T {}

fn flat_snap(sort: SortSettings) -> impl SnapFn {
    move |path: &Arc<Path>,
          cancel: &Arc<AtomicBool>,
          kind: SnapshotKind,
          mut entries: Vec<Entry>| {
        if kind.initial() {
            let start = Instant::now();
            SORT_POOL.install(|| {
                entries.par_sort_by(|a, b| a.cmp(b, sort));
            });
            trace!("Optimistically sorted {} items in {:?}", entries.len(), start.elapsed());
        }
        GuiAction::Snapshot(DirSnapshot::new(kind, path, cancel, entries))
    }
}

fn search_snap(
    _path: &Arc<Path>,
    cancel: &Arc<AtomicBool>,
    kind: SnapshotKind,
    entries: Vec<Entry>,
) -> GuiAction {
    GuiAction::SearchSnapshot(SearchSnapshot::new(kind.finished(), cancel.clone(), entries))
}


async fn consume_entries(
    path: Arc<Path>,
    cancel: Arc<AtomicBool>,
    gui_sender: UnboundedSender<GuiAction>,
    mut receiver: UnboundedReceiver<ReadResult>,
    snap: impl SnapFn,
) {
    let start = Instant::now();
    let mut entries = Vec::new();
    // If we don't load everything in one second, it is a slow directory
    let fast_deadline = Instant::now() + FAST_TIMEOUT;

    select! {
        biased;
        _ = closing::closed_fut() => drop(receiver),
        success = async {
            while let Some(r) = receiver.recv().await {
                if cancel.load(Relaxed) || closing::closed() {
                    break;
                }

                match r {
                    ReadResult::Entry(ent) => {
                        entries.push(ent);
                    }
                    ReadResult::DirUnreadable(e) => {
                        if let Err(e) =
                            gui_sender.send(
                                GuiAction::DirectoryOpenError(path.clone(), e.to_string())) {
                            if !closing::closed() {
                                error!("{e}");
                            }
                        }
                        return false;
                    }
                    ReadResult::DirError(e) => drop(
                        gui_sender.send(GuiAction::DirectoryError(path.clone(), e.to_string()))),
                    ReadResult::EntryError(p, e) => drop(
                        gui_sender.send(GuiAction::EntryReadError(path.clone(), p, e.to_string()))),
                }
            }
            !cancel.load(Relaxed) && !closing::closed()
        } => {
            if !success {
                return;
            }

            drop(receiver);
            trace!("Fast directory completed in {:?} with {} entries", start.elapsed(), entries.len());
            // Send off a full snapshot
            if let Err(e) = gui_sender.send(snap(&path, &cancel, SnapshotKind::Complete, entries)) {
                if !closing::closed() {
                    error!("{e}");
                }
            }
        }
        _ = sleep_until(fast_deadline) => {
            trace!("Starting slow directory handling at {} entries", entries.len());

            #[cfg(feature = "debug-forced-slow")]
            {
                sleep(BATCH_TIMEOUT).await;
            }

            if let Err(e) = gui_sender.send(snap(&path, &cancel, SnapshotKind::Start, entries)) {
                if !closing::closed() {
                    error!("{e}");
                }
                return;
            }

            // Send off an initial batch, the gui may elect not to show anything if it's tiny
            read_slow_dir(path.clone(), cancel, receiver, gui_sender, snap).await;
        }
    };
}


// For slow directories we try to keep the UI responsive (unlike nautilus/caja/etc) by sending
// reasonably large batches as they become available.
//
// For increasing batch sizes we:
//   - Consume entries until we meet the minimum batch size
//   - Continue to consume entries until 5ms have passed without a new entry, up to BATCH_TIMEOUT
//   - If BATCH_TIMEOUT has passed, consume only immediately available entrie
//   - Send the batch
#[cfg_attr(feature = "debug-forced-slow", allow(unreachable_code, unused))]
async fn read_slow_dir(
    path: Arc<Path>,
    cancel: Arc<AtomicBool>,
    mut receiver: UnboundedReceiver<ReadResult>,
    sender: UnboundedSender<GuiAction>,
    snap: impl SnapFn,
) {
    #[derive(Eq, PartialEq)]
    enum SlowBatch {
        Completed,
        Failed,
        Incomplete,
    }
    use SlowBatch::*;

    let start = Instant::now();
    let mut batch_size = INITIAL_BATCH;
    let mut true_size = INITIAL_BATCH as f64;

    loop {
        let batch_start = Instant::now();
        let mut batch = Vec::new();
        let mut deadline_passed = false;

        select! {
            _ = closing::closed_fut() => return drop(receiver),
            status = async {
                loop {
                    let Some(r) = receiver.recv().await else {
                        break Completed;
                    };

                    if cancel.load(Relaxed) || closing::closed() {
                        break Failed;
                    }

                    match r {
                        ReadResult::Entry(ent) => {
                            batch.push(ent);
                            if batch.len() >= batch_size {
                                break Incomplete;
                            }
                        }
                        ReadResult::DirUnreadable(e) => {
                            if let Err(e) = sender.send(
                                GuiAction::DirectoryOpenError(path.clone(), e.to_string())) {
                                if !closing::closed() {
                                    error!("{e}");
                                }
                            }
                            break Failed;
                        }
                        ReadResult::DirError(e) => drop(
                            sender.send(GuiAction::DirectoryError(path.clone(), e.to_string()))),
                        ReadResult::EntryError(p, e) => drop(
                            sender.send(GuiAction::EntryReadError(path.clone(), p, e.to_string()))),
                    }
                }
            } => {
                if status == Completed {
                    #[cfg(feature = "debug-forced-slow")]
                    {
                        sleep_until(Instant::now() + BATCH_TIMEOUT).await;
                    }

                    trace!(
                        "Slow directory done in {:?}/{:?} final batch: {}/{batch_size}",
                        batch_start.elapsed(),
                        start.elapsed(),
                        batch.len()
                    );

                    if let Err(e) = sender.send(snap(&path, &cancel, SnapshotKind::End, batch)) {
                        if !closing::closed() {
                            error!("{e}");
                        }
                    }

                    return
                } else if status == Failed {
                    return
                }
            }
        }

        let batch_deadline = Instant::now() + BATCH_TIMEOUT;

        #[cfg(feature = "debug-forced-slow")]
        {
            sleep_until(batch_deadline).await;

            if let Err(e) = sender.send(snap(&path, &cancel, SnapshotKind::Middle, batch)) {
                if !closing::closed() {
                    error!("{e}");
                }
                return;
            }

            true_size *= BATCH_GROWTH_FACTOR;
            batch_size = true_size as usize;
            continue;
        }

        // Allow up to 5ms between entries, until we hit batch_deadline, then only consume
        // immediately ready values. This avoids the case where we get multiple small batches in a
        // row and perpetually stall on sending items to the UI.
        'batch: loop {
            select! {
                biased;
                done = async {
                    match receiver.recv().await {
                        None => {},
                        Some(ReadResult::Entry(ent)) => {
                            batch.push(ent);
                            return false;
                        }
                        Some(ReadResult::DirUnreadable(e)) => {
                            // By now, this case is unreachable.
                            // It only triggers if the dir is completely unreadable.
                            // Since we've loaded at least one item, that can no longer be the
                            // case.
                            unreachable!("DirUnreadable despite reading it. {e}");
                        }
                        Some(ReadResult::DirError(e)) => drop(
                            sender.send(GuiAction::DirectoryError(path.clone(), e.to_string()))),
                        Some(ReadResult::EntryError(p, e)) => drop(
                            sender.send(GuiAction::EntryReadError(path.clone(), p, e.to_string()))),
                    }
                    !cancel.load(Relaxed) && !closing::closed()
                } => {
                    if done {
                        trace!(
                            "Slow directory done in {:?}/{:?} final batch: {}/{batch_size}",
                            batch_start.elapsed(),
                            start.elapsed(),
                            batch.len()
                        );
                        if let Err(e) = sender.send(snap(&path, &cancel, SnapshotKind::End, batch)) {
                            if !closing::closed() {
                                error!("{e}");
                            }
                        }
                        // Send final snapshot
                        return
                    }
                }

                _ = closing::closed_fut() => return drop(receiver),
                _ = sleep_until(batch_deadline), if !deadline_passed => {
                    deadline_passed = true;
                }
                _ = sleep(Duration::from_millis(5)), if !deadline_passed => {
                    trace!(
                        "Slow directory batch done in {:?}/{:?} batch: {}/{batch_size}",
                        batch_start.elapsed(),
                        start.elapsed(),
                        batch.len()
                    );

                    if let Err(e) = sender.send(snap(&path, &cancel, SnapshotKind::Middle, batch)) {
                        if !closing::closed() {
                            error!("{e}");
                        }
                        return
                    }

                    break 'batch;
                },
                _ = ready(()), if deadline_passed => {
                    trace!(
                        "Slow directory batch done in {:?}/{:?} batch: {}/{batch_size}",
                        batch_start.elapsed(),
                        start.elapsed(),
                        batch.len()
                    );

                    if let Err(e) = sender.send(snap(&path, &cancel, SnapshotKind::Middle, batch)) {
                        if !closing::closed() {
                            error!("{e}");
                        }
                        return
                    }

                    break 'batch;
                }
            }
        }


        true_size *= BATCH_GROWTH_FACTOR;
        batch_size = true_size as usize;
    }
}

pub fn flat_dir_count(path: Arc<Path>, gui_sender: UnboundedSender<GuiAction>) {
    count_dir_contents(path, move |entry| {
        drop(gui_sender.send(GuiAction::Update(Update::Entry(entry.into()))))
    });
}

fn search_dir_count(
    path: Arc<Path>,
    gui_sender: UnboundedSender<GuiAction>,
    search_id: Arc<AtomicBool>,
) {
    count_dir_contents(path, move |entry| {
        let update = SearchUpdate {
            search_id,
            update: Update::Entry(entry.into()),
        };
        drop(gui_sender.send(GuiAction::SearchUpdate(update)))
    });
}


fn count_dir_contents(path: Arc<Path>, send_update: impl FnOnce(Entry) + Send + 'static) {
    COUNT_POOL.spawn(move || {
        debug!("Doing full count of files in {path:?}");
        let read_dir = match path.read_dir() {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to count contents of {path:?}: {e}");
                return;
            }
        };

        let count = read_dir.count();
        trace!("Counted {count} entries in {path:?}");

        if let Some(entry) = Entry::new_assume_dir_size(path, count as u64) {
            send_update(entry);
        }
    });
}


struct ChildAccumulator {
    info: ChildInfo,
    cancel: Arc<AtomicBool>,
    sender: UnboundedSender<GuiAction>,
    unsent: usize,
}

impl Drop for ChildAccumulator {
    fn drop(&mut self) {
        if self.unsent != 0 {
            trace!("Sending info about {} unsent children on drop", self.unsent);
            drop(self.sender.send(GuiAction::DirChildren(self.cancel.clone(), self.info)));
        }
    }
}

impl ChildAccumulator {
    fn send(&mut self) {
        // trace!("Sending info about {} children", self.unsent);
        drop(self.sender.send(GuiAction::DirChildren(self.cancel.clone(), self.info)));
        self.unsent = 0;
        self.info = ChildInfo::default();
    }
}

static CHILD_ATTRIBUTES: LazyLock<String> = LazyLock::new(|| {
    [
        FILE_ATTRIBUTE_STANDARD_TYPE,
        FILE_ATTRIBUTE_STANDARD_SIZE,
        FILE_ATTRIBUTE_STANDARD_ALLOCATED_SIZE,
    ]
    .map(GStr::as_str)
    .join(",")
});

fn recurse_children(
    dirs: Vec<Arc<Path>>,
    cancel: Arc<AtomicBool>,
    sender: UnboundedSender<GuiAction>,
) {
    debug!("Recursively measuring children of {} directories", dirs.len());
    let start = Instant::now();

    let dirs: Arc<[Arc<Path>]> = dirs.into();
    let dir_count = dirs.len();
    let max_dir_len = dirs.iter().map(|d| d.as_os_str().as_bytes().len()).max().unwrap();

    READ_POOL.spawn(move || {
        let mut builder = WalkBuilder::new(&dirs[0]);
        builder
            // Uncertain.
            .follow_links(false)
            // Excessive, hopefully, as a backstop
            .max_depth(Some(50))
            .hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false);

        for d in &dirs[1..] {
            builder.add(d);
        }

        let inner_cancel = cancel.clone();
        let inner_sender = sender.clone();
        builder.build_parallel().run(move || {
            let mut acc = ChildAccumulator {
                info: ChildInfo::default(),
                cancel: inner_cancel.clone(),
                sender: inner_sender.clone(),
                unsent: 0,
            };

            let dirs = dirs.clone();
            let visitor = move |res: Result<ignore::DirEntry, ignore::Error>| {
                if acc.cancel.load(Relaxed) || closing::closed() {
                    return WalkState::Quit;
                }

                let path = match res {
                    Ok(dirent) => dirent.into_path(),
                    Err(e) => {
                        error!("Unexpected error reading directory children: {e}");
                        return WalkState::Continue;
                    }
                };

                if path.as_os_str().as_bytes().len() <= max_dir_len
                    && dirs.iter().any(|d| **d == path)
                {
                    return WalkState::Continue;
                }

                let info = match gio::File::for_path(path).query_info(
                    CHILD_ATTRIBUTES.as_str(),
                    FileQueryInfoFlags::empty(),
                    Option::<&Cancellable>::None,
                ) {
                    Ok(info) => info,
                    Err(e) => {
                        error!("Unexpected error reading directory child info: {e}");
                        return WalkState::Continue;
                    }
                };

                let file_type = info.attribute_uint32(FILE_ATTRIBUTE_STANDARD_TYPE);

                acc.info.allocated += info.attribute_uint64(FILE_ATTRIBUTE_STANDARD_ALLOCATED_SIZE);

                if file_type == G_FILE_TYPE_DIRECTORY as u32 {
                    acc.info.dirs += 1;
                } else {
                    acc.info.files += 1;
                    acc.info.size += info.attribute_uint64(FILE_ATTRIBUTE_STANDARD_SIZE);
                }

                acc.unsent += 1;


                if acc.unsent % 1000 == 0 {
                    acc.send();
                }
                WalkState::Continue
            };

            Box::new(visitor)
        });

        drop(sender.send(GuiAction::DirChildren(
            cancel,
            ChildInfo { done: true, ..ChildInfo::default() },
        )));

        trace!(
            "Finished measuring children of {dir_count} directories in {:?}",
            start.elapsed()
        );
    });
}

impl Manager {
    pub(super) fn start_read_dir(
        &self,
        path: Arc<Path>,
        sort: SortSettings,
        cancel: Arc<AtomicBool>,
    ) {
        trace!("Starting to read flat directory {path:?}");
        spawn_local(read_dir(path, cancel, sort, self.gui_sender.clone()));
    }

    pub(super) fn recurse_dir(&self, path: Arc<Path>, cancel: Arc<AtomicBool>) {
        spawn_local(recurse_dir(path, cancel, self.gui_sender.clone()));
    }

    pub(super) fn get_children(&self, dirs: Vec<Arc<Path>>, cancel: Arc<AtomicBool>) {
        recurse_children(dirs, cancel, self.gui_sender.clone());
    }
}
