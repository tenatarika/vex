# Persistent History Index (Phase 14.8)

How `vex` builds and updates the persistent historical-symbol index —
the path from a git commit to a hit in `vex history <Symbol>`. Written
after the v1.15.0 Phase 14.8 incremental walker landed; consolidates
pipeline knowledge that would otherwise spread across CHANGELOG,
inline comments, and LIMITATIONS.md.

> **Quick model:** opt in via `vex index --history`. The builder walks
> `git log --raw --no-renames` from `HEAD`, dedupes files by blob SHA,
> parses each unique `(path, blob)` via the Phase 14.7 blob cache,
> and writes a FST-indexed sidecar (`<index_dir>/index.git_history`)
> mapping every symbol name to every commit that touched a blob
> containing it. `vex history <Symbol>` auto-picks the indexed path
> (~10ms FST lookup) when the sidecar is present and falls back to
> the v1.15.0 query-time walker (~seconds, shells out to `git log`)
> when it's not.

---

## File layout

```text
<cache-dir>/<project-hash>/
├── index.vex                ← binary index (unchanged by Phase 14.8)
├── index.git_history        ← Phase 14.8 sidecar (VXGH magic)
├── manifest.json            ← gains `history_indexed_at`,
│                              `history_tip_sha`, `history_depth`,
│                              `history: { commit_count, blob_count,
│                              entry_count, depth_capped }`
└── …(existing sidecars for HNSW, body_tokens, bloom, etc.)
```

The history index is a **sidecar**, not an inline section in `index.vex`.
Architect-locked design called for inline (v6→v7 sub-header chain).
Step 4a shipped as a sidecar instead to keep scope tight; the on-disk
record layout is byte-identical to what an inline section would emit,
so promotion is a mechanical relocation later. See the deviation note
in `src/store/git_history.rs` module header.

---

## On-disk layout

```text
[Header — 64 bytes, fixed]
  magic            [u8; 4]  = b"VXGH"
  version          u16      = HISTORY_SECTION_VERSION (1)
  flags            u16      bit 0 = was_depth_capped
  entry_count      u32
  commit_count     u32
  blob_count       u32
  strings_len      u32
  commits_offset   u32      (relative to file start)
  blobs_offset     u32
  strings_offset   u32
  entries_offset   u32
  fst_offset       u32
  fst_len          u32
  postings_offset  u32
  postings_len     u32
  reserved         [u8; 8]

[Commits   — 32 B × commit_count]    fixed-size, mmap-friendly
  sha               [u8; 20]
  date_unix_seconds u32
  author_offset     u32           into strings sub-section
  _pad              [u8; 4]

[Blobs     — 24 B × blob_count]
  sha               [u8; 20]
  _pad              [u8; 4]

[Strings   — packed `[u32 byte_len][UTF-8 bytes]` records]
  Sidecar-private (architect M1). Offset 0 reserved for empty-string
  sentinel. Stores file paths + signatures + author names; does NOT
  contaminate the global StringTable's FST builders.

[Entries   — 28 B × entry_count]
  blob_idx          u32           into Blobs array
  file_offset       u32           into Strings
  line              u32           1-based
  signature_offset  u32           into Strings; 0 = no signature
  first_commit_idx  u32           into Commits (oldest seen)
  last_commit_idx   u32           into Commits (newest seen)
  kind              u8            SymbolKind discriminant
  _pad              [u8; 3]

[FST       — `fst::Map` bytes]
  Keys: lowercased symbol names.
  Values: byte offsets into the postings blob.

[Postings  — `[u32 count][u32 entry_idx; count]` blocks]
  Indexed by FST values.
```

**Compile-time guards** in `src/index/history_builder/mod.rs` pin the
sizes: `HistoryEntry::SIZE == 28`, `Commit::SIZE == 32`,
`Blob::SIZE == 24`, `SidecarHeader::SIZE == 64`. A future field
reorder or type change that breaks the documented layout fails at
`cargo check`, not at runtime when a v1 sidecar suddenly reads garbage.

---

## Pipeline stages

### 1. Git enumeration

`git log --raw --no-renames --no-merges --no-abbrev -nDEPTH
--pretty=format:'COMMIT %H|%ct|%an' tip`

Produces `(commit_sha, path, blob_sha)` triples plus per-commit
metadata (unix-seconds timestamp + author name).

**Critical**: `--no-abbrev` is mandatory. git defaults to abbreviating
blob SHAs in `--raw` (typically to 7 chars); `decode_sha20` rejects
anything that isn't 40 hex chars and drops the triple. Without
`--no-abbrev` the section comes out empty with no error surfaced.

