use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebouncedEvent, Debouncer, RecommendedCache};

use crate::index::pipeline;
use crate::parse::language::Language;

const DEBOUNCE_MS: u64 = 500;

/// One workspace member to watch (multi-repo Phase 7). `root` is the
/// canonical `workspace::Member.root` verbatim — the routing invariant
/// (`event_path.starts_with(root)`) depends on it being the exact path
/// handed to `debouncer.watch` (docs/MULTIREPO-PHASE7.md §9).
pub(crate) struct MemberWatch {
    pub(crate) root: PathBuf,
    pub(crate) display_name: String,
    pub(crate) opts: pipeline::IndexOptions,
    pub(crate) embedder_id: String,
    pub(crate) excludes: Vec<String>,
}

/// Shared event-loop core for single-repo and workspace watch. Owns the
/// debouncer, the delivery channel, and the armed-directory set; `run`
/// drains/merges batches, evicts + re-arms directories, applies the
/// relevance filter, then hands each relevant batch to a caller-supplied
/// `dispatch` (which owns the `pipeline::update` call + its summary print,
/// so the core stays format-agnostic).
///
/// ## v1.12.0 H10 behaviours preserved here
/// 1. **Batch coalescing** — `try_recv`-drain every queued debouncer
///    delivery and merge, so N rapid saves become one dispatch.
/// 2. **`.gitignore` re-eval** — `is_event_batch_relevant` treats any
///    `.gitignore` as relevant (the walker honours fresh rules each call).
/// 3. **New-dir re-arm** — Linux inotify is non-recursive at watch time;
///    `Create(Folder)` re-arms the inner watcher, deduped via `armed_dirs`
///    (the upstream `FileIdMap::add_path` runs an O(subtree) walk).
struct WatchLoop {
    debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    rx: mpsc::Receiver<Vec<DebouncedEvent>>,
    armed_dirs: HashSet<PathBuf>,
    /// The permanent roots passed to `new` (single-repo: one; workspace: the
    /// member roots). Never evicted from `armed_dirs`; re-arm of new dirs is
    /// scoped to descendants of these.
    watched_roots: Vec<PathBuf>,
}

impl WatchLoop {
    /// Create the debouncer and arm every `root` recursively.
    fn new(roots: &[PathBuf]) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut debouncer = new_debouncer(
            Duration::from_millis(DEBOUNCE_MS),
            None,
            move |result: std::result::Result<Vec<DebouncedEvent>, Vec<notify::Error>>| match result
            {
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

        let mut armed_dirs = HashSet::new();
        for r in roots {
            debouncer
                .watch(r, RecursiveMode::Recursive)
                .with_context(|| format!("start watching {}", r.display()))?;
            armed_dirs.insert(r.clone());
        }
        Ok(Self {
            debouncer,
            rx,
            armed_dirs,
            watched_roots: roots.to_vec(),
        })
    }

    /// Block on the watcher until SIGINT, calling `dispatch` once per
    /// relevant (merged) batch. `dispatch` runs on this thread (it is
    /// `FnMut`, NOT `Send`/`'static`) and must handle its own update
    /// errors (log + continue) — single-repo and workspace both want a
    /// transient update failure to keep the watch alive.
    fn run(&mut self, mut dispatch: impl FnMut(&[DebouncedEvent])) {
        while let Ok(events) = self.rx.recv() {
            // H10 fix 1 — drain + merge every queued batch before reacting.
            let mut all_events: Vec<DebouncedEvent> = events;
            while let Ok(more) = self.rx.try_recv() {
                all_events.extend(more);
            }

            // Evict removed dirs BEFORE re-arm so delete-then-recreate
            // re-arms. A permanent watched root (member root) is NEVER
            // evicted — that would silently stop watching one member while
            // the rest keep working; warn instead (set is frozen at start).
            for gone_dir in extract_removed_directories(&all_events) {
                if self.watched_roots.contains(&gone_dir) {
                    tracing::warn!(
                        dir = %gone_dir.display(),
                        "watched root removed; restart `vex watch` to drop it from the set"
                    );
                    continue;
                }
                self.armed_dirs.remove(&gone_dir);
            }

            // H10 fix 3 — re-arm new dirs, scoped to descendants of a
            // watched root (skip workspace-root / between-member scratch so
            // we don't pay the upstream O(subtree) walk on un-owned trees).
            for new_dir in extract_new_directories(&all_events) {
                if !self.watched_roots.iter().any(|r| new_dir.starts_with(r)) {
                    continue;
                }
                if !self.armed_dirs.insert(new_dir.clone()) {
                    continue;
                }
                if let Err(e) = self.debouncer.watch(&new_dir, RecursiveMode::Recursive) {
                    eprintln!(
                        "Watch error: failed to arm new directory {}: {e}",
                        new_dir.display()
                    );
                    self.armed_dirs.remove(&new_dir);
                }
            }

            if !is_event_batch_relevant(&all_events) {
                continue;
            }
            dispatch(&all_events);
        }
    }
}

/// Watch a single project directory for changes and trigger incremental
/// re-indexing. Blocks until SIGINT (Ctrl+C). The batch-coalescing /
/// `.gitignore` re-eval / new-dir re-arm behaviours (v1.12.0 H10) live in
/// [`WatchLoop`]; this is a thin wrapper that supplies a whole-root
/// `pipeline::update` dispatch.
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

