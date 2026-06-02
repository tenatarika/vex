use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebouncedEvent};

use crate::index::pipeline;
use crate::parse::language::Language;

const DEBOUNCE_MS: u64 = 500;

/// Watch a project directory for changes and trigger incremental re-indexing.
/// Blocks until SIGINT (Ctrl+C).
///
/// ## v1.12.0 H10 — UX hardening
///
/// Three behaviours that the original implementation got wrong under
/// real-world editing patterns are pinned here:
///
/// 1. **Batch coalescing.** The debouncer collapses rapid-fire events
///    within a 500 ms window into one delivery, but the mpsc channel
///    in front of it queues those deliveries while a long re-index is
///    in flight. Without draining, every queued delivery triggers a
///    redundant `pipeline::update`. After `rx.recv()` we `try_iter()`
///    every pending delivery and merge them — N debouncer batches
///    become one update.
///
/// 2. **`.gitignore` re-eval.** The relevance filter previously only
///    accepted source-file events (by extension). A change to
///    `.gitignore` itself would be dropped, so an un-ignored file
///    would stay invisible until the next *source* edit happened to
///    nudge `pipeline::update` into re-walking. We now treat
///    `.gitignore` (and any nested `.gitignore`) as a relevant event,
///    which calls into `pipeline::update`'s `discover_files`
///    path — the `ignore::WalkBuilder` honours the freshly-edited
///    rules on every call.
///
/// 3. **New-dir re-arm.** notify's `RecursiveMode::Recursive` only
///    recurses *at watch time* on the inotify backend (Linux); new
///    sub-directories created during the watch session are invisible.
///    `Create(Folder)` events now call back into the debouncer's
///    inner watcher to add the new directory. macOS FSEvents and
///    Windows ReadDirectoryChangesW both auto-recurse, so the
///    re-arm is a no-op there but the call is harmless.
pub fn watch(
    root: &Path,
    opts: pipeline::IndexOptions,
    embedder_id: &str,
    excludes: &[String],
) -> Result<()> {
    let root = root.canonicalize().context("canonicalize root")?;

    println!("Building initial index...");
    let (count, _rebuilt) = pipeline::run(&root, opts, embedder_id, excludes)?;
    println!(
        "Watching {} ({count} symbols). Press Ctrl+C to stop.",
        root.display()
    );

    let (tx, rx) = mpsc::channel();

    let mut debouncer = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: std::result::Result<Vec<DebouncedEvent>, Vec<notify::Error>>| match result {
            Ok(events) => {
                let _ = tx.send(events);
            }
            Err(errors) => {
                for e in errors {
                    eprintln!("Watch error: {e}");
                }
            }
        },
    )
    .context("create file watcher")?;

    debouncer
        .watch(&root, RecursiveMode::Recursive)
        .context("start watching")?;

    // H10 fix 3 follow-up (rust-reviewer N8) — track every path we've
    // armed so repeated `Create(Folder)` events for the same path don't
    // call `debouncer.watch(p, …)` on every batch. The upstream
    // `notify-debouncer-full::watch` runs an O(subtree) `WalkDir` inside
    // `FileIdMap::add_path` each time it's invoked (its own `data.roots`
    // dedupe runs AFTER that walk), so long sessions that keep recreating
    // the same scratch dir would do real work on every re-arm. Track our
    // own armed set so we short-circuit before the upstream call.
    let mut armed_dirs: HashSet<PathBuf> = HashSet::new();
    armed_dirs.insert(root.clone());

    while let Ok(events) = rx.recv() {
        // H10 fix 1 — drain every queued debouncer batch and merge them
        // before reacting. Avoids N redundant updates when the user
        // saves several files in rapid succession or while a long
        // initial update is still running.
        let mut all_events: Vec<DebouncedEvent> = events;
        while let Ok(more) = rx.try_recv() {
            all_events.extend(more);
        }

        // Evict `Remove(Folder)`'d paths from `armed_dirs` BEFORE the
        // re-arm loop so a `delete-then-recreate` scratch-dir pattern
        // (rust-reviewer SHOULD-FIX) lets the re-arm fire again on the
        // re-created path. Without this, `armed_dirs.insert` returns
        // `false` on the recreate and the new directory silently stays
        // un-watched for the rest of the session.
        for gone_dir in extract_removed_directories(&all_events) {
            armed_dirs.remove(&gone_dir);
        }

        // H10 fix 3 — re-arm notify on every newly-created directory.
        // On the inotify backend (Linux) `RecursiveMode::Recursive`
        // does not auto-watch subdirs that didn't exist when `watch`
        // was first called. Re-arming the inner watcher is idempotent
        // on backends that already cover this (FSEvents, RDCW), so
        // the call is safe to make unconditionally — but we still
        // dedupe against `armed_dirs` so we don't pay the upstream
        // `FileIdMap::add_path` walk on a path we've already armed
        // (rust-reviewer N8).
        for new_dir in extract_new_directories(&all_events) {
            if !armed_dirs.insert(new_dir.clone()) {
                continue;
            }
            if let Err(e) = debouncer.watch(&new_dir, RecursiveMode::Recursive) {
                eprintln!(
                    "Watch error: failed to arm new directory {}: {e}",
                    new_dir.display()
                );
                // Roll the path back out of `armed_dirs` so a retry
                // (e.g. the dir was momentarily missing) can still
                // succeed on a later batch.
                armed_dirs.remove(&new_dir);
            }
        }

        if !is_event_batch_relevant(&all_events) {
            continue;
        }

        let start = std::time::Instant::now();
        match pipeline::update(&root, opts, embedder_id, excludes) {
            Ok((total, changed, deleted)) => {
                if changed > 0 || deleted > 0 {
                    println!(
                        "[{:.1?}] Updated: {changed} changed, {deleted} deleted, {total} total",
                        start.elapsed()
                    );
                }
            }
            Err(e) => {
                eprintln!("Update error: {e:#}");
            }
        }
    }

    Ok(())
}

