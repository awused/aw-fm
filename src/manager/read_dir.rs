use std::any::Any;
use std::ffi::OsString;
use std::fs::{DirEntry, Metadata};
use std::future::ready;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self};
use std::time::{Duration, SystemTime};

use constants::*;
use futures_util::StreamExt;
use gtk::glib::{self, GStr};
use gtk::prelude::FileExt;
use once_cell::sync::Lazy;
use rayon::prelude::{ParallelBridge, ParallelIterator};
use rayon::{ThreadPool, ThreadPoolBuilder};
use tokio::select;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::spawn_local;
use tokio::time::{sleep, sleep_until, Instant};

use super::Manager;
use crate::com::{DirSnapshot, Entry, EntryObject, GuiAction, SnapshotKind};
use crate::natsort::ParsedString;
use crate::{closing, handle_panic};

#[cfg(not(feature = "debug-forced-slow"))]
mod constants {
    use std::time::Duration;

    pub static FAST_TIMEOUT: Duration = Duration::from_millis(1000);
    // Aim to send batches at least this large to the gui.
    // Subsequent batches grow larger to avoid taking quadratic time.
    pub static INITIAL_BATCH: usize = 100;
    // The timeout after which we send a completed batch as soon as no more items are immediately
    // available.
    pub static BATCH_TIMEOUT: Duration = Duration::from_millis(100);
}

// For testing, force directories to load very slowly
#[cfg(feature = "debug-forced-slow")]
mod constants {
    use std::time::Duration;

    pub static FAST_TIMEOUT: Duration = Duration::from_millis(0);
    pub static INITIAL_BATCH: usize = 1;
    pub static BATCH_TIMEOUT: Duration = Duration::from_millis(1000);
}


// Could potentially be a non-rayon threadpool for directory reading and only use rayon for
// individual entries.
static READ_POOL: Lazy<ThreadPool> = Lazy::new(|| {
    ThreadPoolBuilder::new()
        .thread_name(|u| format!("read-dir-{u}"))
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
        .expect("Error creating directory read threadpool")
});

enum ReadResult {
    DirError(std::io::Error),
    EntryError(Arc<Path>, gtk::glib::Error),
    Entry(Entry),
}

pub const fn full_snap(path: Arc<Path>, entries: Vec<Entry>) -> GuiAction {
    GuiAction::Snapshot(DirSnapshot::new(SnapshotKind::Complete, path, entries))
}

pub const fn start_snap(path: Arc<Path>, entries: Vec<Entry>) -> GuiAction {
    GuiAction::Snapshot(DirSnapshot::new(SnapshotKind::Start, path, entries))
}

pub const fn mid_snap(path: Arc<Path>, entries: Vec<Entry>) -> GuiAction {
    GuiAction::Snapshot(DirSnapshot::new(SnapshotKind::Middle, path, entries))
}

pub const fn end_snap(path: Arc<Path>, entries: Vec<Entry>) -> GuiAction {
    GuiAction::Snapshot(DirSnapshot::new(SnapshotKind::End, path, entries))
}


fn read_dir_sync(path: Arc<Path>, sender: UnboundedSender<ReadResult>) -> oneshot::Receiver<()> {
    let (send_done, recv_done) = oneshot::channel();

    READ_POOL.spawn(move || {
        let rdir = match std::fs::read_dir(&path) {
            Ok(rdir) => rdir,
            Err(e) => {
                error!("Unexpected error opening directory {path:?} {e}");
                drop(sender.send(ReadResult::DirError(e)));
                return;
            }
        };

        rdir.take_while(|_| !closing::closed()).par_bridge().for_each(|dirent| {
            let dirent = match dirent {
                Ok(e) => e,
                Err(e) => {
                    error!("Unexpected error reading directory {path:?} {e}");
                    drop(sender.send(ReadResult::DirError(e)));
                    return;
                }
            };

            let entry = match Entry::new(dirent.path().into()) {
                Ok(entry) => entry,
                Err((path, e)) => {
                    error!("Unexpected error reading file info {path:?} {e}");
                    drop(sender.send(ReadResult::EntryError(path, e)));
                    return;
                }
            };

            if sender.send(ReadResult::Entry(entry)).is_err() && !closing::closed() {
                error!("Channel unexpectedly closed while reading directory");
                closing::close();
            }
        });

        let _ignored = send_done.send(());
    });

    recv_done
}