### 2. Index assignment

Commit indices are assigned **chronologically** (oldest = 0,
newest = N-1) via **git-log encounter-order reversal**. git log emits
newest-first; we reverse the encounter list to get chronological
order. Sorting by `(unix_seconds, sha)` instead would fail on
synthetic test fixtures where three commits land within one second
and the SHA tiebreaker scrambles the chronological order that the
`first_commit_idx <= last_commit_idx` invariant depends on. git's
parent-walk ordering is deterministic regardless of timestamp
resolution; reversal preserves it.

Blob SHAs are assigned indices in first-seen order during the
triple walk.

### 3. Commit-span computation

Walk triples in chronological order. For each `(blob_sha, path)`
key, track `(first_commit_idx, last_commit_idx)`:
- First observation: `first = last = current_commit_idx`
- Subsequent observation: `first = min(first, current)`, `last = max(last, current)`

**Convex-hull semantics** (architect H1, accepted lossy
approximation): if blob X appears at commits A → C and B has a
different blob in between (revert / cherry-pick), the entry's
`[first=A, last=C]` overstates continuity. The dominant case is
contiguous presence; the pathological case is rare and correctness
still holds in the "X existed at some point in [first, last]" sense.
A future `--exact-presence` flag could materialise a per-entry
presence bitmap if real demand surfaces.

### 4. Parse via 14.7 blob cache

For each unique `(blob_sha, path)`:
1. `Language::from_extension(path.ext)` → skip if unsupported.
2. `BlobCache::lookup(blob_sha, lang)` → return cached `ParsedFile` if hit.
3. On miss: `git cat-file --batch` → `parse_file(path, content, lang)` → `cache.insert`.

The cat-file process is long-lived (`CatFileBatch`) — spawned once,
fed blob SHAs via stdin, parses headers + reads exactly `size`
bytes via the SAME BufReader (going around the buffer with raw
`read_exact` is the classic Rust pipe-deadlock trap; see the bench
harness Step 2 retro). Drop kills the child explicitly before
waiting to avoid the stdin-still-open deadlock that pure
`child.wait()` would hit.

### 5. Materialise entries

For each `ParsedSymbol` in each parsed blob:
- Intern path + signature + author into the sidecar-private strings table
- Emit one `HistoryEntry { blob_idx, file_offset, line, signature_offset, first_commit_idx, last_commit_idx, kind }`
- Push symbol name into the parallel `entry_names: Vec<String>`

### 6. Write sidecar

`encode_section` projects entries + commits + blobs into the on-disk
layout. The FST is built once from `entry_names` (lowercased), with
sorted-and-dedupped posting lists per key. `write_sidecar` does
atomic temp-rename + fsync (mirrors `index.hashes` / `index.bloom`
sidecar pattern).

---

## Update paths

`vex update --history` (or sticky-via-manifest `vex update` after an
initial `vex index --history`) takes one of three branches inside
`write_output_locked`:

### Branch A — no-op fast path

Triggered when:
- Sidecar present
- `manifest.history_tip_sha == git rev-parse HEAD`
- `opts.history_depth == manifest.history_depth`

The section build is **skipped entirely**. Manifest's `indexed_at`
refreshes to today; sidecar mtime and content stay untouched. Pinned
by `update_history_no_new_commits_uses_fast_path` integration test
(mtime must not change). Sub-200ms on any repo size.

### Branch B — incremental walker

Triggered when (Step 5c):
- Sidecar present
- Prior tip is an ancestor of HEAD (linear history — no force-push)
- Tip changed
- Depth unchanged

`HistoryReader::extract_owned` reverses the on-disk sidecar back to
builder shape (FST stream + entry_idx → name reverse map + bulk
record copy + strings clone). `build_history_section_for_range`
walks ONLY `<prior_tip>..HEAD`. `merge_history_sections` unions the
two:

- **Commits**: concat. Delta is by construction disjoint from prior
  (walked the range). Delta commit indices shift by
  `prior.commits.len()`.
- **Blobs**: union by SHA. Delta blobs already in prior reuse prior
  index; new blobs append. Delta entries' `blob_idx` remapped.
- **Strings**: concat raw bytes. Delta string offsets shift by
  `prior.strings.len()`. Duplicate empty-string sentinel at the
  shifted offset is harmless (4 wasted bytes).