/// True when any event in `events` should cause a re-index. Recognises
/// (a) source files with a known language extension, and (b) any path
/// named `.gitignore` (H10 fix 2 — without this, un-ignoring a file via
/// `.gitignore` edit had no effect until an unrelated source change
/// happened to trigger an update).
fn is_event_batch_relevant(events: &[DebouncedEvent]) -> bool {
    events.iter().any(|e| {
        e.event.paths.iter().any(|p| {
            is_source_path(p)
                || matches!(p.file_name().and_then(|n| n.to_str()), Some(".gitignore"))
        })
    })
}

/// `true` when `p` ends in an extension recognised by [`Language::from_extension`].
fn is_source_path(p: &Path) -> bool {
    p.extension()
        .and_then(|ext| ext.to_str())
        .and_then(Language::from_extension)
        .is_some()
}

/// Collect every distinct path from `Create(Folder)` events. Used by
/// fix 3 to re-arm the watcher on subdirectories that did not exist
/// when the initial recursive watch was installed.
fn extract_new_directories(events: &[DebouncedEvent]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for e in events {
        if !matches!(
            e.event.kind,
            EventKind::Create(notify::event::CreateKind::Folder)
        ) {
            continue;
        }
        for p in &e.event.paths {
            if p.is_dir() && !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    out
}

/// Collect every distinct path from `Remove(Folder)` events. Used to
/// evict armed-set entries so the recreate-after-delete scratch-dir
/// pattern still re-arms on the recreated path. `is_dir()` is
/// intentionally NOT checked — by the time the event reaches us the
/// directory is already gone.
fn extract_removed_directories(events: &[DebouncedEvent]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for e in events {
        if !matches!(
            e.event.kind,
            EventKind::Remove(notify::event::RemoveKind::Folder)
        ) {
            continue;
        }
        for p in &e.event.paths {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, Event, RemoveKind};
    use notify_debouncer_full::DebouncedEvent;
    use std::time::Instant;

    fn evt(kind: EventKind, paths: Vec<PathBuf>) -> DebouncedEvent {
        DebouncedEvent {
            event: Event {
                kind,
                paths,
                attrs: Default::default(),
            },
            time: Instant::now(),
        }
    }

    #[test]
    fn source_paths_are_relevant() {
        let e = evt(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            vec![PathBuf::from("src/lib.rs")],
        );
        assert!(is_event_batch_relevant(&[e]));
    }

    #[test]
    fn non_source_paths_are_not_relevant() {
        let e = evt(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            vec![PathBuf::from("README.md.swp"), PathBuf::from("target/x")],
        );
        // .md is a known language, .swp is not. The path is `README.md.swp`
        // which has extension `swp` (since extension takes the last
        // component); not recognised. Use a clear non-source path:
        let e2 = evt(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            vec![PathBuf::from("target/debug/binary")],
        );
        assert!(!is_event_batch_relevant(&[e, e2]));
    }

    #[test]
    fn gitignore_changes_are_relevant() {
        // H10 fix 2 — `.gitignore` edits must cause a re-index even when
        // no source file changes in the same batch. Without this, un-
        // ignoring a file would stay invisible until the next unrelated
        // source edit happened to trigger an update.
        let e = evt(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            vec![PathBuf::from(".gitignore")],
        );
        assert!(is_event_batch_relevant(&[e]));
    }

    #[test]
    fn nested_gitignore_changes_are_relevant() {
        // The walker honours nested `.gitignore` files too; an edit to
        // one should re-eval the discovery for the same reason.
        let e = evt(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            vec![PathBuf::from("crates/foo/.gitignore")],
        );
        assert!(is_event_batch_relevant(&[e]));
    }

    #[test]
    fn extract_new_directories_filters_to_folder_creates() {
        let dir = tempfile::tempdir().unwrap();
        let new_sub = dir.path().join("subproject");
        std::fs::create_dir_all(&new_sub).unwrap();

        let create_dir = evt(EventKind::Create(CreateKind::Folder), vec![new_sub.clone()]);
        let create_file = evt(
            EventKind::Create(CreateKind::File),
            vec![dir.path().join("foo.rs")],
        );
        let modify = evt(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            vec![dir.path().join("bar.rs")],
        );

        let dirs = extract_new_directories(&[create_dir, create_file, modify]);
        assert_eq!(dirs, vec![new_sub]);
    }

    #[test]
    fn extract_new_directories_skips_already_deleted_paths() {
        // The Create(Folder) event might fire just before the dir is
        // removed (rapid cleanup). `is_dir()` returns false on a
        // missing path so the re-arm call is skipped — `notify::watch`
        // would otherwise return an error we'd have to log and swallow.
        let phantom = std::env::temp_dir().join("vex_h10_definitely_not_a_real_dir_xxx");
        let _ = std::fs::remove_dir_all(&phantom); // ensure missing

        let create = evt(EventKind::Create(CreateKind::Folder), vec![phantom]);
        let dirs = extract_new_directories(&[create]);
        assert!(
            dirs.is_empty(),
            "missing path must not be passed to watch()"
        );
    }

    #[test]
    fn extract_new_directories_dedupes_repeated_paths() {
        let dir = tempfile::tempdir().unwrap();
        let new_sub = dir.path().join("subproject");
        std::fs::create_dir_all(&new_sub).unwrap();

        let e1 = evt(EventKind::Create(CreateKind::Folder), vec![new_sub.clone()]);
        let e2 = evt(EventKind::Create(CreateKind::Folder), vec![new_sub.clone()]);
        let dirs = extract_new_directories(&[e1, e2]);
        assert_eq!(dirs.len(), 1, "duplicate dir must be deduped: {dirs:?}");
    }

    /// Pins the rust-reviewer SHOULD-FIX from v1.12.0 final review:
    /// `Remove(Folder)` events must be surfaced so the event loop can
    /// evict them from `armed_dirs`, otherwise a delete-then-recreate
    /// scratch-dir pattern silently leaves the recreated path
    /// un-armed for the rest of the session.
    #[test]
    fn extract_removed_directories_filters_to_folder_removes() {
        let gone = PathBuf::from("/tmp/vex_h10_phantom_dir");
        let remove_dir = evt(EventKind::Remove(RemoveKind::Folder), vec![gone.clone()]);
        let remove_file = evt(
            EventKind::Remove(RemoveKind::File),
            vec![PathBuf::from("/tmp/foo.rs")],
        );
        let create_dir = evt(
            EventKind::Create(CreateKind::Folder),
            vec![PathBuf::from("/tmp/other")],
        );

        let dirs = extract_removed_directories(&[remove_dir, remove_file, create_dir]);
        assert_eq!(dirs, vec![gone]);
    }

    /// The eviction helper must dedupe within a single batch — repeated
    /// `Remove(Folder)` events for the same path should yield one entry,
    /// mirroring `extract_new_directories_dedupes_repeated_paths`.
    #[test]
    fn extract_removed_directories_dedupes_repeated_paths() {
        let gone = PathBuf::from("/tmp/vex_h10_phantom_dup");
        let e1 = evt(EventKind::Remove(RemoveKind::Folder), vec![gone.clone()]);
        let e2 = evt(EventKind::Remove(RemoveKind::Folder), vec![gone.clone()]);
        let dirs = extract_removed_directories(&[e1, e2]);
        assert_eq!(
            dirs.len(),
            1,
            "duplicate Remove(Folder) must dedupe: {dirs:?}"
        );
    }
}
