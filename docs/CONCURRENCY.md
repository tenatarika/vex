# Concurrency

How `vex` behaves when multiple processes touch the same index at once.
Written after the v1.11.1 + v1.11.2 thundering-herd fixes — read this before
extending or debugging the index-build paths.

## The contract

> Only one process rebuilds a given index at a time. Concurrent rebuilders
> serialize via an advisory lock; if a peer just finished an identical
> rebuild, the rest observe the fresh manifest and skip.

This applies to both `vex index` (`pipeline::run`) and `vex update`
(`pipeline::update`). The on-disk index pair (`index.vex` + `index.hnsw`)
is published under a single critical section, so concurrent readers always
see a consistent snapshot.

## The lock

The build lock lives next to the index file:

```text
<cache-dir>/<project-hash>/
├── index.vex          ← binary index
├── index.hnsw         ← optional HNSW vectors (when --semantic was used)
├── manifest.json      ← file hashes + index metadata
└── index.lock         ← persistent advisory-lock sentinel
```

- **Per-project.** The lock path is derived from the index path, which is
  in turn derived from a hash of the canonical project root. Two
  different projects do not contend with each other.
- **Persistent.** The file is created once and never deleted. Removing it
  on release would re-introduce the classic `flock` + unlink race —
  a queued waiter keeps its handle on the now-unlinked inode while a new
  instance creates a fresh inode under the same name and locks it
  immediately, so both end up running.
- **Advisory.** Uses `flock` on POSIX and `LockFileEx` on Windows (via
  `fs2`). Other processes that ignore the lock can still write
  garbage — but `vex` itself always honors it.
- **Released on close.** The OS releases the lock when the file
  descriptor is closed, which happens automatically when the process
  exits — even on crash. There is no stale-lock cleanup needed.

## When the lock is held

- `pipeline::run`: acquired immediately after file discovery and held
  across parse + embed + write + HNSW build. The skip path (identical
  fingerprint) is taken under the lock and releases it immediately.
- `pipeline::update`: acquired after the *first* diff (so the cheap "no
  changes" case never blocks waiting peers), then held across the
  re-checked diff + parse + embed + write + HNSW build.

### `--no-wait` (v1.12.0)

Both `vex index` and `vex update` accept `--no-wait`. The corresponding
library entry points are `pipeline::run_or_busy` and
`pipeline::update_or_busy`. They use `IndexLock::try_acquire` (non-blocking
`flock` / `LockFileEx`) and return `Ok(None)` when a peer is currently
holding the lock; the CLI surfaces this as a `Skipped: another vex
instance is indexing` message and exit code 0, matching `git pull`'s
"Already up to date." UX. Without the flag, the blocking lock path is
unchanged.

The `update_or_busy` no-change fast path (working tree already matches
the manifest) does *not* go through `try_acquire`: if there is nothing
to do, there is no point deduping against a peer. Only the
parse + embed + write section is gated.

In both paths the lock-holding window is the entire expensive section.
For a small project this is sub-second; for a large index built with
`--semantic`, the HNSW build alone can take a minute or two on millions
of vectors. A waiting peer sees the lock-wait log message (below) within
the first millisecond of contention so it's clear what's happening.

## What waiting looks like

Since v1.11.2, contention is logged:

```text
INFO vex::index::pipeline: waiting for index lock (another vex instance is indexing) lock=/Users/foo/Library/Caches/vex/abc123/index.lock
```

The wait itself is silent at the syscall level (a blocking `flock`),
but the message above is emitted **before** the blocking call so users
and agent harnesses see why the CLI appears stuck. The message is at
`info` level — visible by default in CLI runs, can be filtered out by
setting `RUST_LOG=warn`.

When the peer finishes, the waiter takes the lock, runs its
`Manifest::load` re-check, and either:
- Re-uses the peer's work and skips (most common in agent fan-outs).
- Or proceeds to its own rebuild (file changes happened during the wait).

## Filesystem caveats

- **Local filesystems only.** The default cache directory is
  `~/Library/Caches/vex` (macOS), `~/.cache/vex` (Linux), or
  `%LOCALAPPDATA%\vex` (Windows) — always local. If you set
  `VEX_CACHE_DIR` to a network mount, advisory-lock semantics depend on
  the protocol (NFSv4 has byte-range locks; SMB uses its own oplock
  scheme) and *we make no guarantee*. Use a local cache directory.
- **Per-project lock.** If you point two projects with different
  canonical paths at the same `--path`, they will end up with the
  same cache hash and contend on the same lock — that's correct
  behavior, not a bug.

## Tests

The contract is pinned in `tests/concurrency_test.rs` (8 tests):

- `parallel_index_serialized_by_lock` — N concurrent `vex index` calls
  do not corrupt the index.
- `parallel_update_serialized_by_lock` — N concurrent `vex update`
  calls do not corrupt the index.
- `read_during_reindex_no_crash` — a reader thread sees a consistent
  snapshot while a writer rebuilds.
- `file_deleted_during_indexing_no_panic` — pipeline does not panic
  when a discovered file disappears before parse.
- `concurrent_update_rebuilds_once_not_per_thread` — of N concurrent
  `vex update` calls on a stale index, **exactly one** rebuilds.
- `concurrent_run_skips_when_index_already_fresh` — of N concurrent
  `vex index` calls when the index is *already fresh*, **zero**
  rewrite the manifest.
- `concurrent_run_rebuilds_once_not_per_thread` — of N concurrent
  `vex index` calls on a cold cache, **exactly one** rebuilds and the
  rest skip via the manifest re-check under the lock. (v1.12.0:
  enabled by the new `(usize, bool)` return on `pipeline::run` —
  before that, rebuild-vs-skip was not observable from outside.)
- `many_concurrent_readers` — 8 concurrent readers all see the same
  symbol count.

Run them with `cargo test --test concurrency_test`.

## Known limitations

- **Skip path is fingerprint-only, not options-aware.** Both
  `pipeline::run` and `pipeline::update` decide whether to skip a
  rebuild purely from the file-hash diff. A peer that built without
  `--semantic`, followed by a `vex index --semantic` waiter, will be
  served the structural-only index from the skip path and *no error
  is raised*. The waiter's `--semantic` request is silently
  downgraded. Tracked as a v1.12.0 follow-up; the workaround is to
  delete the cache directory and rebuild manually. This is a
  pre-existing behaviour shared with `update` since v1.11.1, not a
  regression introduced in v1.11.2.

## Things that intentionally are NOT locked

- **Read paths.** `IndexReader::open` uses memory-mapped I/O and
  tolerates a writer swapping `index.vex` out from under it — the old
  mmap stays valid until dropped. Readers never block writers and
  writers never block readers.
- **Different projects.** Locks are per-project. `vex index` against
  project A does not block `vex index` against project B even on the
  same machine.
- **MCP server query handling.** The MCP server only reads; the lock
  matters only when it triggers `auto_update`, which goes through
  `pipeline::update` and serializes there.

## History

- **v1.11.1** (2026-06-02) — closed the `vex update` herd: lock moved
  to wrap parse + embed (not just write); manifest re-check under the
  lock; never-unlink sentinel.
- **v1.11.2** (2026-06-02) — closed the `vex index` herd symmetrically;
  added the "waiting for index lock" tracing event so contention is
  visible.

See `CHANGELOG.md` `[1.11.1]` and `[1.11.2]` for the full context and
the original PR (#2) for the lock-file-race analysis.