- **Entries**: appended with shifted commit indices + remapped blob
  indices + shifted string offsets.
- `was_depth_capped = prior || delta` (OR).

`tracing::info!` logs `phase 14.8: incremental git_history update
(prior=N, delta=M, merged=N+M)`. Pinned by
`update_history_linear_new_commits_is_incremental` (prior symbols
preserved + delta symbols added).

**Defensive fallback**: any error (corrupt prior sidecar,
future-version mismatch, range walk failure) falls through to a
from-scratch full rebuild. `vex update` never produces a missing
section as a result of incremental failure.

### Branch C — force-push detect + full rebuild

Triggered when prior tip exists but `git merge-base --is-ancestor
<prior> <current>` returns non-zero (architect H3). Logs
`tracing::warn!` "prior history tip is not an ancestor of HEAD
(force-push or rebase detected). Full git_history rebuild forced."
The full rebuild walks the entire reachable history; the section
correctly reflects only commits reachable from the new tip.

Pinned by `force_push_triggers_full_rebuild_with_warning`
(rewritten-history symbols become invisible; new symbols visible).

---

## Drop path

`vex update --no-history` (after an indexed run) takes its own
fourth branch:
- Sidecar deleted (best-effort: permission errors warn but don't
  block the rest of the index write).
- All four `history_*` manifest fields set to `None`.
- `cmd_history` next call falls back to the query-time walker; JSON
  envelope advertises `_meta.vex.dev/history_mode = "walker"`.

The skip-path gate `manifest_options_cover` is extended so
`--no-history` on an unchanged tree actually reaches the delete
code (the gate otherwise short-circuits before `write_output_locked`
runs).

Pinned by `no_history_drops_sidecar_and_manifest_fields`.

---

## Manifest fields

```jsonc
{
  // … existing fields (call_graph, bm25, cpp_includes_processed, …)
  "history_indexed_at": "2026-06-08",          // sticky sentinel (architect L3)
  "history_tip_sha": "0123…4567",              // for incremental + force-push detect
  "history_depth": 500,                        // sticky cap (None = unbounded)
  "history": {
    "commit_count":   4346,
    "blob_count":     18922,
    "entry_count":    586136,
    "depth_capped":   false                    // true when --history-depth N stopped the walk
  }
}
```

All four are `Option<…>` with `skip_serializing_if = "Option::is_none"`.
Pre-Phase-14.8 manifests load transparently with all four as `None`.

The sticky-via-sentinel rule: `vex update` (no flag) reads
`manifest.history_indexed_at.is_some()` and forces `with_history=true`
when it is. `--history` always wins; `--no-history` always wins.
`--history-depth` inherits from manifest if not passed.

---

## Reader API

```rust
use vex::store::git_history::HistoryReader;

let path = vex::util::config::git_history_path(&project_root);
match HistoryReader::open(&path)? {
    Some(reader) => {
        let entry_idxs = reader.find_by_name("parse_payment");
        for idx in entry_idxs {
            let entry = reader.entry(idx).unwrap();
            let commit = reader.commit(entry.last_commit_idx).unwrap();
            let blob = reader.blob(entry.blob_idx).unwrap();
            let file_path = reader.string(entry.file_offset);
            // …
        }
    }
    None => {
        // No sidecar — caller falls back to walker.
    }
}
```

Reader is mmap-backed, zero-copy except for the FST instantiation
(which clones the FST bytes via `fst::Map::new(fst_slice.to_vec())`
— the alternative is plumbing lifetimes that cross thread
boundaries via `Arc`, deferred until profiling shows it matters).

`extract_owned()` returns `(HistorySection, Vec<String>)` rebuilt
from the on-disk records — used by the Step 5c incremental update
path to load the prior section back into builder shape.

---

## CLI surface

### `vex index --history [--history-depth N]`

Opt-in. Builds the sidecar after the main `index.vex` write. Failure
is non-fatal (`tracing::warn!` + sidecar absent → walker fallback).

### `vex update [--history | --no-history]`

Sticky-via-manifest by default (inherits prior decision). Explicit
`--history` or `--no-history` overrides. Picks one of A/B/C/drop
branches per `manifest.history_tip_sha` vs current HEAD.

### `vex history <Symbol> [--branch REV] [--no-index]`

Auto-mode selection: indexed if sidecar present, walker otherwise.
`--no-index` forces walker. `--branch <REV>` only honoured by the
walker (indexed reflects HEAD at index time; passing `--branch other`
on the indexed path logs a `tracing::warn!` suggesting `--no-index`
for branch-specific queries).