    let mut watch_loop = WatchLoop::new(std::slice::from_ref(&root))?;
    watch_loop.run(|_batch| {
        // Whole-root update — single-repo ignores per-path routing.
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
    });

    Ok(())
}

/// `vex watch --workspace` (multi-repo Phase 7): build each member's initial
/// index, then watch every member root from one debouncer, routing a changed
/// file to its OWNING member's incremental update. All-or-nothing on the
/// initial build (matches `index --workspace`); the member set is frozen at
/// startup. Concurrency: each `pipeline::update(m.root)` takes its own
/// per-root `IndexLock` and writes atomically (`.tmp` → rename), so a
/// concurrent cross-process reader is safe — no new machinery here.
pub(crate) fn watch_workspace(members: Vec<MemberWatch>) -> Result<()> {
    if members.is_empty() {
        anyhow::bail!("workspace declares no members to watch");
    }

    println!("Building initial indexes for {} members...", members.len());
    for m in &members {
        // Routing invariant: the watched root must be canonical so notify's
        // echoed event paths `starts_with` it (docs/MULTIREPO-PHASE7.md §9).
        // `unwrap_or(true)` is deliberate: a `canonicalize` failure means the
        // dir was removed between workspace load and now (a benign race) —
        // don't panic the debug build over it; the `pipeline::run` below
        // surfaces the real error. We only want to catch an EXISTING but
        // non-canonical root.
        debug_assert!(
            m.root.canonicalize().map(|c| c == m.root).unwrap_or(true),
            "MemberWatch.root must be canonical: {}",
            m.root.display()
        );
        let (count, _rebuilt) = pipeline::run(&m.root, m.opts, &m.embedder_id, &m.excludes)
            .with_context(|| format!("initial index for workspace member {:?}", m.display_name))?;
        println!("  {} ({count} symbols)", m.display_name);
    }

    let roots: Vec<PathBuf> = members.iter().map(|m| m.root.clone()).collect();
    println!(
        "Watching {} workspace members. Press Ctrl+C to stop.",
        members.len()
    );

    let mut watch_loop = WatchLoop::new(&roots)?;
    watch_loop.run(|batch| {
        // Route the relevant changed paths to their owning members, then
        // update each affected member ONCE.
        for idx in route_changed_paths(&members, batch) {
            let m = &members[idx];
            let start = std::time::Instant::now();
            match pipeline::update(&m.root, m.opts, &m.embedder_id, &m.excludes) {
                Ok((total, changed, deleted)) => {
                    if changed > 0 || deleted > 0 {
                        println!(
                            "[{:.1?}] {}: {changed} changed, {deleted} deleted, {total} total",
                            start.elapsed(),
                            m.display_name
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Update error ({}): {e:#}", m.display_name);
                }
            }
        }
    });

    Ok(())
}

/// Map a batch's relevant changed paths to the indices of the members that
/// own them (deduped, in first-seen order). A path belongs to member `m`
/// when `path.starts_with(&m.root)`; members are disjoint
/// (`workspace::reject_overlaps`) so each path maps to at most one member.
/// Paths under no member (between members, or workspace-root files) and
/// non-relevant paths (not source / not `.gitignore`) are dropped.
fn route_changed_paths(members: &[MemberWatch], events: &[DebouncedEvent]) -> Vec<usize> {
    let mut affected: Vec<usize> = Vec::new();
    for e in events {
        for p in &e.event.paths {
            let relevant = is_source_path(p)
                || matches!(p.file_name().and_then(|n| n.to_str()), Some(".gitignore"));
            if !relevant {
                continue;
            }
            // `starts_with` is COMPONENT-wise (so `/ws/foo-ext` does not
            // match root `/ws/foo`); on canonical paths this is the correct
            // ownership test.
            if let Some(idx) = members.iter().position(|m| p.starts_with(&m.root)) {
                if !affected.contains(&idx) {
                    affected.push(idx);
                }
            }
        }
    }
    affected
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

    fn member(root: &str) -> MemberWatch {
        MemberWatch {
            root: PathBuf::from(root),
            display_name: root.rsplit('/').next().unwrap_or(root).to_string(),
            opts: pipeline::IndexOptions::default(),
            embedder_id: String::new(),
            excludes: Vec::new(),
        }
    }

    fn modify(path: &str) -> DebouncedEvent {
        evt(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            vec![PathBuf::from(path)],
        )
    }

    #[test]
    fn route_maps_path_to_owning_member() {
        let members = [member("/ws/alpha"), member("/ws/beta")];
        // A file two levels deep under alpha routes to member 0.
        let affected = route_changed_paths(&members, &[modify("/ws/alpha/src/a.rs")]);
        assert_eq!(affected, vec![0]);
    }

    #[test]
    fn route_ignores_paths_under_no_member() {
        let members = [member("/ws/alpha"), member("/ws/beta")];
        // A source file at the workspace root (between members) owns nobody.
        // `/ws/alpha-ext` must NOT match `/ws/alpha` (component-wise prefix).
        let affected = route_changed_paths(
            &members,
            &[modify("/ws/top.rs"), modify("/ws/alpha-ext/x.rs")],
        );
        assert!(affected.is_empty(), "got {affected:?}");
    }

    #[test]
    fn route_dedupes_and_covers_multiple_members() {
        let members = [member("/ws/alpha"), member("/ws/beta")];
        // Two files in alpha + one in beta → [0, 1], alpha not duplicated.
        let affected = route_changed_paths(
            &members,
            &[
                modify("/ws/alpha/a.rs"),
                modify("/ws/alpha/b.rs"),
                modify("/ws/beta/c.rs"),
            ],
        );
        assert_eq!(affected, vec![0, 1]);
    }

    #[test]
    fn route_drops_non_source_paths() {
        let members = [member("/ws/alpha")];
        // A non-source file under alpha is not relevant → no member affected.
        let affected = route_changed_paths(&members, &[modify("/ws/alpha/target/bin")]);
        assert!(affected.is_empty(), "got {affected:?}");
        // But a `.gitignore` under alpha IS relevant.
        let gi = route_changed_paths(&members, &[modify("/ws/alpha/.gitignore")]);
        assert_eq!(gi, vec![0]);
    }

    #[test]
    fn route_workspace_root_gitignore_owns_nobody() {
        // A `.gitignore` at the workspace ROOT (between members) is relevant
        // batch-wide but belongs to no member, so routing is a no-op (no
        // member re-indexes). Documents the otherwise-surprising silence.
        let members = [member("/ws/alpha"), member("/ws/beta")];
        let affected = route_changed_paths(&members, &[modify("/ws/.gitignore")]);
        assert!(affected.is_empty(), "got {affected:?}");
    }

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