async fn read_dir_sync_thread(path: Arc<Path>, gui_sender: glib::Sender<GuiAction>) {
    debug!("Starting to read directory {path:?}");

    let start = Instant::now();
    let mut entries = Vec::new();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let h = read_dir_sync(path.clone(), sender);

    // If we don't load everything in one second, it is a slow directory
    let fast_deadline = Instant::now() + FAST_TIMEOUT;

    select! {
        biased;
        _ = closing::closed_fut() => drop(receiver),
        _ = async {
            while let Some(r) = receiver.recv().await {
                match r {
                    ReadResult::Entry(ent) => {
                        entries.push(ent);
                    }
                    ReadResult::DirError(e) => {
                        todo!()
                    }
                    ReadResult::EntryError(p, e) => {
                        todo!()
                    }
                }
            }
        } => {
            trace!("Fast directory completed in {:?} with {} entries", start.elapsed(), entries.len());
            // Send off a full snapshot
            if let Err(e) = gui_sender.send(full_snap(path.clone(), entries)) {
                if !closing::closed() {
                    error!("{e}");
                }
                return
            }
        }
        _ = sleep_until(fast_deadline) => {
            trace!("Starting slow directory handling at {} entries", entries.len());

            if let Err(e) = gui_sender.send(start_snap(path.clone(), entries)) {
                if !closing::closed() {
                    error!("{e}");
                }
                return;
            }

            // Send off an initial batch, the gui may elect not to show anything if it's tiny
            read_slow_dir(path.clone(), receiver, gui_sender).await;
        }
    };

    // Technically this blocks, but it's more a formality by this point
    h.await.unwrap();
    debug!("Done reading directory {:?} in {:?}", path, start.elapsed());
}

async fn read_slow_dir(
    path: Arc<Path>,
    mut receiver: UnboundedReceiver<ReadResult>,
    gui_sender: glib::Sender<GuiAction>,
) {
    let start = Instant::now();
    let mut batch_size = INITIAL_BATCH;
    let mut next_size = INITIAL_BATCH;

    loop {
        let batch_start = Instant::now();
        let mut batch = Vec::new();
        let mut deadline_passed = false;

        select! {
            _ = closing::closed_fut() => return drop(receiver),
            done = async {
                loop {
                    let Some(r) = receiver.recv().await else {
                        break true;
                    };

                    match r {
                        ReadResult::Entry(ent) => {
                            batch.push(ent);
                            if batch.len() >= batch_size {
                                break false;
                            }
                        }
                        ReadResult::DirError(e) => {
                            todo!()
                        }
                        ReadResult::EntryError(p, e) => {
                            todo!()
                        }
                    }
                }
            } => {
                if done {
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

                    if let Err(e) = gui_sender.send(end_snap(path, batch)) {
                        if !closing::closed() {
                            error!("{e}");
                        }
                    }

                    return
                }
            }
        }

        let batch_deadline = Instant::now() + BATCH_TIMEOUT;

        #[cfg(feature = "debug-forced-slow")]
        {
            sleep_until(batch_deadline).await;

            if let Err(e) = gui_sender.send(mid_snap(path.clone(), batch)) {
                if !closing::closed() {
                    error!("{e}");
                }
                return;
            }

            next_size += batch_size;
            batch_size = next_size - batch_size;
            continue;
        }

        // Allow up to 5ms between entries, until we hit batch_deadline, then only consume
        // immediately ready values. This avoids the case where we get multiple small batches in a
        // row.
        'batch: loop {
            select! {
                biased;
                done = async {
                    match receiver.recv().await {
                        None => true,
                        Some(ReadResult::Entry(ent)) => {
                            batch.push(ent);
                            false
                        }
                        Some(ReadResult::DirError(e)) => {
                            todo!()
                        }
                        Some(ReadResult::EntryError(p, e)) => {
                            todo!()
                        }
                    }
                } => {
                    if done {
                        trace!(
                            "Slow directory done in {:?}/{:?} final batch: {}/{batch_size}",
                            batch_start.elapsed(),
                            start.elapsed(),
                            batch.len()
                        );
                        if let Err(e) = gui_sender.send(end_snap(path, batch)) {
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

                    if let Err(e) = gui_sender.send(mid_snap(path.clone(), batch)) {
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

                    if let Err(e) = gui_sender.send(mid_snap(path.clone(), batch)) {
                        if !closing::closed() {
                            error!("{e}");
                        }
                        return
                    }

                    break 'batch;
                }
            }
        }


        next_size += batch_size;
        batch_size = next_size - batch_size;
    }
}


impl Manager {
    pub(super) fn start_read_dir(&self, path: Arc<Path>) {
        // Assume we don't need to worry about cancellation for this
        spawn_local(read_dir_sync_thread(path, self.gui_sender.clone()));
    }
}