**Phase 14.9 v1.16.0 flags (Tier A + B):**

- `--since YYYY-MM-DD` / `--until YYYY-MM-DD` — inclusive date
  window. Lex-compared against `commit_date` (fixed-width ISO is
  lex-equivalent to chronological order). Works on both paths.
- `--author <SUBSTR>` — case-insensitive substring on commit author.
  **Walker-only** — the sidecar drops author info; passing on the
  indexed path emits an `eprintln!` and exits non-zero with a hint
  pointing at `--no-index`.
- `--kind <KIND>` — exact lowercase match against `kind`
  (`function` / `struct` / `impl` / …). Suppresses partner rows like
  the `impl` that pairs with every `struct` hit.
- `--diff` — render unified diffs between consecutive entries of the
  same `(symbol, kind)` group via `similar::TextDiff::from_lines`.
  Head of each group carries the full signature; non-head entries
  carry `--- @prev_sha\n+++ @curr_sha\n…` lines (text mode) or
  `body_diff: { from, to, hunks }` (JSON). Advertised in
  `capabilities.history_diff = true`.
- `--exact-presence` — enumerate the exact set of commits where the
  entry's blob lived in its file (defeats the §4c #4 convex-hull
  lossy span). Walks `git log` from HEAD capped by
  `--exact-presence-max-commits N` (default 500); resolves each
  commit's blob at `file_path` via batched `git cat-file
  --batch-check`. Above the cap, falls back to the convex-hull span
  with `presence_truncated: true` in JSON and an `eprintln!` notice
  in text mode. **File-blob equality, not symbol-body equality** —
  a sibling-symbol change in the same file produces a new file blob
  and narrows presence.

**JSON envelope (v1.16.0 BREAKING):** ported from the hand-rolled
`json!({...})` literal to the typed `ResponseEnvelope<T>` via
`output::print_envelope`. `results.items[*]` → `results[*]` (array,
not object). Legacy `vex.dev/query_symbol` and `vex.dev/result_count`
fields drop in favour of `MetaEnvelope`'s canonical
`vex.dev/index_age_ms` / `ttlMs` / `cacheScope`. New observability
field `vex.dev/history_mode = "indexed" | "walker"` indicates which
path served the query.

**Cookbook**

```bash
# Default — every historical version of a symbol, newest first
vex history IndexReader

# Time-windowed — only entries from a specific quarter
vex history IndexReader --since 2026-04-01 --until 2026-06-30

# Author + kind narrowing on the walker
vex history IndexReader --no-index --author furcas --kind struct

# Diff mode — what actually changed between versions
vex history IndexReader --diff

# Discovery: prefix fallback when you forget the exact name
vex history inde --limit 20

# Revert-aware presence (Phase 14.9 Tier B.7)
vex history IndexReader --exact-presence --limit 5

# Combined: diff + filter + JSON for an MCP agent
vex history IndexReader --since 2026-01-01 --kind struct --diff \
  --format json
```

### `vex status`

JSON: `history_indexed_at` (top-level, ISO date or null) +
`history` sub-object with `commit_count`/`blob_count`/`entry_count`/
`depth_capped`. Phase 14.9 Tier B.6 additions: top-level
`has_submodules: bool` and `git_history_size_bytes: u64 | null`.

Text:
```
History:    indexed at 2026-06-08 (4346 commits, 18922 blobs, 586136 entries)
            ⚠ section is partial: --history-depth cap stopped walking before the root commit.
              Symbols introduced before the cap are NOT indexed; re-run `vex index --history`
              without the cap to cover full history.
