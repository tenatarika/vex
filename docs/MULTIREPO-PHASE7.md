# Multi-repo Phase 7 — `vex watch --workspace`

Status: **SHIPPED** (2026-06-29). Implements `docs/MULTIREPO.md` §6.5 / §8
phase 7. One long-running watcher keeps every `.vex-workspace.toml` member's
index incrementally fresh. No on-disk format change. All §9 review
resolutions folded in; 3046/3046 nextest, clippy + stable-fmt clean.

## 1. Goal

`vex watch --workspace` = the watch-mode analogue of `vex index/update
--workspace`. It:
1. builds the initial index for every member (each via its own `.vex.toml`),
2. arms a recursive file watcher over every member root,
3. routes each changed file to its OWNING member and runs that member's
   incremental `pipeline::update` — only the affected members re-index.

## 2. Current single-repo watch (what we extend)

`src/watch/handler.rs::watch(root, opts, embedder, excludes)`:
- `pipeline::run` (initial), then a `notify_debouncer_full` debouncer over
  `root` (`RecursiveMode::Recursive`), 500 ms debounce.
- Event loop: `rx.recv()` → drain queued batches (`try_recv`) → evict
  `Remove(Folder)` paths from `armed_dirs` → re-arm `Create(Folder)` dirs
  (Linux inotify non-recursive-at-watch-time; `armed_dirs` dedupes the
  O(subtree) `FileIdMap::add_path` walk) → relevance filter
  (`is_event_batch_relevant`: source extension or `.gitignore`) →
  `pipeline::update(root)`.
- Standalone, reusable helpers: `is_event_batch_relevant`, `is_source_path`,
  `extract_new_directories`, `extract_removed_directories`.

## 3. CLI surface

- `args.rs` Watch variant gains `workspace: bool` (`--workspace`), mirroring
  the other 10 fanout commands.
- `common.rs::extract_workspace_flag` gains a `Commands::Watch { workspace, .. }`
  arm so `cli/mod.rs` installs the Phase 2 per-member `CacheResolver` before
  dispatch (a member's own `cache_dir`/`local_cache` is honoured in watch
  too). `extract_path_hint` already covers `Watch`.
- `cmd_watch::watch` gains `workspace: bool`; when set it delegates to a new
  `watch_workspace`.

## 4. `watch_workspace`

```
load Workspace::find_and_load(root_hint or cwd)        // resolver already installed
build per member:
    MemberWatch { root: m.root, opts, embedder_id, excludes }
        opts        = build_index_options(member_cfg + shared flags + member prior manifest)
        embedder_id = resolve_embedder(flag, member_cfg)
        excludes    = member_cfg.exclude
initial index: for each member → pipeline::run(m.root, m.opts, …)   // all-or-nothing (MVP)
one debouncer; for each member → debouncer.watch(m.root, Recursive)
armed_dirs = { every member root }
event loop:
    drain batch (recv + try_recv)
    evict Remove(Folder) from armed_dirs                 // reuse helper
    re-arm Create(Folder) new dirs (armed_dirs dedupe)   // reuse helper
    if !is_event_batch_relevant(batch): continue          // reuse helper
    route: affected = { member m : some changed path p, p.starts_with(m.root) }
           (members disjoint via reject_overlaps → each path maps to ≤1 member)
    for m in affected (declared order):
        pipeline::update(m.root, m.opts, m.embedder_id, m.excludes) → print per-repo summary
```

Routing detail: collect the relevant changed paths from the batch, map each
to its owning member by `starts_with(member.root)`, dedupe members, update
each affected member ONCE (a batch touching 3 files in member A + 1 in member
B → one update for A, one for B). Paths under no member (gaps between members,
or files at the workspace root itself) are ignored — only declared members
are indexed.

## 5. Concurrency (§9 — the flagged risk)

- **Within the watch process**: the event loop is single-threaded and updates
  members sequentially; no two updates touch one member concurrently. (rayon
  parallelism is *inside* each `pipeline::update`; `--jobs`/`VEX_JOBS` is
  workspace-wide — documented.)
- **Cross-process** (a concurrent `vex search --workspace` reading member X's
  index while watch rewrites it): already safe via the existing protocol that
  single-repo watch + every read command rely on — `pipeline::update` writes
  `index.vex.tmp` then `fs::rename`s atomically; an mmap `IndexReader` holds
  the old inode until it drops, so a reader never sees a half-written index.
  **No new concurrency machinery is introduced by this phase.**

## 6. Refactor vs duplication

The debouncer setup + drain + evict + re-arm logic is identical between
single-repo `watch` and `watch_workspace`; only (a) the initial-index step
(1 vs N) and (b) the update dispatch (whole-root vs route-to-member) differ.
Proposal: extract the shared event-loop skeleton into a helper that takes the
armed roots + a `dispatch(&[DebouncedEvent])` closure, so both callers share
the drain/evict/re-arm/relevance code and supply only their dispatch. Keeps
`extract_*`/`is_*` helpers as-is. (Open for review — a thin shared core vs
~50 lines duplicated.)

