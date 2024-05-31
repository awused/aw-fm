use std::cmp::min;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use once_cell::sync::Lazy;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use rayon::{ThreadPool, ThreadPoolBuilder};
use regex::Regex;
use tokio::sync::oneshot::{self, Receiver};

use crate::com::CompletionResult;
use crate::gui::TabId;
use crate::handle_panic;
use crate::natsort::{lowercase, normalize_lowercase};

static COMPLETION_POOL: Lazy<ThreadPool> = Lazy::new(|| {
    ThreadPoolBuilder::new()
        .thread_name(|u| format!("completion-{u}"))
        .panic_handler(handle_panic)
        .num_threads(4)
        .build()
        .expect("Error creating completion threadpool")
});

// Priorities:
// 0 - exact match
// 1 - starts with the fragment (TODO -- consider smart case here?)
// 2 - first occurence is a word boundary
//
// --- If there's only one candidate with priority < 3, it will be chosen
// 3 - anywhere else
// TODO (4+) fuzzy matches?
type Candidate = (PathBuf, i32);

fn next_candidates(
    candidates: Vec<Candidate>,
    fragment: &OsStr,
    require_dir: bool,
    cancel: &AtomicBool,
) -> Vec<Vec<Candidate>> {
    let lower = lowercase(fragment);
    let filter = normalize_lowercase(&lower);
    // We escape the filter, so this should never fail
    let pattern = Regex::new(&format!(r#"((?-u:\b)|_)?{}"#, regex::escape(&filter))).unwrap();

    candidates
        .into_par_iter()
        .take_any_while(|_| !cancel.load(Ordering::Relaxed))
        .filter_map(|(dir, _)| {
            dir.read_dir()
                .and_then(|rd| {
                    rd.take_while(|_| !cancel.load(Ordering::Relaxed))
                        .collect::<Result<Vec<_>, _>>()
                })
                .ok()
        })
        .take_any_while(|_| !cancel.load(Ordering::Relaxed))
        .filter(|entries| !entries.is_empty())
        .map(|entries| {
            entries
                .into_par_iter()
                .filter(|de| !require_dir || de.file_type().map_or(false, |ft| ft.is_dir()))
                .filter_map(|de| {
                    let path = de.path();
                    let binding = lowercase(path.file_name()?);
                    let normalized = normalize_lowercase(&binding);

                    let caps = pattern.captures(&normalized)?;
                    let p = if normalized.len() == filter.len() {
                        0
                    } else if caps.get(0).unwrap().start() == 0 {
                        1
                    } else if caps.get(1).is_some() {
                        2
                    } else {
                        3
                    };

                    Some((path, p))
                })
                .collect::<Vec<_>>()
        })
        .filter(|entries| !entries.is_empty())
        .collect::<Vec<_>>()
}

fn extend_root(
    root: &mut PathBuf,
    fragment: &OsStr,
    final_segment: bool,
    unflattened: Vec<Vec<Candidate>>,
) -> Vec<Candidate> {
    if unflattened.is_empty() {
        root.push(fragment);
        Vec::new()
    } else if unflattened.len() == 1 && unflattened[0].len() == 1 {
        root.clone_from(&unflattened[0][0].0);
        unflattened.into_iter().next().unwrap()
    } else if unflattened.len() == 1 {
        let flattened = unflattened.into_iter().next().unwrap();

        let mut high_priority = flattened.iter().filter(|(_, p)| *p < 3);
        if let Some(hp) = high_priority.next() {
            if high_priority.next().is_none() {
                // Only one high priority -> pick that
                root.clone_from(&hp.0);
            } else if final_segment {
                // If it's matching the last segment, and we have multiple options, try finding
                // the longest prefix because it's probably what I want.

                let prefix =
                    flattened.iter().filter(|(_, p)| *p < 2).fold(None::<&[u8]>, |acc, (f, _)| {
                        let name = f.file_name().unwrap().as_bytes();

                        if let Some(pref) = acc {
                            for i in 0..min(pref.len(), name.len()) {
                                if name[i] != pref[i] {
                                    return Some(&name[0..i]);
                                }
                            }
                            Some(&name[0..min(pref.len(), name.len())])
                        } else {
                            Some(name)
                        }
                    });

                *root = flattened[0].0.parent().unwrap().to_path_buf();
                if let Some(prefix) = prefix {
                    let prefix = OsStr::from_bytes(prefix);
                    if prefix.len() >= fragment.len() && prefix.to_str().is_some() {
                        // Check to_str to avoid appending garbage
                        root.push(prefix);
                    } else {
                        root.push(fragment);
                    }
                } else {
                    root.push(fragment);
                }
            } else {
                *root = flattened[0].0.parent().unwrap().to_path_buf();
                root.push(fragment);
            }
        } else {
            *root = flattened[0].0.parent().unwrap().to_path_buf();
            root.push(fragment);
        }

        flattened
    } else {
        let flattened = unflattened.into_iter().flatten().collect::<Vec<_>>();

        let mut high_priority = flattened.iter().filter(|(_, p)| *p < 3);
        if let Some(hp) = high_priority.next() {
            if high_priority.next().is_none() {
                // Only one high priority -> pick that
                root.clone_from(&hp.0);
            } else {
                root.push(fragment);
            }
        } else {
            root.push(fragment);
        }

        flattened
    }
}


pub(super) fn complete(
    path: PathBuf,
    initial: String,
    tab: TabId,
) -> (Receiver<CompletionResult>, Arc<AtomicBool>) {
    let start = Instant::now();
    let (send, recv) = oneshot::channel();
    let c = Arc::new(AtomicBool::default());

    let cancel = c.clone();
    let complete = move || {
        let mut root = path.as_path();
        if root.exists() {
            return None;
        }

        let mut fragments = Vec::new();

        while !root.exists() {
            fragments.push(root.file_name()?);
            root = root.parent()?;
        }

        if cancel.load(Ordering::Relaxed) || !root.is_dir() {
            return None;
        }

        let mut root = root.to_path_buf();
        let mut candidates = vec![(root.clone(), 0)];

        while let Some(fragment) = fragments.pop() {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }

            if candidates.is_empty() {
                root.push(fragment);
                continue;
            }


            let unflattened = next_candidates(
                candidates,
                fragment,
                /* require_dir= */ fragments.is_empty(),
                &cancel,
            );

            candidates = extend_root(
                &mut root,
                fragment,
                /* last_segment= */ fragments.is_empty(),
                unflattened,
            );
        }

        let target = if candidates.len() == 1 { candidates.pop().unwrap().0 } else { root };

        if target != path {
            Some(CompletionResult { tab, initial, target })
        } else {
            None
        }
    };

    COMPLETION_POOL.spawn(move || {
        let out = complete();

        if let Some(out) = out {
            debug!(
                "Completion suggested {:?} for {:?} in {:?}",
                out.target,
                out.initial,
                start.elapsed()
            );
            drop(send.send(out));
        }
    });

    (recv, c)
}