```

Phase 14.9 Tier B.6 — two additional warnings fire when history is
indexed:
```
            ⚠ this repo has submodules — their history is NOT in
              index.git_history. Submodule blobs aren't in the parent
              repo's git db. (LIMITATIONS §4c #6)
            ℹ git_history sidecar is 3.5× index.vex (19500.0 KB) —
              long-lived repos scale by history depth, not
              current-symbol-count. Cap with --history-depth N.
              (LIMITATIONS §4c #5)
```

Or, on a non-history-indexed project:
```
History:    no (run `vex index --history` to enable indexed `vex history`)
```

---

## Performance

End-to-end CLI wall time, vex self-repo (~500 commits, 4505 symbols)
and tokio (~4346 commits, 15300 symbols):

| operation                            | vex self     | tokio        |
|--------------------------------------|--------------|--------------|
| cold `vex index --history`           | 11.68 s      | 68.12 s      |
| `vex history <S>` indexed            | <10 ms       | 10 ms        |
| `vex history <S>` walker (`--no-index`) | 6.75 s    | 16.41 s      |
| **indexed-vs-walker speedup**        | **~675×**    | **~1640×**   |
| `vex update` no-op fast path         | 290 ms       | 190 ms       |
| `vex update` incremental (1 new commit) | ~50 ms    | ~200 ms      |

### Section-size scaling (honest)

```
                  index.vex   index.git_history   ratio
vex self-repo     1.8 MB      1.5 MB              84%
tokio             5.6 MB      19.5 MB             346%
```

The Step 2 napkin "≤10% of index.vex" target was wrong. Section size
scales with **history depth** (commits × symbols-per-blob), not with
current-symbol-count. On tokio (4346 commits × 586k history entries)
the section is ~3.5× the main index size. This is correct behaviour
— the user explicitly opts in via `--history`; the storage trade-off
vs query-speed (1640× faster) is overwhelmingly positive.

If section size matters: pass `--history-depth N` to cap the walk at
N newest commits. `vex status` warns when a cap was hit so users
don't silently miss historical symbols.

---

## Operational guidance

### When to enable `--history`

- **Yes**: codebases with rich git history where `vex history <Symbol>`
  is a regular workflow (archaeology, finding when a symbol was
  introduced/changed, tracking removed code).
- **Yes**: agent workflows where sub-second history queries unlock
  composability with other tools (the walker's seconds-scale latency
  breaks chained agent loops).
- **No**: short-lived repos or one-off projects where the walker's
  per-query cost is acceptable.
- **No**: storage-constrained environments unless `--history-depth`
  is set to a modest cap.

### Recovery from a corrupt sidecar

`vex history` falls back to the walker on any sidecar load error. To
force a rebuild: `rm <index_dir>/index.git_history && vex index
--history`. Or `vex update --no-history` followed by `vex update
--history` to round-trip through the drop branch.

### Inspecting the section

Use the `vex` CLI:
- `vex status --format json` for counts.
- `vex history <Symbol> --format json` for entry-level inspection.

Raw byte inspection: the sidecar starts with `b"VXGH"`. Use `xxd` or
`od -c` to confirm magic + read header bytes.

---

## Known limitations

See [LIMITATIONS.md §4c](LIMITATIONS.md) for the user-facing list.
Headline items:

- **No symbol-rename tracking**: `foo` renamed to `bar` surfaces as
  two separate symbols. Two-query workflow.
- **Single ref only**: index built from `HEAD` at build time;
  `--branch <REV>` on `vex history` is ignored on the indexed path
  (warned). Use `--no-index` for branch-specific queries.
- **No per-commit time-travel**: `vex callers @sha` not supported —
  this is a symbol-only index, no historical call graph.
- **Convex-hull commit spans** (architect H1): a blob's
  `[first_commit_idx, last_commit_idx]` overstates continuity when
  the blob is reintroduced after a different blob in between
  (revert / cherry-pick). Documented + accepted.
- **Section size scales with history depth, not current symbols** —
  see the table above; use `--history-depth N` to bound.

---

## Cross-references

- [Phase 14.7 blob cache](https://github.com/Furcas-vrn/vex/blob/main/CHANGELOG.md#1100---2026-05-28)
  — the prerequisite that makes parse-cheap (warm 14.7 cache makes
  cold history index ~5-10× faster than uncached).
- [docs/SEMANTIC.md](SEMANTIC.md) — the v1.15.0 B1.2 semantic
  pipeline. Parallel design (sidecar + sticky-via-manifest +
  incremental update on the same architectural footprint).
- [docs/LIMITATIONS.md](LIMITATIONS.md) §4c — known limits.
- [docs/COOKBOOK.md](COOKBOOK.md) Recipe 1 — agent-workflow example
  using `vex history` for code archaeology.
- Source: [`src/store/git_history.rs`](../src/store/git_history.rs)
  (writer + reader + StringTable),
  [`src/index/history_builder/mod.rs`](../src/index/history_builder/mod.rs)
  (builder + merge), [`src/cli/cmd_history.rs`](../src/cli/cmd_history.rs)
  (auto-mode selection), [`src/index/pipeline/output.rs`](../src/index/pipeline/output.rs)
  (3-branch update logic).
- Tests: [`tests/history_index_test.rs`](../tests/history_index_test.rs)
  (12 integration tests pinning every branch + status surface).