## 7. Tests
- Unit: a `route_changed_paths(members, batch) -> Vec<affected_member_idx>`
  pure helper — a path under member A maps to A; a path under no member maps
  to none; a batch touching A and B yields both, deduped.
- e2e (assert_cmd): watch is long-running (blocks on Ctrl+C), so a full e2e is
  awkward in CI. Cover the routing + initial-index via the pure helper +
  reuse the existing `index --workspace` e2e for the build path. A
  best-effort spawned-process e2e (start watch, touch a member file, poll the
  member index mtime, SIGINT) is optional and may be flaky — gate behind a
  non-default feature or skip.

## 8. Risks / non-goals
- **Initial-index failure** → bail the whole watch (all-or-nothing, matches
  `index --workspace`). A member that fails to build aborts startup with a
  clear error rather than watching a partial set.
- **Member added/removed at runtime** — the member set is fixed at startup
  (the manifest is read once). Editing `.vex-workspace.toml` mid-watch is not
  honoured until restart (document). New *source* dirs INSIDE a member are
  armed via the existing `Create(Folder)` re-arm.
- **Scale** — one debouncer over N member roots; fine for tens of repos
  (same ceiling as the rest of the workspace feature).

## 9. Review resolutions (architect + rust-reviewer, locked before scaffold)

**HIGH — routing canonical-symmetry (the load-bearing correctness point).**
`MemberWatch.root` MUST be `Member.root` verbatim (canonical, `workspace/mod.rs:142`).
We register `debouncer.watch(m.root, …)` with that canonical root; notify
derives event paths from the watched root, so they arrive canonical-root-
prefixed and `event_path.starts_with(m.root)` matches — the same symmetry the
single-repo `watch` relies on (it canonicalizes `root` at `handler.rs:54`).
Do NOT canonicalize event paths in the router (canonicalize fails on a
just-deleted path, dropping delete events). Pin the invariant with a
`debug_assert!(m.root == m.root.canonicalize().unwrap_or(m.root))` and a
two-levels-deep routing unit test.

**HIGH — member-root deletion must not silently kill one member.** Single-repo
eviction of a `Remove(Folder)` path from `armed_dirs` is benign (whole watch
dies visibly). In workspace mode, evicting a MEMBER ROOT would silently stop
watching that member while the others keep working. Resolution: never evict a
member root from `armed_dirs` (they are permanent for the session); if a
`Remove(Folder)` names a member root, log a `tracing::warn!` ("member X
removed; restart `vex watch --workspace` to drop it"). Member set is frozen at
startup (§8).

**HIGH — `extract_workspace_flag` MUST gain the `Watch` arm.** Without it the
Phase 2 `CacheResolver` is never installed for watch and members resolve to
the wrong cache dirs (macOS symlink hazard). Add it with the args
`workspace: bool`.

**MEDIUM — shared core as `struct WatchLoop`, not a free fn + closure.**
`WatchLoop { debouncer, rx, armed_dirs, watched_roots: Vec<PathBuf> }` with
`new(roots: &[PathBuf]) -> Result<Self>` (does all `debouncer.watch` calls +
seeds `armed_dirs`) and `run(&mut self, dispatch: impl FnMut(&[DebouncedEvent])
-> Result<()>)`. `dispatch` is `FnMut` — NOT `Send`/`'static` (stays on the
event-loop thread; only the debouncer's own `tx` callback is `'static`).
Re-arm is scoped: a `Create(Folder)` is armed only if it falls under some
`watched_roots` entry (skips workspace-root / between-member scratch →
no wasted `FileIdMap::add_path` O(subtree) walks). Single-repo `watch` and
`watch_workspace` become thin wrappers supplying their own `dispatch` (which
also owns the per-update summary print, so the core stays format-agnostic).

**MEDIUM — eviction also skips member roots** (see above) — `WatchLoop` knows
`watched_roots`, so both the re-arm scope and the no-evict rule key off it.

**LOW — per-member initial index error context.** Wrap each initial
`pipeline::run(&m.root, …)` in `.with_context(|| format!("initial index for
workspace member {:?}", m.display_name))?` so an all-or-nothing abort names
the failing member.

**LOW — concurrency note.** Each `pipeline::update(m.root)` takes its OWN
per-root `IndexLock` internally (`pipeline/mod.rs`); the watcher holds no lock
between updates, exactly as single-repo watch. The atomic-rename in
`store/writer.rs` (`.tmp` → `fs::rename` + parent fsync) is what makes a
concurrent cross-process reader safe. No new machinery (§5 confirmed).

**LOW — double manifest load** (resolver build in cli/mod.rs + `watch_workspace`'s
own `find_and_load`) — accepted, matches the other fanout commands.
