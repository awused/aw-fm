use std::cmp::min;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use once_cell::sync::Lazy;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use rayon::slice::ParallelSliceMut;
use rayon::{ThreadPool, ThreadPoolBuilder};
use regex::Regex;
use tokio::sync::oneshot::{self, Receiver};

use crate::com::CompletionResult;
use crate::config::CONFIG;
use crate::gui::TabId;
use crate::handle_panic;
use crate::natsort::{lowercase, normalize_lowercase};

static COMPLETION_POOL: Lazy<ThreadPool> = Lazy::new(|| {
    ThreadPoolBuilder::new()
        .thread_name(|u| format!("completion-{u}"))
        .panic_handler(handle_panic)
        // 8 threads as this hits diminishing returns later
        // We're not doing much I/O per directory or file, so even 16 isn't completely wasted
        .num_threads(8)
        .build()
        .expect("Error creating completion threadpool")
});

// Priorities:
// 0 - exact match
// 1 - starts with the fragment (TODO -- consider smart case here?)
// 2 - first occurence is a word boundary
//
// 3 - anywhere else
// TODO (4+) fuzzy matches?
type Candidate = (u64, PathBuf);
// Later path segments get more significant bits in the final priority
// Bigger numbers -> lower priority
const SHIFT: usize = 2;
const START_MATCH: u64 = 1 << (64 - SHIFT);
const WORD_START_MATCH: u64 = 2 << (64 - SHIFT);
const MIDDLE_MATCH: u64 = 3 << (64 - SHIFT);

fn next_candidates(
    candidates: Vec<Candidate>,
    fragment: &OsStr,
    final_segment: bool,
    cancel: &AtomicBool,
) -> Vec<Vec<Candidate>> {
    let lower = lowercase(fragment);
    let filter = normalize_lowercase(&lower);
    // We escape the filter, so this should never fail
    let pattern = Regex::new(&format!(r#"((?-u:\b)|_)?{}"#, regex::escape(&filter))).unwrap();
    let hidden = CONFIG.search_show_all;

    candidates
        .into_par_iter()
        .filter_map(|(p, dir)| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }

            dir.read_dir()
                .and_then(|rd| {
                    rd.take_while(|_| !cancel.load(Ordering::Relaxed))
                        .collect::<Result<Vec<_>, _>>()
                })
                .ok()
                .map(|entries| (p, entries))
        })
        .take_any_while(|_| !cancel.load(Ordering::Relaxed))
        .filter(|(_, entries)| !entries.is_empty())
        .map(|(p, entries)| {
            let parent_priority = p >> SHIFT;

            entries
                .into_par_iter()
                .take_any_while(|_| !cancel.load(Ordering::Relaxed))
                .filter(|de| {
                    (hidden || !de.file_name().as_bytes().first().is_some_and(|b| *b == b'.'))
                        && (final_segment || de.file_type().is_ok_and(|ft| ft.is_dir()))
                })
                .filter_map(|de| {
                    if cancel.load(Ordering::Relaxed) {
                        return None;
                    }

                    let path = de.path();

                    if fragment.is_empty() {
                        return Some((parent_priority, path));
                    }

                    let lower = lowercase(path.file_name()?);
                    let normalized = normalize_lowercase(&lower);

                    let mut midword_match = false;
                    for cap in pattern.captures_iter(&normalized) {
                        if normalized.len() == filter.len() {
                            return Some((parent_priority, path));
                        } else if cap.get(0).unwrap().start() == 0 {
                            return Some((parent_priority | START_MATCH, path));
                        } else if cap.get(1).is_some() {
                            return Some((parent_priority | WORD_START_MATCH, path));
                        }

                        midword_match = true;
                    }

                    midword_match.then_some((parent_priority | MIDDLE_MATCH, path))
                })
                .collect::<Vec<_>>()
        })
        .filter(|entries| !entries.is_empty())
        .collect::<Vec<_>>()
}

fn flatten_candidates(
    fragment: &OsStr,
    final_segment: bool,
    grouped: Vec<Vec<Candidate>>,
) -> Vec<Candidate> {
    let single_parent = grouped.len() == 1;
    let mut flattened = if single_parent {
        grouped.into_iter().next().unwrap()
    } else {
        grouped.into_iter().flatten().collect::<Vec<_>>()
    };

    if single_parent && final_segment {
        let prefix =
            flattened.iter().filter(|(p, _)| *p < 2).try_fold(&[] as &[u8], |acc, (p, f)| {
                if *p == 0 {
                    // If there is an exact match, this is a waste of time.
                    return Err(());
                }

                let name = f.file_name().unwrap().as_bytes();

                for i in 0..min(acc.len(), name.len()) {
                    if name[i] != acc[i] {
                        return Ok(&name[0..i]);
                    }
                }
                Ok(&name[0..min(acc.len(), name.len())])
            });

        if let Ok(prefix) = prefix {
            let prefix = OsStr::from_bytes(prefix);
            if !prefix.is_empty() && prefix.len() >= fragment.len() && prefix.to_str().is_some() {
                let mut root = flattened[0].1.parent().unwrap().to_path_buf();
                root.push(prefix);
                // We're going to sort by priority and we don't have any other 0s
                flattened.push((0, root));
            }
        }
    }

    flattened
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

        let mut fragments = Vec::new();

        // Must do this at least once so /exact/path can match /exact/path2 as well
        while {
            let fragment = root.file_name()?;
            fragments.push(fragment);
            let bytes = root.as_os_str().as_bytes();
            for i in (0..(bytes.len().saturating_sub(fragment.as_bytes().len() + 1))).rev() {
                if bytes[i].is_ascii()
                    && std::path::is_separator(char::from_u32(bytes[i] as _).unwrap())
                {
                    fragments.push(OsStr::new(""));
                } else {
                    break;
                }
            }

            root = root.parent()?;

            !root.exists()
        } {}

        if cancel.load(Ordering::Relaxed)
            || !root.is_dir()
            || fragments.len() > CONFIG.search_max_depth.unwrap_or(255) as usize
        {
            return None;
        }

        let mut candidates = vec![(0, root.to_path_buf())];

        while let Some(fragment) = fragments.pop() {
            if candidates.is_empty() || cancel.load(Ordering::Relaxed) {
                return None;
            }

            let final_segment = fragments.is_empty();

            let unflattened = next_candidates(candidates, fragment, final_segment, &cancel);

            if cancel.load(Ordering::Relaxed) {
                return None;
            }

            candidates = flatten_candidates(fragment, final_segment, unflattened);
        }

        if candidates.is_empty()
            || (candidates.len() == 1 && candidates[0].1 == path)
            || cancel.load(Ordering::Relaxed)
        {
            return None;
        }

        candidates.as_mut_slice().par_sort();
        let mut candidates: Vec<_> = candidates.into_iter().map(|(_, p)| p).collect();


        // Allow the user to cycle back to whatever they initially entered
        if candidates[0].to_string_lossy() != initial {
            candidates.push(initial.clone().into());
        }

        let position = if candidates[0] == path { 1 } else { 0 };

        Some(CompletionResult { tab, initial, candidates, position })
    };

    COMPLETION_POOL.spawn(move || {
        if let Some(out) = complete() {
            debug!(
                "Completion for {:?} suggested {} items in {:?}: {:?}",
                out.initial,
                out.candidates.len(),
                start.elapsed(),
                &out.candidates[0..min(out.candidates.len(), 20)],
            );
            drop(send.send(out));
        } else {
            debug!("Completion terminated with no result in {:?}", start.elapsed());
        }
    });

    (recv, c)
}
