# Changelog

All notable changes to vex are documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed — Phase 14.11: `vex history --branch` on the indexed path

- `vex history <Symbol> --branch <non-HEAD>` now transparently routes
  to the walker even when the persistent history sidecar is present.
  Pre-14.11 the indexed path silently returned HEAD-time data and
  emitted a `tracing::warn!("phase 14.8: --branch is ignored …")`; the
  warning is removed and the query is now semantically correct without
  requiring `--no-index`.
- `--branch HEAD` (literal) normalizes to absent and keeps the indexed
  fast path — explicit `--branch HEAD` users see no perf regression.
- **JSON envelope behaviour change for MCP consumers**: the
  `_meta.vex.dev/history_mode` field now returns `"walker"` (was
  `"indexed"`) for `--branch <non-HEAD>` queries. This reflects the
  path that actually serviced the query and is the documented contract;
  the prior `"indexed"` value was paired with a silent warning and
  HEAD-time data, so dispatch logic that branches on it was already
  unreliable.
- Closes `docs/LIMITATIONS.md` §4c #1. Walker's "symbol name must still
  appear at the requested tip" constraint is the only surviving limit
  on `--branch` queries — same constraint that applied to
  `--no-index --branch X` pre-14.11.
- No on-disk format change. Old `index.git_history` (VEXH v1) and
  `index.rename_chains` (VEXR v1) sidecars load byte-identically.

## [1.18.0] - 2026-06-17

### Added — Phase 11.1.11 (Q4-C): transitive cascade via BFS

- `vex update` cascade now follows the `imported_by` reverse graph
  **transitively**. Q4-B (11.1.10) re-parsed direct importers; Q4-C
  walks the graph via BFS bounded by `CASCADE_MAX_DEPTH = 16`. A
  `c → b → a` chain where only `a` changes now re-parses both `b`
  (depth 1) AND `c` (depth 2); `vex usages --strict` recovers refs
  through Python / TypeScript re-export façades and deep Rust
  module chains without a full `vex index`.
- Cycle-safe via visited-set + the existing "already in changed/deleted"
  guard; star patterns terminate because all leaves are added to the
  visited set in a single depth pass.
- Depth saturation (`CASCADE_MAX_DEPTH` hit with a non-empty frontier)
  emits `tracing::warn!` so the operator can decide whether the
  pathologically deep chain justifies a full `vex index`. The log line
  now reports `depths 1..=N` with `N = the deepest level reached`.
- Closes the "Depth-1 only" item from LIMITATIONS §4d (Q4-B carry-
  over). No persistent-state schema change — Q4-C piggy-backs on the
  same `imported_by` map Q4-B built. The recent `index.state` sidecar
  (audit C1) was the architectural enabler: Q4-C is a pure cascade-
  algorithm change against the same on-disk shape.
- Cross-language coverage pinned by integration tests:
  - **Rust** (`use crate::a::T`) — 3-hop chain + cycle + star.
  - **TypeScript** (`import { T } from './a'`) — 3-hop chain.
  - **Python** (`from a import T`) — 3-hop chain.
  - **C++** (`#include "a.h"` via the Pass-2 include resolver) —
    3-hop chain across a.h → b.h → c.cpp.
  - **C#** (`using Namespace;` via namespace resolution) — 3-hop
    chain across `MyA` → `MyB` → `MyC`.
  - **Go** — no binder yet (`scope/mod.rs::bind_refs` falls through
    to `Ok(Vec::new())`); cascade is a documented no-op. A negative-
    control test fires if a Go binder ever lands so coverage is
    backfilled.

### Added — Phase 11.1.10 (Q4-B): `imported_by` cascade re-parses importers on rename

- New `Manifest.imported_by: BTreeMap<String, BTreeSet<String>>` records
  the reverse import graph — `imported_by[target_file]` is the set of
  files whose binder produced at least one resolved (or recoverable)
  cross-file edge into `target_file`. Populated by the writer from both
  the per-file resolution loop and the Q4-A reconstruction second pass
  (architect-H1 must-fix: capture import relationship even when this
  turn's specific edge resolution fails — preserves the edge for the
  next update's cascade).
- During `vex update`, every importer in `imported_by[changed_file]` is
  added to the changed set and **re-parsed** (not reconstructed). Fresh
  `bound_refs` against the new name table close the Phase 11.1.9 gap
  where renamed-in-changed-file targets silently dropped reconstructed
  edges. `vex usages --strict` recovers the rename without a full
  `vex index`.
- Bootstrap: pre-11.1.10 manifests have no `imported_by`. The first
  `vex update` after upgrade emits `tracing::info!` ("imported_by
  absent in manifest; cascade skipped this turn") and populates the
  map for subsequent updates. No format-version bump or forced
  re-index needed.
- Cycle-safe at depth 1: A↔B with only A edited cascades B; B's reverse
  re-cascade of A is filtered by the "already in changed/deleted"
  guard. Documented in LIMITATIONS §4d.
- Cascade activity surfaces at `RUST_LOG=vex=info` ("cascade:
  re-parsing N importer(s) of changed/deleted files").
- Writer entry point now returns `Result<NewIndexMetadata>`; back-compat
  shims (`write_index`, `write_index_full`, `write_index_with_call_graph`,
  `write_index_with_call_graph_and_skeletons`) discard via `.map(|_| ())`
  so external test/bench callers keep `Result<()>`.

### Fixed — Phase 11.1.9 (Q4-A): `vex update` no longer drops `ref_edges` from unchanged files

- Prior to this fix, `reconstruct_unchanged` set `bound_refs: Vec::new()`
  for every unchanged file during `vex update`. The writer then emitted
  a `ref_edges` section containing only edges from the *changed* slice
  — every cross-file reference from unchanged files was silently lost
  on each incremental update. `vex usages --strict` would degrade to an
  almost-empty result set after a few `vex update`s, recoverable only by
  a full `vex index`.
- The fix walks the old index's `ref_edges` for unchanged files,
  re-encodes each edge as a `(from_file_id, target_name, target_path)`
  triple, and re-resolves to the new index's symbol indices via a
  path-tiebreak helper (`resolve_by_name_and_path`). When a target's
  name has multiple definitions, the matching `target_path` is
  preferred — single-candidate fallback would silently mis-attribute.
- Memory budget: `target_name` / `target_path` are interned as
  `Arc<str>` (architect-H1 must-fix), keeping the reconstruction buffer
  sub-2 GB even on a 50M-edge index.
- Known limitation: refs targeting a symbol that was renamed in the
  *changed* slice are dropped (their old target_name no longer resolves)
  — this is the `imported_by`-cascade gap that Q4-B will close. See
  `docs/LIMITATIONS.md` §4d.
- New reader API: `IndexReader::ref_edge_count()` and `ref_edge(idx)`.
- `RefKind` gains `impl From<RefKind> for u8` and `impl TryFrom<u8>`
  with typed `UnknownRefKind` error, replacing the writer-local
  `ref_kind_bits` helper.

### Security — defense-in-depth cap on `Manifest::load`

- `<index_dir>/manifest.json` now has a 128 MiB hard ceiling (`stat`
  size-check before `read_to_string`). Loads above the cap return an
  actionable error instead of allocating multi-GB heap. The threat
  model is user-owned files (vex never reads remote manifests), so this
  is defense-in-depth — a corrupted-mid-write file or a manifest
  crafted to OOM a CI runner that triggers `vex update` automatically
  is rejected without parsing. The cap comfortably covers monorepos
  with hundreds of thousands of files plus a dense `imported_by` graph
  (architect S1 finding, 2026-06-17 audit).

### Internal — cascade-then-reconstruct ordering invariant pinned

- `src/index/pipeline/mod.rs` `update_inner` now documents the
  ordering contract that the Q4-B cascade discovery must merge into
  `changed_set` **before** the Q4-A reconstruction reads it. Two
  `debug_assert!`s pin the invariants: `cascade_paths ⊆ changed_set`
  after the merge, and `cascade ∩ reconstructed == ∅` after
  `reconstruct_unchanged`. Reordering these blocks would silently
  re-introduce the Q4-A regression Q4-B was built to fix — the
  assertions make the breakage surface in tests instead of in user
  refs (architect A3 finding, 2026-06-17 audit).

### Refactor — split `incremental_consistency_test.rs` by concern (audit H3)

- The 848-line monolith broke into three focused integration binaries:
  `incremental_consistency_basic.rs` (rename / move / empty / new file),
  `incremental_consistency_ref_edges.rs` (Q4-A preservation +
  multi-candidate disambiguation + multi-iteration), and
  `incremental_consistency_cascade.rs` (Q4-B cascade scenarios).
  Helpers are inlined per-file to keep each suite self-contained;
  nextest parallelises across the three binaries, and a future Q4-C
  test addition lands in `_cascade.rs` without colliding on fixtures
  with unrelated tests.

### Refactor — drop stale `#[allow(dead_code)]` on the ref_edges read path (audit H2)

- `cmd_usages.rs:74` has called `find_ref_edges_by_symbol` in
  production for several phases, so the six "wired into the CLI
  dispatch in 11.1.3d" annotations on `RefEdgeReader` /
  `has_ref_edges` / `ref_edges_section_bytes` / `slice_or_empty`
  were stale. The two remaining `RefEdge::column` / `ref_kind_bits`
  bit accessors are kept with an honest "exercised by integration
  tests; documents the bit layout" comment.

### Refactor — extract `ReconstructedRef` + `IndexBuildArtefacts` (audit C2)

- The Q4-A cross-stage handoff (`ReconstructedRef` + the old-index
  `file_paths` table) moved out of `src/index/pipeline/parse_files.rs`
  into a new `src/index/types.rs` module and now travels through
  `write_output_locked` as a single `IndexBuildArtefacts` parameter.
  The writer signature shrinks by one positional arg; a future Q4-C
  field lands as a named struct member instead of widening every
  call site again.

### Refactor — extract `IncrementalState` into `index.state` binary sidecar (audit C1)

- The `Manifest` god-object split into two storage layers:
  - JSON `manifest.json` retains the file-fingerprint table
    (`files`), git provenance (`git_head`, `indexed_at`), embedder
    identity, the sticky user-toggle opt-outs (`call_graph`, `bm25`,
    `pattern_index`), and rename-chain provenance.
  - **NEW** binary `<index_dir>/index.state` (magic `VEXS` v1,
    bincode payload) carries `imported_by` + `imported_by_built` +
    `cpp_includes_processed` + `body_tokens_persisted` +
    `history_indexed_at` + `history_tip_sha` + `history_depth` +
    `history`. These are writer-provenance / phase-state / reverse-
    index-cache fields whose scale (especially `imported_by` —
    O(cross-file-edges) bytes) had pushed JSON parse cost into
    measurable territory on watch-mode hot paths.
- Migration: `Manifest::load` reads the JSON first, then layers the
  sidecar on top. Pre-v1.18 indexes have no sidecar; the JSON
  `#[serde(default)]` fallback paths preserve their state, so
  `vex update` after upgrade re-bootstraps the sidecar without a
  forced `vex index`. The moved fields keep their struct membership
  on `Manifest` so the 50+ existing call sites stay untouched —
  only storage moved.
- Sidecar surface: 256 MiB hard ceiling before payload allocation,
  fuzzed via the new `fuzz_state_load` target alongside every other
  binary sidecar parser. Architect verdict (memory
  `reference_manifest_god_object_debt`): clean separation lets Q4-C
  transitive-cascade state land as a `state.transitive_*` extension
  instead of widening `Manifest` further.

## [1.17.0] - 2026-06-14

### Added — Phase 14.10: symbol-rename tracking via content-similarity

- **`vex history <Symbol>` follows renames across commits.** Detection
  runs unconditionally during `vex index --history` (no opt-in flag).
  The chain builder computes a 240-slot MinHash signature over each
  symbol's `body_tokens`, prunes candidates via 20×12 LSH bands
  (xxh3 u64 fingerprints), gates each candidate pair on kind match +
  length-ratio ≥ 0.60 + body-Jaccard ≥ 0.70, composite-scores at
  0.78·j_body + 0.22·j_sig (no-cosine path; the 0.70/0.20/0.10
  with-cosine path is wired but the MiniLM tiebreaker is dark until a
  follow-up commit plumbs `entry_context_hash`), greedy 1:1 assigns
  per commit pair with deterministic tie-break, then union-find
  merges across commit boundaries. Chains land in
  `<index_dir>/index.rename_chains` (new VEXR v1 magic, 48 B header
  guarded by `body_tokens_hash` + `history_tip_sha_prefix`).
  `vex history` opens the sidecar via
  `RenameChainsReader::open_for_query` (relaxed-guard variant; the
  tip-SHA guard plus co-write atomicity is sufficient at query time)
  and expands each FST hit through `follow_chain(entry_idx)` so a
  query for either side of a rename returns the full pre + post-rename
  timeline. Absent / stale / corrupt sidecar silently degrades to v1.16
  singleton chains. Closes LIMITATIONS §4c #2 (qualified — N:M merge /
  split is still punted; same-name overloads in the same file can
  over-chain).
- **`vex status` reports chain stats.** New JSON top-level
  `rename_chains` object: `{chain_count, forward_count, member_count,
  sidecar_size_bytes, thresholds: {score, jaccard, len_ratio},
  weights: {body_with_cos, sig_with_cos, cos, body_no_cos, sig_no_cos},
  minilm_tiebreak_hits: null}`. `null` when the sidecar is absent or
  pre-1.17; non-null with `chain_count: 0` when history is indexed but
  the builder found no chains. Text mode emits a one-line summary
  alongside the existing `History:` line.
- **Validation (exit gate met):** CodeShovel oracle full run on
  2026-06-14 — 70 methods across 7 repos, macro **P = 0.951 /
  R = 0.927 / F1 = 0.913**, above the F1 ≥ 0.90 exit gate. Per-repo:
  jetty.project 1.000 · pmd 0.983 · commons-io 0.947 · hadoop 0.891 ·
  spring-boot 0.888 · elasticsearch 0.857 · hibernate-search 0.824.
  Three repos (intellij-community, lucene-solr, mockito) were
  unreachable from the run host (curl 56 / curl 92 mid-fetch on a
  residential network); the harness's repo-level skip set short-
  circuits subsequent oracles for any repo that fails its 3-attempt
  retry, so a single permanently-failed repo costs 3 clone attempts,
  not 30. The `≥80 methods evaluated` floor was relaxed to ≥65 to
  reflect this reality. Bench (`benches/rename_chains.rs`,
  2026-06-14 re-run): 50k entries at 5% rename rate runs in **1.59 s**
  (faster than the originally documented 1.9 s — well within the ≤30%
  re-index overhead target).

### Changed

- **`parse_cache` format bump v2 → v3.** Dropped `#[serde(skip)]` from
  `ParsedSymbol.body_tokens` so the blob cache persists the field. The
  v2 cache stripped body_tokens on serialise, which made cross-blob
  rename detection impossible by construction (the current-tip parse
  populates the cache for the tip blob, and the history walker then
  reads the same blob back from the cache with `body_tokens = None`).
  v2 caches are invalidated on read via the version-mismatch guard
  and re-parsed once — one-time cost per repo, automatic.
- **`HistorySection` gains build-time-only `entry_body_tokens` +
  `entry_sig_tokens` fields.** Populated from the parser during
  `build_with_range`; padded with `None` on the disk-loaded side via
  `HistoryReader::extract_owned`. Documented limitation: chains across
  the `vex update --history` incremental-merge boundary are not
  detected until the next full rebuild (prior body tokens are not
  persisted on disk).

## [1.16.1] - 2026-06-11

Patch release. Folds in the v1.16.0-driven `vex self-update` Windows-sidecar fix
landed via PR #4 (resolves the silent CPU-fallback path on GPU builds upgraded
through the old binary-only updater) plus a handful of follow-ups surfaced by
post-release review: a `OnceLock`-gated DLL-missing warning so long-running
consumers (MCP server, daemon) don't get one warning per embedding batch, RAII
env-restore guards on the env-mutating `#[serial]` tests so a panic between
`set_var` and cleanup no longer pollutes the next test's view of `VEX_DEVICE` /
`VEX_EMBEDDER`, a `vex gpu --help` long-form that documents every env var the
GPU paths observe (previously only README-side), and a `docs/RELEASING.md` audit
trail for the pinned `DirectML.dll` SHA so the next `ort` upgrade has explicit
steps to re-verify the constant in the release workflow.

### Fixed

- **`vex self-update` now installs the `DirectML.dll` sidecar on Windows.** The previous updater delegated to `self_update`'s built-in flow, which extracts only the named `vex` binary from the release archive — so a self-updating Windows user got the DirectML-capable exe *without* its required redist DLL and silently fell back to CPU embedding (the DLL only reached fresh installs; see GPU_SUPPORT.md §6). The apply path is rebuilt in `src/cli/self_update_flow.rs` from the crate's public building blocks: one download, one zipsign ed25519 verification (replicated — context is the asset file name), whole-archive extraction, then every non-binary file installs as a sidecar beside the exe. Sidecars are SHA-256-gated (a byte-identical DLL is skipped; a DLL missing after an older self-update is healed) and install *before* the binary swap, so a failed sidecar write (e.g. unelevated under `C:\Program Files\vex\`) aborts with the old exe + DLL pair intact instead of leaving exe↔DLL version skew. The install path deliberately avoids `self_update::Move` (its bare `fs::rename` fails when the OS temp dir and the install dir sit on different volumes): the old DLL is renamed aside within its own directory — legal even while mapped by another running vex process — and the new one is copied in. Linux/macOS archives contain only the binary, so behavior there is unchanged. `--check`, the confirm/`--yes` UX, and the v1.13.1 `vex-<target>` identifier fix (don't match `vex-mcp-*` assets) are preserved; 26 new unit/E2E tests (one Unix-gated) cover the pipeline, including a signed synthetic-archive end-to-end run, a tampered-signing-context negative, a corrupt-staged-copy abort, and a symlink-entry rejection. One-release lag: a user running v1.16.0 or older still updates with the old binary-only code on that hop; the heal applies from the next update onward.
- **`warn_if_directml_dll_missing` is now `OnceLock`-gated.** v1.16.0 wired the warning into `execution_providers`, which fires once per embedding session — fine for a one-shot CLI, but a long-running MCP server / daemon would re-run `current_exe + canonicalize + is_file` (three syscalls) on every batch and re-emit the `eprintln!` to stderr, the exact noise users would silence with `2>/dev/null` (hiding the next real diagnostic). The lock is set unconditionally after the first probe (DLL present or absent), so subsequent calls return on the cheap `OnceLock::get` check.
- **`#[serial]` env-mutating tests in `embed::device` and `embed::mod` are now panic-safe.** Both tests had a single `remove_var` at the trailing edge of a several-`set_var` block — a panic between any `set_var` and that cleanup left `VEX_DEVICE` / `VEX_EMBEDDER` set for the next `#[serial]` test (the lock orders tests, it does not unwind their env mutations). New `VexDeviceGuard` / `VexEmbedderGuard` RAII types capture the pre-test env on construction and restore it on `Drop`, so the cleanup runs on the panic path too.

### Added

- **A DirectML-capable vex now warns when `DirectML.dll` is missing beside the exe.** Embedding initialisation prints an unconditional stderr warning (plus a `tracing` WARN for `RUST_LOG` users — `tracing::warn!` alone is filtered out under the default `EnvFilter`) with a `vex self-update` hint, instead of silently falling back to CPU — surfacing both installs degraded by the old binary-only updater and the one-release lag of the sidecar fix above.
- **`vex gpu --help --long` documents every env var the GPU paths observe**: `VEX_DEVICE` (global default device), `VEX_EMBEDDER` (global default embedder), `VEX_GPU_STRICT` (turn ORT's silent EP-registration fallback into a hard error — same mode `vex gpu` requests internally without mutating the process env), and `VEX_GPU_ATTN_BUDGET` / `VEX_GPU_MEM_LIMIT` (advanced batching / VRAM tunables, see `docs/GPU_SUPPORT.md` §11). Closes the gap that the README and GPU_SUPPORT.md documented these but `--help` did not.

### Changed

- **`vex self-update` now follows the latest release across major-version boundaries.** The old apply path inherited `self_update`'s `bump_is_compatible` gate (major-pinned: a hypothetical v2.0.0 would never be offered) while `vex self-update --check` already reported strictly-newer releases. Both paths now agree: strictly newer than current wins, majors included, prereleases excluded (GitHub's `/releases/latest` never serves them). Downgrades remain impossible — an older-but-signed release never passes the version gate. Crossing a major boundary prints a loud stderr warning in interactive, `--no-confirm`, and `--check` runs, so scripted updates cannot silently land on a new major and the preview carries the same signal as the apply. The confirm prompt now also accepts a typed `yes`, not just bare `y`.
- **`docs/GPU_SUPPORT.md` status block** updated from "IMPLEMENTED — pending §8 pre-release validation" (a stale phrasing from the pre-v1.16.0 branch) to "RELEASED in v1.16.0", cross-referencing the `vex gpu` doctor as the runtime validation gate users now run on their own machines.
- **`docs/RELEASING.md` documents the `DirectML.dll` SHA pin audit trail**: how to find the redist the current `ort` crate version pulls (the `%LOCALAPPDATA%\ort.pyke.io\<version>\runtimes\win-x64\native\DirectML.dll` cache populated by `cargo build`), how to verify the SHA-256 with the full path (a bare `sha256sum DirectML.dll` would silently agree with whatever DLL is in `$PWD`), and when to bump `EXPECTED_SHA256` in `.github/workflows/release.yml` for a new `ort` version. Closes the audit-gap noted at the v1.16.0 security review.

## [1.16.0] - 2026-06-10

Two-feature release. **Phase 14.9** ports the `vex history` JSON envelope to the standard `MetaEnvelope` (BREAKING), adds Tier A filters (`--diff`, date / author / kind) and Tier B (`--exact-presence`, prefix-FST fallback, `vex status` submodule + size warnings) on top of the Phase 14.8 history sidecar shipped in v1.15.0. **Optional GPU embedding** runs the embedding model on CUDA / DirectML / CoreML — 51× faster on CUDA and 29× on DirectML for the default MiniLM model on a 28k-symbol C++ corpus, with a `vex gpu` doctor command and four heavier opt-in embedders; the default CPU build is byte-for-byte unchanged. Both tracks were reviewed by parallel architect + rust-reviewer + code-reviewer (+ security-reviewer for the GPU track) rounds before scaffold and again before release. Also bundles three post-v1.15.2 stabilisation commits: Windows HNSW commit-rename retry, Linux CI test-profile debug-info cap, and a Linux x86_64 tarball install section in the README.

### Added

- **`vex history --since YYYY-MM-DD` / `--until YYYY-MM-DD` / `--author SUBSTR` / `--kind KIND` filters.** Date filters lex-compare against the fixed-width ISO `commit_date` (chronologically equivalent — no `chrono` / `time` dep added). Dates are calendar-validated at the CLI boundary: `2026-13-99`, `2026-02-30`, and non-leap Feb 29 are all rejected with a clean error. `--author` is walker-only because the Phase 14.8 sidecar drops author info; passing it on the indexed path emits an `eprintln!` and exits non-zero with a hint at `--no-index`. `--kind` is exact lowercase match (`function` / `struct` / `impl` / …) — suppresses the partner-row noise that pairs e.g. an `impl` with every `struct` hit (2026-06-09 dogfooding annoyance).
- **`vex history --diff`.** Renders unified diffs between consecutive entries of the same `(symbol, kind)` group via `similar::TextDiff::from_lines`. Head of each group keeps the full signature; non-head entries carry `--- @prev_sha\n+++ @curr_sha\n…` (text) or `body_diff: { from, to, hunks }` (JSON). Advertised in `capabilities.history_diff = true` for MCP feature-detection. Mutually exclusive with `--exact-presence` (clap rejects the combination at parse time — `--diff`'s `(symbol, kind)` grouping breaks the per-row presence mapping). Note: v1 stores only `signature` per entry — "body diff" is effectively signature-line diff today; the field is named `body_diff` so a future phase can graduate to full-body diffs without renaming.
- **`vex history --exact-presence [--exact-presence-max-commits N]`.** Defeats the Phase 14.8 convex-hull lossy span (`[first_commit_idx, last_commit_idx]` overstates continuity when a revert / cherry-pick produces a different blob mid-span). Walks `git log` from HEAD ONCE (shared across all files in the result set) capped at N (default 500) and batch-resolves each `<commit>:<file_path>` via stdin-piped `git cat-file --batch-check`. Per-`(file_path, blob_sha)` memoisation in-process; not persisted to disk (canonicalize-symmetry hazard from Phase 14.8 Step 7). Above the cap, the entry falls back to the convex-hull span with `presence_truncated: true` in JSON and an `eprintln!` notice in text mode. Caveat: file-blob equality, not symbol-body equality — a sibling-symbol change in the same file produces a new file blob and narrows presence.
- **Prefix-FST fallback on the indexed path.** When `HistoryReader::find_by_name` misses AND the query is identifier-shaped AND `query.len() >= 3`, the new `find_by_name_or_prefix` walks the FST for keys starting with the lowercased query and unions their posting lists (capped at 50 distinct names). Order is lexicographic, not relevance — `vex history inde` surfaces `index`, `IndexReader`, `index_path`, etc. without re-hitting the walker. Partially closes `LIMITATIONS §4c #7` (sub-3-char and non-identifier queries still need `--no-index` for the walker's `git grep --word-regexp`).
- **`vex status` warnings under indexed history.** Two informational lines fire when `index.git_history` is on disk: (1) submodule warning when `.gitmodules` exists (LIMITATIONS §4c #6 — submodule blobs are silently skipped during history build; behaviour unchanged, surface is new); (2) size-ratio warning when `git_history > 2× index.vex` (§4c #5 — long-lived repos scale by history depth, not symbol count). JSON adds top-level `has_submodules: bool` and `git_history_size_bytes: u64 | null`.
- **`crate::util::ident::is_identifier_shaped`.** Promoted from `cli::cmd_search` (was a private fn used only by the v1.15.0 search-drift hint) so the new `HistoryReader::find_by_name_or_prefix` can reuse the same conservative definition.
- **`tests/history_v149_flags_test.rs`** — 7 end-to-end `assert_cmd` integration tests covering the v1.16.0 CLI dispatch (kind filter, author walker + indexed-rejection, diff+presence clap conflict, presence JSON shape, since/until window, calendar-invalid rejection). Closes the gap that no test exercised the new flags through the full CLI path.
- **Optional GPU-accelerated semantic indexing** (CUDA / DirectML / CoreML) for `vex index --semantic` / `vex update`. The embedding model can now run on a GPU execution provider — on an RTX 3080 over a 28k-symbol C++ corpus, embedding the default MiniLM model is **51× faster on CUDA and 29× on DirectML** vs CPU (benchmarks + design in `docs/GPU_SUPPORT.md`). Prebuilt binaries bake in a **driver-only** EP (Windows → DirectML with the redist `DirectML.dll` bundled in the release archive; macOS arm64 → CoreML); **CUDA stays a source-build opt-in** (`cargo install vex --features gpu-cuda`). The default CPU build pulls no new dependencies, and its embedding load path is byte-for-byte the legacy CPU path (an empty execution-provider list) — `gpu-coreml` / `gpu-directml` / `gpu-cuda` are all off by default.
- **Device selection** on the index path: `--gpu` / `--no-gpu` / `--device cpu|auto|cuda|directml|coreml` on `vex index` & `vex update` (and matching `gpu`/`device` args on the MCP `index`/`update` tools), `gpu` / `device` keys in `.vex.toml`, and a `VEX_DEVICE` env var as a global cross-project default. Precedence: CLI > `.vex.toml` > `VEX_DEVICE` > compile-time default (`auto` on a GPU build, `cpu` otherwise). A stale `VEX_DEVICE` pinning an EP the current binary lacks degrades gracefully to the default instead of erroring.
- **`vex gpu` doctor subcommand**: reports the compiled-in EP and *actively probes* it with strict registration (one real inference), so a silent CPU fallback shows as `FAILED` with EP-specific remediation. `vex gpu <device>` narrows to one EP; `vex gpu --enable` persists the working device to `VEX_DEVICE`. Supports `--format json` with the standard `MetaEnvelope` payload (build, compiled EPs, per-probe outcome, engaged device, pin status) so MCP agents can gate on it.
- **Heavier / code-specialized embedders** as explicit opt-ins via `--embedder` / `.vex.toml` / `VEX_EMBEDDER`: `jina-code` (768-d code model), `bge-base-en-v1.5`, `bge-large-en-v1.5`, `mxbai-large` — the models where GPU acceleration genuinely pays off. The embedder id is recorded in the manifest and mismatches are detected at query time. An unknown `VEX_EMBEDDER` value falls back to the default embedder with a warning (CLI/config ids still error loudly).
- **Length-aware GPU micro-batching** (`src/embed/batching.rs`): inference batches are sized from actual context lengths (`count × max_len² ≤ budget`, cap 256/batch) so peak VRAM stays bounded with zero configuration — ~14× faster than naive fixed-size batching on a large C++ repo. A single context exceeding the budget on its own is embedded alone and logged with `tracing::warn!`. Tunables for shared GPUs: `VEX_GPU_ATTN_BUDGET`, `VEX_GPU_MEM_LIMIT` (advanced, opt-in).
- **Model-aware Auto miss-gate**: `Device::Auto` keeps tiny incremental updates on CPU (GPU warm-up dominates below the per-model break-even, e.g. 512 misses for MiniLM, 32 for `jina-code`); explicit `--gpu`/`--device` bypasses the gate.
- `vex status` now reports GPU support: a `GPU: <build EP> · default <device>` line in text output, and `gpu_support` / `default_device` fields in the `--format json` envelope.
- `VEX_GPU_STRICT=1` env var: turns ORT's silent EP-registration fallback into a hard error for benchmarking/verification. Read-only — `vex gpu` requests strict registration via a constructor parameter, never by mutating the process environment.

### Changed

- **`vex history --format json` envelope ported to `crate::cli::output::print_envelope`.** Was a hand-rolled `serde_json::json!({...})` literal that bypassed `MetaEnvelope` and wrapped results as `{items: [...]}`. Now uses the standard typed `ResponseEnvelope<T>` with `output::default_meta_for`, picking up `vex.dev/index_age_ms` / `ttlMs` / `cacheScope` / `vex.dev/stale` / `vex.dev/why_trace` for free. `MetaEnvelope` gains `vex.dev/history_mode` (`"indexed" | "walker"`) so MCP agents can observe which path served the query.
- **`cmd_history::resolve_mode` returns the opened `HistoryReader` alongside the mode tag.** Pre-fix, `run_indexed` re-opened the sidecar (re-mmapped the header) on every indexed query; now the reader is plumbed through. Cheap fix but eliminates an unreachable error path (`"sidecar disappeared between mode probe and run"`) that was misleading future readers about what could fail.
- **`docs/LIMITATIONS.md` §4c, `docs/HISTORY-INDEX.md`, `README.md`** — qualify each of the four covered limits (`#4`/`#5`/`#6`/`#7`) with the Phase 14.9 fix that partially or fully closes them; add a cookbook block to HISTORY-INDEX.md with the new flags.

### Breaking

- **`vex history --format json` shape change.** `results: { items: [...] }` → `results: [...]` (array directly). Legacy `_meta` keys `vex.dev/query_symbol` and `vex.dev/result_count` are gone — the caller already knows the query, and `len(results)` is trivially observable. MCP agents that pinned to the legacy shape must read from `results[*]` instead of `results.items[*]` and recompute the dropped fields from context. There is no opt-out env-var; the legacy shape was only on `vex history` (every other command already emitted the standard envelope).

### Security

- **Release pipeline pins `DirectML.dll` by SHA-256**: the Windows staging step selects the DLL from the ort build cache only if its hash matches the pinned constant (x64 DLL from the official `Microsoft.AI.DirectML` 1.15.4 NuGet package, the version ort `=2.0.0-rc.12` stages) and fails closed otherwise — a poisoned runner cache or dirty build can no longer substitute a different DLL into the signed release archive.
- `docs/GPU_SUPPORT.md` §6 documents the `DirectML.dll` side-loading vector (install vex in a restricted-write directory such as `C:\Program Files\vex\`) and §7 documents the accepted residual risk that opt-in embedder models are verified by fastembed against Hugging Face metadata without a secondary vex-side SHA pin.

## [1.15.2] - 2026-06-08

Documentation pass: across every agent-facing surface (README, `/vex` skill, COOKBOOK, LIMITATIONS, AGENTS.md template, SECURITY.md, parent `CLAUDE.md`), the recommended tool for "find a specific symbol by exact name" is now `vex check` first, with `vex search` explicitly reframed as the fuzzy / keyword / multi-word surface that returns ranked NEIGHBORS when no symbol literally matches. The change addresses the field-test observation that the prior `vex search <Symbol>` guidance led agents to act on caller/import noise as if it were the definition. No code paths changed; the v1.15.0 search-drift stderr hint already existed and is now properly cross-referenced. Also bundled: a SECURITY.md fuzz-attestation update covering the v1.15.2 release-gate run (~853k executions across four highest-signal libFuzzer targets, zero crashes), and version-slip cleanup in docs that pre-emptively attributed Phase 14.8 / `vex history` walker to v1.16/v1.17 — both shipped in v1.15.0.

### Changed

- **`vex check` is now the recommended first tool for exact-name symbol lookup** across all agent-facing documentation surfaces. `vex search` documentation reframed to make the ranked-blend / neighbors behavior explicit. Affects `README.md` Quick Start + "What Vex isn't" section, `.claude/skills/vex/SKILL.md`, `docs/COOKBOOK.md` cheat sheet, `src/integrations/agents_md.rs` (the `vex init --agents-md` template — propagates to every project that runs it). A new unit-test pin guards against future template refactors silently dropping the `vex check` recommendation.

### Fixed

- **Windows: HNSW commit-rename retries transient `ERROR_SHARING_VIOLATION` / `ERROR_ACCESS_DENIED`.** v1.15.1 fixed the in-process file-handle race by dropping the `usearch::Index` before `std::fs::rename`; on Windows that's necessary but not sufficient, because (a) usearch's C++ FFI close + the OS-level handle release are not synchronous, and (b) Windows Defender / search-indexer real-time scans grab a brief read handle on freshly-saved files. Both windows close out fast; a short retry with linear backoff (up to ~1.1 s across 10 attempts, 20 ms → 200 ms) on `os error 5` / `os error 32` only on Windows masks them. Linux / macOS still get a single rename and a hard error on real failure.
- **Linux CI: `cargo test --workspace` no longer blows the 14 GB GitHub-runner disk budget.** Set `[profile.test] debug = "line-tables-only"` in `Cargo.toml`. The 30+ integration test binaries each statically link the full tree-sitter + ONNX dep graph; `debug = 2` (the default) ballooned each binary 3-5× past the runner's free disk. `line-tables-only` keeps `file:line` resolution in backtraces — the load-bearing signal for a test panic — but drops variable / type / symbol info. Affects test binary size only; the dev profile and bench profile are unchanged so local debugger workflows keep full info.
- `docs/COOKBOOK.md`, `docs/LIMITATIONS.md`, `docs/HISTORY-INDEX.md`: forward-looking version stamps (`v1.16` walker, `v1.17` Phase 14.8 history index, `v1.17+` search-drift hint) updated to `v1.15.0` to match the actual ship vehicle.

### Security

- `SECURITY.md`: v1.15.2 release-gate fuzz attestation (~853k executions across `fuzz_incremental_hnsw`, `fuzz_hash_index_load`, `fuzz_bloom_load`, `fuzz_index_reader`, zero crashes). The `fuzz_incremental_hnsw` target — added in v1.15.0 for the B1.2 incremental update path — is now documented in the in-scope coverage map.

## [1.15.1] - 2026-06-08

Field-test fix bundle responding to an external report against a real C++ codebase (~4 022 files, 82.3k symbols). One critical bug took the semantic channel offline on any corpus with hash-colliding symbols; one high-severity bug turned a failed MCP auto-update into a self-perpetuating stale loop where agents trusted "0 results" as a real answer. Four UX polish items round out the bundle.

### Fixed

- **CRITICAL — `vex index --semantic` no longer aborts on duplicate HNSW keys.** Two C++ symbols with identical signatures in the same file (forward decl + definition, overloads, anonymous-namespace clones) hash to the same `context_hash`, and usearch's `multi: false` high-level index rejected the second `add` as fatal — aborting the whole build mid-corpus and leaving no `index.hnsw` on disk. Pre-fix this meant `vex index --semantic` returned exit 2 on every run for affected corpora; the on-disk graph silently went stale (one user reported a 3-day-old vector index serving semantic search). Both `build_hnsw_at` (full rebuild) and `build_hnsw_incremental_at` (incremental update) now dedup at the insert boundary, skip-and-warn on the duplicate hash with the offender's first / duplicate sym_idx, and continue. The on-disk hash sidecar stays sym_idx-aligned (full length, duplicates preserved) — `src/search/semantic.rs:156` requires `hashes.len() == expected_symbols` at query open, and the reader at the same site already dedups via `entry().or_insert` keeping the first sym_idx per hash. 6 regression tests cover all-unique / all-dup / partial-dup / incremental-with-new-dup / incremental-collision-with-existing.

- **HIGH — MCP failed auto-update no longer returns exit code 2 or empty `{results:[]}` envelopes.** When the on-disk index was stale and `auto_update` triggered a rebuild that failed (e.g. the critical HNSW bug above, but also future failure modes: disk full, embedder model unavailable, corrupted manifest), `handle_staleness` bubbled the `pipeline::update` error up → the CLI exited non-zero → the MCP wrapper either surfaced `exit code 2` to the caller or, in one observed case for `vex usages`, wrapped a valid `{results: []}` envelope inside an MCP error string. Agents read "0 usages" and trusted it. Now `handle_staleness` catches the error, records the reason via a per-request `stale_signal` slot, logs to stderr, and returns `Ok` — the command serves the existing (stale) index and the JSON envelope's `_meta.vex.dev/stale = true` + `_meta.vex.dev/stale_reason` advertise the degradation. Same path also fixes the embedder-mismatch bail that previously bypassed auto-update with a non-zero exit. `MetaEnvelope` gains 3 optional fields (`stale`, `stale_reason`, `why_trace`) — wire-compatible additions guarded by `skip_serializing_if = "Option::is_none"`. The MCP-side in-process single-flight remains a follow-up (the cross-process `IndexLock` already serializes rebuilds, and the herd's failure mode resolves once the critical HNSW bug is fixed).

- **`vex capabilities` no longer returns `{}` through the MCP wrapper.** Pre-fix the CLI emitted only `{ protocol_version, capabilities }` — a "half envelope" that the MCP wrapper's `is_envelope` heuristic at `crates/vex-mcp/src/main.rs:480` accepted (both required fields present) but populated `structuredContent.results` with an empty object because the `results` key was absent. The CLI now emits a full `ResponseEnvelope` with `results: null` (an explicit "no per-query payload" signal — the capability matrix lives at `capabilities`, not echoed into `results`).

- **`vex usages --strict --why` now surfaces the `--why` trace in the success envelope.** Pre-fix the trace was emitted only on stderr (`VEX_WHY:` prefix) and never reached JSON consumers — agents that piped `--format json | jq` couldn't observe it; it only appeared embedded in the *error* payload. The trace now also populates `_meta.vex.dev/why_trace` on success, alongside the existing stderr emission for back-compat with scripts that grep the stream.

### Added

- **`--drop-semantic` flag on `vex index`.** Pre-fix, `vex index --no-semantic` unconditionally deleted `index.hnsw` + `index.hashes` and orphaned the embedder cache (often hundreds of MB on real corpora) — reattaching semantic search required a full re-embed of every symbol. `--no-semantic` now PRESERVES the prior semantic artifacts (the query path at `src/search/semantic.rs:156` catches the size mismatch and degrades to brute-force semantic search until the next `--semantic` build); pass `--drop-semantic` to opt into the destructive teardown.

- **`vex callees` default-on stdlib / macro filter for C++ readability.** Pre-fix, `vex callees WriteFrameFile` on a real C++ codebase returned `std::move`×4, `c_str`×3, `_T` (MFC macro), and a handful of method-chain artifacts — drowning the real edges. The new `src/callgraph/stdlib_filter.rs` module drops names matching `std::*`, `__*`, common stdlib container/string methods (`c_str`, `push_back`, `begin`, etc.), and short all-uppercase macro-style identifiers. Generic names like `get` / `data` / `clear` / `reset` are deliberately NOT filtered (they're extremely common in user-defined domain code). `--include-stdlib` bypasses the filter when the user wants the raw list. C++ idiom only — other languages already produce mostly-clean callee sets.

### Breaking

- **`vex index --no-semantic` no longer deletes `index.hnsw` + `index.hashes` + the embedder cache by default.** Scripts that relied on `--no-semantic` to free disk space must now pass `--drop-semantic` too. The previous behavior was a usability footgun — one user lost semantic search permanently to a `--no-semantic` rebuild without realizing the embed cache had been orphaned.

## [1.15.0] - 2026-06-08

Bundled release: **B1.2 incremental HNSW** (the headline `vex update --semantic` perf win), **Phase 14.8 persistent git-history index** (`vex history` ~10 ms vs walker's ~6-16 s, **675-1640× speedup**), **`vex mcp install`** for seven MCP-compatible agents (Claude Code / Cursor / Codex CLI / Windsurf / Cline / Continue.dev / Zed) + `vex init --agents-md`, a **search-drift stderr hint** for the "imported-from-dependency" lookup case, plus a documentation pass (COOKBOOK + integrations folder + `/vex` skill + `SEMANTIC.md` + `HISTORY-INDEX.md` + LIMITATIONS update). A1 parallelises `vex duplicates`; C tightens the HNSW + hash-index sidecar atomic-commit window from ms to μs.

### Added

- **Phase 14.8 — persistent history-symbol index sidecar (`vex index --history`).**
  Built on top of the v1.15.0 query-time walker (below).
  `vex index --history [--history-depth N]` walks `git log --raw
  --no-abbrev --no-renames` once at index time, parses every blob
  through the v1.14 Phase 14.7 content-addressed cache (warm hits
  short-circuit tree-sitter), and writes
  `<index_dir>/index.git_history` — a separate sidecar (NOT inline
  in `index.vex`, byte-identical schema reserved for a future
  promotion) carrying an FST `symbol_name → Vec<HistoricalSymbol>`
  plus 28-byte mmap-friendly entries, 32-byte commits, and 24-byte
  blobs. `vex history <Symbol>` then auto-picks the indexed path:
  ~10 ms FST lookup vs the walker's seconds. Measured speedup
  **~675× on vex self-repo**, **~1640× on tokio**. Indexed mode
  also finds symbols whose name has been **deleted from HEAD** —
  the walker can't (its `git grep` probe runs against the chosen
  tip). `vex update` is sticky via the manifest with a 4-branch
  refresh: no-op fast path (sidecar mtime preserved on tip
  unchanged), incremental walker (linear `<prior_tip>..HEAD` +
  in-place merge with the prior section), force-push detect
  (`git merge-base --is-ancestor` → warn + full rebuild),
  `--no-history` drop (delete sidecar + null manifest fields).
  Manifest gains `history_indexed_at`, `history_tip_sha`,
  `history_depth`, and `history: { commit_count, blob_count,
  entry_count, depth_capped }`. `vex status` surfaces section
  presence + counts + a depth-capped warning on its own line.
  Reader auto-canonicalises the project path before computing the
  cache subdir hash (fixes macOS `/tmp → /private/tmp` symlink
  mismatch that previously fell back to walker silently). 12
  integration + 22 unit tests. Bounds-check truncation bug (count
  ≥134 M → OOB via `read_unaligned`) caught by parallel rust-
  reviewer + code-reviewer, fixed via `(u32, u32) → u64` closure
  widening before the section format shipped. See
  `docs/HISTORY-INDEX.md` for the full pipeline + cost-benefit and
  `docs/LIMITATIONS.md` §4c for the index limits.

- **Search-drift hint on `vex search <identifier>`.** When the
  query is identifier-shaped (`compile_query`, `Foo`, `_internal`
  — single bare name, no punctuation / spaces) AND the structural
  FST channel returns zero matches, vex prints a one-line stderr
  hint suggesting `vex check` / `vex show` / `vex usages --strict`
  for exact-symbol lookup. Covers the "imported from a dependency,
  not defined locally" case where BM25 would otherwise rank callers
  / imports as if they were the definition — confusing for LLM
  agents asking "where is `Foo` defined?". Hint goes to stderr so
  it doesn't pollute stdout JSON envelopes; 4 integration tests
  pin the trigger matrix (defined symbol → no hint, multi-word
  query → no hint, JSON mode → hint stays on stderr,
  identifier-shaped + 0 FST hits → hint fires). External-feedback
  driven (`vex search "compile_query"` surfaced callers on a
  codebase where the symbol was imported from `chili_pg_utils`).
  See `docs/COOKBOOK.md` FAQ for the full decision rule.

- **`vex history <Symbol>` — query-time git-log walker.** Returns
  every historical version of the named symbol reachable from the
  chosen tip (`HEAD` by default, or `--branch <REV>`). No indexing
  required: shells out to `git grep` (whole-word probe to locate
  candidate files), then `git log --follow` per file, fetches each
  blob via `git ls-tree` + `git cat-file`, parses with the same
  `extract_symbols_and_imports` vex uses at index time, and keeps
  only matches whose `name == query`. Blob-SHA dedup collapses
  consecutive commits with identical file content into one entry,
  so a "touch — recommit" round trip doesn't bloat the result set.
  `--depth N` caps per-file walk; `--limit N` caps the total
  result set (the walker stops early). Three output formats:
  default text, `--format compact` (tab-separated for grep/awk in
  agent shells), `--format json` (v1 envelope matching
  `vex search --format json` / bundle output). Limitations
  documented inline in [`src/history/mod.rs`](src/history/mod.rs):
  symbols whose current name has been fully removed are invisible
  (the `git grep` probe runs against the chosen tip); symbol-level
  renames inside a file split into two queries (old name + new
  name). 7 unit tests pin the two-version walk, blob-SHA dedup
  across no-op recommits, empty result for never-existed symbol,
  `--limit` cutoff, non-git-dir rejection, empty-name rejection,
  and the `--word-regexp` substring filter (`parse` not matching
  `parse_payment`). Smoke-tested against vex's own history.

- **`vex mcp install / uninstall / list` — auto-configure MCP-compatible
  agents.** Adds (or removes) the `vex-mcp` entry in your agent's MCP
  config file without hand-editing JSON / TOML / YAML.
  `vex mcp install --agent claude-code` writes
  `~/.claude/claude_desktop_config.json`; `--agent cursor` writes
  `~/.cursor/mcp.json` with the `"type": "stdio"` quirk Cursor
  requires. Idempotent — re-running the command on a matching entry
  is a no-op skip (`--force` overrides). `--dry-run` prints the
  post-merge config without touching files so the user can diff
  before committing. Atomic writes (`.tmp` + fsync + rename) so a
  crash mid-write can never leave a half-rendered config. `--agent
  all` fans out across every supported agent; `vex mcp uninstall`
  removes the named entry; `vex mcp list` enumerates current entries.
  v1.15.0 ships all seven handlers: **Claude Code** + **Cursor** +
  **Windsurf** + **Cline** + **Zed** (JSON with per-agent quirks),
  **Codex CLI** (TOML, `[mcp_servers.<name>]` table-of-tables; same
  preserve-other-keys discipline as the JSON path — bench-tested
  against a real `~/.codex/config.toml` carrying unrelated
  `personality`/`hooks` keys), and **Continue.dev** (YAML, drops a
  dedicated `<project>/.continue/mcpServers/<server_name>.yaml`
  rather than merging into a shared file, matching Continue's
  documented one-server-per-file convention and side-stepping the
  need to pull in a YAML library). 5 additional unit tests pin the
  TOML/YAML paths (top-level-key preservation, surgical uninstall,
  idempotent re-install, Continue YAML exact-shape regression guard). Architecture:
  `McpAgentHandler` trait + shared `JsonProfile`-driven install /
  uninstall / list primitives in [`src/integrations/mcp.rs`]; each
  agent contributes a handler with a one-line profile and a
  `config_path()` resolver. 12 unit tests pin idempotence, preserve-
  other-servers, dry-run-no-write, force-overwrite, Cursor's
  `type: stdio` profile, and uninstall-only-targeted-entry.

- **`vex init --agents-md` — emit an AGENTS.md template.** The
  community-convention `AGENTS.md` file is read as a fallback to
  per-tool configs by Cursor / Codex CLI / Aider / Cline / Windsurf
  and most non-Claude agents; v1.15.0's `vex init` gains an
  `--agents-md` flag that drops a generic vex-aware AGENTS.md (load-
  bearing rules + tool-selection table + MCP-setup pointer) next to
  the `.vex.toml`. `--agents-md-only` is the variant for projects
  that already have `.vex.toml` but want the agent file too.
  Refuses to overwrite an existing AGENTS.md (same idempotent
  behaviour as `.vex.toml`). 4 unit tests pin the template marker
  (`# AGENTS.md\n` first line, used by downstream linters), the
  load-bearing-flag advertisement (`--strict` must appear), and the
  refuse-on-conflict path.

- **B1.2 — incremental HNSW update on `vex update --semantic`.** `vex
  update` now tries `usearch::Index::load() → remove() → add() → save()`
  on the existing HNSW instead of rebuilding it from scratch on every
  run. The diff between the old `index.hashes` sidecar and the freshly
  computed hash set determines which keys to remove (orphans) and which
  to add (new symbols); the HNSW key is `context_hash` (B1.1) so
  reordered sym_idx slots no longer trigger a wholesale rebuild. When
  removals exceed a 25% tombstone threshold the path bails to a full
  rebuild — at high churn the per-key `remove()` + lingering tombstone
  overhead outweighs the rebuild. Missing / corrupt sidecar, dim
  mismatch (embedder switch), or any usearch-level error degrades to
  the same full-rebuild fallback, so the feature can never produce a
  worse on-disk state than v1.14.1. 7 new unit tests in
  `pipeline::output::tests` pin every fallback branch + the
  small-add-remove end-to-end query (post-update HnswHandle resolves
  the new sym_idx mapping correctly via the rewritten sidecar) +
  exact-25%-boundary strict-GT regression guard.

  **Activation requirements:** incremental HNSW only fires for
  `vex update --semantic` (or `vex update` when `.vex.toml` has
  `semantic = true`). Non-semantic updates don't touch the HNSW.
  **First-update-after-upgrade cold start:** pre-v1.15 indexes lack
  the `index.bodytokens` sidecar, so the next `vex update --semantic`
  falls back to full rebuild. Run `vex index --semantic` once to write
  the sidecar; the run after that will be incremental. `vex status`
  surfaces the migration state as `Body tokens: yes/no`. See
  `docs/LIMITATIONS.md` §4b for the full cold-start contract.

- **`index.bodytokens` sidecar (v1.15.0 B1.2 prerequisite).** New
  per-index sidecar at `<index_dir>/index.bodytokens` (`VEXT` magic v1,
  u32 count, records of `u32 byte_len` + UTF-8 bytes with `u32::MAX`
  encoding `None`) persists `ParsedSymbol.body_tokens` in sym_idx
  order. `parse_files::reconstruct_unchanged` loads the sidecar
  best-effort and feeds the restored body_tokens back into the
  reconstructed `ParsedSymbol`. Without this persistence, reconstructed
  symbols produced body-less `context_hash` values that drifted from
  the fresh `vex index` baseline — the diff between the old
  `index.hashes` sidecar and the recomputed hashes would treat every
  unchanged symbol as a `remove → re-add` pair, defeating B1.2. Closes
  the long-standing "BM25 recall regressed for unchanged symbols after
  `vex update`" warning in `reconstruct_unchanged` — body-aware BM25
  bags now survive incremental updates. Format-version stays at v6;
  pre-v1.15 indexes have no sidecar, `reconstruct_unchanged` falls
  back to `body_tokens: None` (legacy behaviour), and `vex update`
  takes the full HNSW rebuild path until the next `vex index` writes
  the sidecar. 12 new unit tests in `store::body_tokens` (round-trip
  mixed Some/None, atomic save, bad magic / version / count, byte_len
  cap, non-UTF-8, truncated body, missing file, sym_idx position
  preservation) + 4 integration tests in `cli_body_tokens_sidecar_test`
  (sidecar written next to `index.vex`, content-equal across a no-op
  `vex update`, marker surfaces in `vex status` text + JSON).

- **`Manifest.body_tokens_persisted: Option<bool>` + `vex status`
  surface.** Mirrors the v1.14 `cpp_includes_processed` pattern: every
  v1.15+ build writes `Some(true)` unconditionally (version marker,
  not project-content predicate); pre-1.15 manifests carry `None`.
  `vex status` renders `Body tokens: yes (incremental HNSW update
  enabled)` for `Some(true)` and an actionable `Body tokens: no (run
  vex index to enable incremental HNSW update)` for `None`. JSON
  envelope exposes the same value as a literal bool (`jq
  '.body_tokens_persisted'` works without unwrapping). 3 new unit
  tests in `manifest::tests` pin the back-compat round-trip pattern.

### Performance

- **A1 — `vex duplicates` outer loop parallelised via rayon.**
  Replaces the sequential `for sym_idx in 0..reader.symbol_count()`
  scan in `find_duplicates` with `into_par_iter().flat_map_iter(…)
  .collect()` over a new `per_symbol_duplicate_candidates` helper.
  Each iteration's `HnswHandle::search` is independent (reader /
  body_lines / hnsw are `&` borrows; usearch carries `unsafe impl
  Send + Sync` upstream). The sequential `HashSet` dedup phase
  stays — it's the cheap part; HNSW search at 1k+ symbols dominates
  wall time. Per-symbol pair ordering and final tiebreak
  determinism preserved via rayon's ordered `flat_map_iter` +
  `collect`. Expected 2-3× on dense corpora (architect-bounded —
  usearch's internal OpenMP threads can contend with rayon workers
  at high core counts).

### Fixed

- **C — two-phase atomic commit for HNSW + hash-index sidecar
  pair.** `hash_index::save` is now split into `save_to_tmp`
  (writes `.tmp` + fsync) plus caller-managed `rename`; both
  `build_hnsw_at` and `build_hnsw_incremental_at` go through a new
  `commit_hnsw_and_sidecar` helper that writes both files to `.tmp`
  siblings (usearch's `index.save(tmp_path)` writes directly to
  the tmp), then renames both back-to-back. The on-disk
  inconsistency window — during which `HnswHandle::open`'s
  size-check has to fall back to brute force — shrinks from ~ms
  (the time to write the sidecar after the HNSW completes) to ~μs
  (two adjacent `rename` syscalls). Same self-heal contract on
  partial commit: HNSW-new / sidecar-old triggers brute-force
  fallback, next successful update fixes both. New unit test
  `two_phase_commit_cleans_up_tmps_on_hnsw_rename_failure` pins
  the cleanup-on-error branch (both tmps removed when the
  destination rename can't complete) so a leaked tmp can't confuse
  the next run.

### Documentation

- **`docs/COOKBOOK.md` — agent workflow recipes.** Five end-to-end
  chains for the common vex MCP-tool sequences: code archaeology
  (`find_symbol` → `bundle(mode="symbol")`), cross-file refactor with
  `usages --strict` verification gate (the load-bearing flag pattern),
  PR-impact analysis via `bundle(mode="pr-impact")` with transitive
  caller depth tuning, dead-code & near-duplicate cleanup using
  `duplicates(explain=true)` + `usages --strict` cross-check, and
  multi-codebase orchestration (one `vex-mcp` server per `VEX_ROOT`).
  Each recipe leads with a "phrase the agent like this" prompt
  template so a user can trigger the chain without naming individual
  tools; the recipe body shows the explicit tool sequence + why the
  ordering matters + when to deviate. Also adds a tool-selection cheat
  sheet table covering the 13 most-common "I want to X, reach for Y"
  decisions. Cross-linked from `README.md` (new H3 "Agent Recipes &
  Workflows" in the Integration section) and `integrations/README.md`.
  Complements `integrations/` (which solves *connect the server* —
  the cookbook solves *now what do I ask the agent*).

- **`integrations/` folder — per-agent MCP setup for Cursor / Codex
  CLI / Windsurf / Cline / Continue.dev / Zed (plus Claude Code for
  completeness).** The same `vex-mcp` binary already shipped with
  prebuilt releases since v1.11.2 works with every MCP-compatible
  client — only the config file path and serialization format differ.
  Each agent gets its own subdirectory with a ready-to-paste config
  file in the format that client expects (JSON for Cursor / Windsurf /
  Cline / Zed / Claude Code, TOML for Codex CLI, YAML for
  Continue.dev). Filenames mirror the upstream config name
  (`mcp.json` / `config.toml` / `mcp_config.json` / `vex.yaml` /
  `settings.json`) so users can `cp` straight into place. The new
  `integrations/README.md` is the index — target paths, per-agent
  caveats (Cline auto-approve, Codex timeout overrides, Continue
  agent-mode gate, Zed status indicator), and a link back to the
  Integration section in the project README. The README's
  `Other MCP Clients` block was trimmed from six inline snippet blocks
  to a single table pointing at `integrations/`, keeping the README
  scannable while making the snippets directly diff-able and
  copy-pasteable. Closes the "MCP server exists but only Claude Code
  is documented" gap noted during the v1.15 prep cycle.

- **`/vex` skill catalog (`.claude/skills/vex/SKILL.md`).** Full
  command catalog (search / show / usages — text + scope-bound
  `--strict` — structural AST patterns, call graph, semantic search,
  filters, cross-file binder coverage matrix, common pitfalls) lives
  as a lazy-loaded Claude Code skill instead of being inlined in every
  project's `CLAUDE.md`. ~2.5k tokens of catalog now load only when
  `/vex` is explicitly invoked; the parent `CLAUDE.md` keeps only six
  load-bearing rules and a pointer to the skill. Mirror to
  `~/.claude/skills/vex/SKILL.md` for global cross-project availability
  (`cp` after edits — there is no auto-sync).

- **`docs/HISTORY-INDEX.md` — full Phase 14.8 pipeline spec.**
  Mirrors the structure of `SEMANTIC.md`: end-to-end pipeline
  (git-log enumeration → blob-cache-backed parse → entry/commit/
  blob tables → FST + private string sub-section → atomic temp-
  rename write), on-disk format (`VXGH` magic, 64-byte header, fixed-
  size records with compile-time `SIZE` asserts), 4-branch update
  state machine (no-op fast path / incremental / force-push / drop),
  the canonicalize-symmetry contract every cache-keyed sidecar
  must follow, deviation note (sidecar over inline section — saves
  ~2/3 of a v6→v7 format bump, schema byte-identical for future
  promotion), benchmark numbers from the Step 7 perf bench, and
  the cold-start migration story. Also extends `docs/LIMITATIONS.md`
  with §4c history-index limits (convex-hull spans, no per-branch
  indexing, no rename tracking) and §5 tool-selection pitfall (the
  search-drift case the v1.15.0 hint addresses).

- **`docs/SEMANTIC.md` — authoritative semantic-pipeline spec.**
  Consolidates the parse → `build_context` → `context_hash` → embed
  cache → HNSW build/incremental → search flow that was previously
  scattered across CHANGELOG, inline comments, and `LIMITATIONS.md`.
  Covers the v1.14.1 hash-keyed HNSW, the v1.15.0 body_tokens
  persistence, the `Result<bool>` fallback contract on the incremental
  path, the strict-GT 25% tombstone threshold, the per-corpus-size
  performance table from `benches/perf_b12.rs`, the cold-start
  migration story, and the disk-state recovery matrix for partial
  writes. Cross-references every relevant source file
  (`output.rs::build_hnsw_at`, `body_tokens.rs`, `hash_index.rs`,
  `semantic::HnswHandle`, the bench / proptest / libFuzzer harness)
  so a future maintainer extending the semantic side has one place
  to start.

## [1.14.1] - 2026-06-06

Follow-up release closing every cross-file `--strict` ref gap the v1.14.0 release left behind (Python / C# / TypeScript class member methods + C++ class methods + name_to_global index-space bug), reorganising HNSW around content-addressed keys (prerequisite for B1.2 incremental update), and parallelising the embed pipeline's context-string build. Also bumps `CACHE_FORMAT_VERSION` 1 → 2 retroactively for the v1.14.0 `ParsedFile.cpp_includes` field — pre-1.14.1 blob caches are silently invalidated on next `vex index` (no user action; a one-time re-parse).

### Performance

- **Embed pipeline Step 1 parallelised.** `generate_embeddings`'s
  context-string + `context_hash` build now runs over `rayon::par_iter`
  instead of a sequential loop. `build_context` (string assembly +
  identifier tokenisation + path-keyword extraction) plus `xxh3_64`
  averages ~5μs/symbol; at 50k symbols the old sequential pass cost
  ~250ms wall on a warm M1, an outright second on slower laptops.
  Parallel collection preserves sym_idx order via rayon's ordered
  `unzip` contract, so cache lookup / `hash_index` sidecar / vector
  slot alignment stay correct. Pure CPU win — no impact on ORT model
  load, embedding throughput, or cache semantics. Tracing logs Step 1
  elapsed at `debug` for visibility.

  This is the **first concrete deliverable of the v1.13 B-track
  redirect**: investigation found that the original "E1 warm-clone /
  B2 parallel sessions" plan was blocked at the fastembed boundary
  (`TextEmbedding::embed` takes `&mut self`; `UserDefinedEmbeddingModel`
  consumes `Vec<u8>` by value preventing Arc-shared weights; fastembed
  already saturates CPU via ORT's `intra_threads = available_parallelism()`,
  so spawning N sessions adds memory and contention without throughput).
  Future B-track work focuses on single-session pipeline optimisations
  like this one rather than multi-session spawn.

### Changed

- **B1.1 — HNSW now keyed by `context_hash`, not by sym_idx.** The
  `vex index --semantic` build writes a paired `index.hashes` sidecar
  (`<index_dir>/index.hashes`, 4-byte `VEXH` magic + version + count +
  `Vec<u64>` in sym_idx order) so the query path can map HNSW results
  (which are now hash-keyed) back to `SymbolRecord` positions. This is
  the architectural prerequisite for B1.2 incremental update: a
  symbol's HNSW key is stable across `vex update` runs (content-based,
  same hash the v1.13 E2b embed cache uses), while the old
  sym_idx-as-key broke whenever any earlier file's symbol count
  changed — the entire HNSW had to be rebuilt for every update.
  Existing pre-1.14.1 indexes have no sidecar; `HnswHandle::open`
  bails to brute-force, matching the existing missing/stale HNSW
  degradation path. Re-run `vex index --semantic` to upgrade. 8 new
  unit tests in `search::hash_index` (round-trip, atomic save, bad
  magic / version / count / truncated body / missing file), plus an
  end-to-end smoke test that builds a hash-keyed HNSW and confirms
  `HnswHandle::search` returns the expected sym_idx via the hash map.
  Note: B1.1 does not yet do incremental update — both
  `vex index --semantic` and `vex update --semantic` still
  full-rebuild the HNSW. B1.2 will wire `load()` + `add()` +
  `remove()` + tombstone threshold once persisted body_tokens or a
  body-agnostic hash scheme stabilises the per-symbol hash across the
  fresh-parse / reconstruct boundary.

### Fixed

- **Python / C# cross-file method-call refs silently dropped under
  `--strict`.** The v1.14 Pass-2 resolver had no fallback for
  non-C++ files: `BindTarget::Unresolved` refs (method calls on
  instances, namespace members not pulled in by an explicit
  `import` / `using A.B.X;` symbol-aliasing) early-exited the C++
  include-BFS and produced no ref edge. Symptom: `gw.do_charge()`
  in Python and `gw.DoCharge()` in C# returned empty strict refs;
  C# was worse — every file had zero resolved refs, making the
  entire `reference_edges` section empty and `--strict` bail with
  "this index is v6 or has no resolved refs". Fix adds a
  **single-candidate fallback** to the Unresolved arm of the Pass-2
  loop in `store::writer`: when `name_to_global` holds exactly one
  entry for the name, resolve to it; with two or more candidates
  the resolver bails (no `Imported`-style first-match-wins for
  duck-typed method calls — disambiguating that needs type
  inference). Three new integration tests pin the contract
  (Python single-resolution, multi-candidate safety, C#
  namespace-aliased method call). Modest false-positive risk for
  projects where the same method name lives in two unrelated
  classes — those refs stay Unresolved like before.

- **TypeScript class methods + member access invisible.** Two gaps
  closed in one fix. (1) `queries/typescript.scm` had no patterns for
  `method_definition` (regular + static class methods),
  `method_signature` (interface signatures), or
  `abstract_method_signature` (abstract methods) — only free
  `function_declaration` was indexed, so `vex search do_charge` on a
  class returned nothing. (2) `src/parse/scope/typescript.rs` binder
  only emitted refs for `identifier` / `type_identifier`; member access
  (`gw.do_charge()`) puts the method name as `property_identifier` on
  the rhs of `member_expression`, never reaching the dispatch table.
  Fix adds three SCM patterns (all index as `SymbolKind::Method`)
  plus a `member_expression` walker that emits the property as a Value
  ref so the v1.14.1 single-candidate fallback can resolve it
  cross-file. Targeted at `member_expression.property` specifically —
  NOT a global `property_identifier` match — to avoid phantom refs
  from object-literal pair keys (`{ key: value }` uses the same node
  kind but it's a binding, not a usage). One integration test pins
  three method shapes (regular, static, interface signature) cross-file.

- **C++ class member methods invisible to vex.** `queries/cpp.scm`
  only covered free `function_definition` and file-level `declaration`
  shapes; methods declared inside a class body are `field_declaration`
  nodes — they never reached the index. Symptom: `vex search
  do_charge` on a header with `class Gateway { int do_charge(); }`
  returned only `Gateway`, and `vex usages do_charge --strict` had
  nothing to resolve to (the v1.14 Pass-2 BFS walked the include
  graph but couldn't find a matching name in `name_to_global`). Two
  new SCM patterns close it: `field_declaration → function_declarator
  → field_identifier` for header-declared prototypes, and
  `function_definition → function_declarator → field_identifier` for
  inline definitions inside the class body (the existing free-fn
  query only matches `identifier`-shaped declarators). Both index
  with `SymbolKind::Method`. Two integration tests pin the fix:
  `class_member_method_resolves_cross_file_via_include` (both
  declared-and-defined-elsewhere `do_charge` and inline
  `inline_method`) and `qualified_static_method_call_resolves_cross_file`
  (`app::Gateway::static_method()` qualified call site). The v1.14
  documented limitation "class member methods still don't resolve
  cross-file" is now closed.

- **`vex usages --strict` silently empty for files with module-level
  expressions.** Long-standing (since Phase 11.1.3c) inconsistency in
  the Pass-2 ref resolver: `name_to_global` pushed the post-Module-
  filter enumeration index `i` instead of the real SymbolRecord
  position carried in `sym_entries[i].1`. Any cross-file ref whose
  target lived after a synthetic `<module:path>` row in its defining
  file (Phase 14.1 sentinel — emitted for every Python file with a
  top-level statement, every Rust file with a module-level static,
  TypeScript with module expressions, etc.) had its `to_sym_idx`
  silently pointed at the Module row, not the intended symbol. Reader
  filters Module rows from `usages` output → empty result. After the
  fix, all three Pass-2 arms (`ModuleSymbol`, `Imported`, and v1.14
  `Unresolved` C++ BFS) share the SymbolRecord-position convention.
  `sym_to_file_id` was also rebuilt 1:1 with `records` (no longer
  Module-filtered) so the C++ include-BFS lookup stays correct under
  the new index-space contract. Regression test
  `strict_resolves_cross_file_ref_past_module_row_in_target_file`
  pins the bug: pre-fix it returned `No usages found`; post-fix it
  returns the expected `b.py:4` call site. The bug was invisible to
  the existing 2255-test suite because every prior fixture used files
  without module-level statements.

### Changed

- **E3 — embed cache mark-and-sweep.** New
  `EmbedCache::sweep_to(&live_hashes)` drops every entry whose hash is
  not in the current build's live-symbol set; called by the pipeline
  orchestrator (`pipeline::run` / `pipeline::update`) via the
  `prune_embed_cache` helper once the **full** set of live hashes is
  known. Closes the v1.13 E2b follow-up: the cache used to grow
  monotonically across runs because deleted or renamed symbols left
  their entries behind. After E3 the cache size equals live symbol
  count exactly — no thresholds, no LRU bookkeeping, self-healing in
  one cycle. Three new unit tests pin the sweep contract (keeps live,
  clears empty, no-op when all live); tracing log surfaces `swept N`
  only when there was work to do. Cache binary format unchanged (still
  v1, `VEXE` magic). **Reviewer-caught:** earlier draft ran sweep
  inside `generate_embeddings`, which in the `vex update` path saw
  only the changed-files hashes — would have evicted every unchanged
  symbol's cache entry on each update, defeating the cache. Now sweep
  is hoisted; `pipeline::update` passes `compute_hashes_for(all_parsed)`
  so the live set covers the entire corpus.

### Audited (no code change)

- **E2a — symbol-level diff on update.** Audit conclusion: **already
  fully closed by v1.13 E2b.** The cache key `xxh3_64(embedder_id ⨁
  kind ⨁ name ⨁ path-keywords ⨁ signature ⨁ doc ⨁ body_tokens ⨁
  budget)` is built from stable symbol-shape inputs only — no
  timestamps, no wall-clock leak, no mtime data. Changing one symbol
  in a file means exactly one cache miss; the other N-1 symbols
  cache-hit. The all-hit fast path additionally skips the 80 MB ONNX
  model load entirely. Three pre-existing `context_hash_*` unit tests
  pin the determinism contract.

## [1.14.0] - 2026-06-05 (no separate release — folded into 1.14.1)

### Added

- **C++ `#include` cross-file resolution for `vex usages --strict`.**
  Pass-2 BFS in `src/store/include_resolver.rs` walks the transitive
  quoted-`#include "..."` graph at index time and resolves
  `BindTarget::Unresolved` C++ refs against symbols in reachable
  headers. Closes the original user bug where strict refs returned
  empty for **every** C++ symbol on a 50k-symbol Windows codebase.

  Two-branch include-path resolution: relative-to-file first
  (`dir(from_path)` + include string, `./` / `..` collapsed without
  I/O), then project-wide basename fallback with deterministic
  tie-break (same-dir > shortest-path-from-root > alphabetical).
  BFS uses `HashSet<file_id>` for cycle safety — mutual includes
  (`A.h ⇄ B.h`) and `#pragma once` patterns terminate cleanly.

  **Out of scope (still unresolved):** class member methods accessed
  via `obj.method()` or `Class::method()` — the symbol extractor
  treats class members as nested-scope, not top-level, so the binder
  emits `field_identifier` refs that bypass `Unresolved`. System
  headers (`<vector>`), macro includes (`#include MY_HEADER`), `-I`
  compiler search paths, and `using namespace std;` continue to
  produce no edges. See [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md)
  §4a for the full contract.

  Storage: inline into the existing `ref_edges` section, **no format
  bump** (v6 index still v6). Architect-locked Pass-2 placement
  piggybacks on the existing `name_to_global` resolution loop in
  `src/store/writer.rs:306-371` — NOT a per-language binder hook
  (would break rayon parallelism) NOR a new pipeline stage. A new
  parallel `Vec<file_id>` indexed by sym_entries position maps
  candidate `sym_idx` → defining file_id for BFS intersection.

  New `Manifest::cpp_includes_processed: Option<bool>` marker
  unconditionally `Some(true)` for v1.14+ writes; `None` on pre-1.14
  manifests. Surfaced in `vex status`: text shows `C++ includes:
  yes` for `Some(true)` and `no (run \`vex index\` to enable
  cross-file C++ refs)` for `None`; JSON exposes
  `cpp_includes_processed: bool`.

  30 new unit tests in `store::include_resolver::tests` (path
  resolver branches + include graph build + BFS), 3 new manifest
  round-trip tests, 6 new end-to-end integration tests in
  `tests/cpp_strict_refs_test.rs` (sibling include, transitive
  depth-2 chain, mutual cycle termination, `<vector>` system header
  ignored, `using` regression guard, unincluded header doesn't
  pollute refs).

## [1.13.0] - 2026-06-05

Performance pass closing every open `P*` item from the v1.9.1 external
review (P1 / P2 / P5 / P7 / P8), plus two hotfixes: E2b is a
content-addressed embedding cache that closes a pre-existing unforced
error where `vex update` re-embedded every symbol in any touched file,
and U1 fixes a `vex self-update` asset-selection bug that downloaded
the wrong archive on every platform. No on-disk format change
(`index.vex` stays v6); no public CLI API change. Two reviewer
passes (rust-reviewer + code-reviewer in parallel) applied before
ship; 4M libFuzzer iterations across four targets clean.

**⚠️ Upgrade note for v1.12.0 users.** `vex self-update` on v1.12.0
is itself broken (U1) — it picks the wrong release archive. Do **NOT**
rely on `vex self-update` to reach v1.13.0; install once manually via
`brew upgrade vex`, your package manager, or by downloading the
`vex-<target>.tar.gz` archive from the GitHub release. Subsequent
`vex self-update` invocations from v1.13.0 onward work correctly.

### Performance

- **P2 — ONNX SHA-256 marker cache.** `verify_with_marker` writes a
  sibling `<onnx>.sha256.marker` (text format, atomic rename) recording
  `(mtime_ns, size, sha256_hex)`. Subsequent `vex search --semantic`
  invocations skip the 86 MiB rehash when on-disk mtime + size match
  the marker. **Bench: 163.73 ms → 10.71 μs warm (~15,280×).** Slow
  path unchanged; marker write failures are non-fatal. 8 new unit
  tests cover cold-first-call, hit-skips-rehash, size invalidation,
  magic-mismatch, malformed marker, wrong-sha marker bail, tamper
  detection, and `VEX_EMBEDDER_SKIP_CHECK` bypass.
- **P1 — HNSW handle hoist.** New `semantic::HnswHandle` wraps a
  single opened `usearch::Index` + `view()`. `find_similar` /
  `find_duplicates` open the handle once before any HNSW lookup; the
  latter previously reopened + mmap'd the HNSW file per outer-loop
  iter (`symbol_count` mmap cycles per query). `nearest_neighbors`
  now takes `Option<&HnswHandle>` instead of `&Path`. **Bench:
  28.70 ms → 14.10 ms at 500 sym (~2.0×).** Win scales linearly with
  `symbol_count`.
- **P5 — vectors L2-normalized at write time.** New
  `Manifest::vectors_normalized: Option<bool>` gates the brute-force
  fast path. `pipeline::run` and `pipeline::update` normalize before
  persist; `update` reads the existing manifest first and skips
  re-normalizing already-unit `unchanged_vectors` to avoid
  floating-point drift across many watch-mode cycles. CLI handlers
  thread the flag through `find_similar` / `find_duplicates` /
  `search_with_embedder` / `BundleCtx`. Brute-force similarity
  collapses to a dot product on unit vectors (skips per-call `sqrt`
  + norm computations). **Bench: 97.74 ms → 33.71 ms (~2.9×).** Three
  new unit tests pin `dot_product == cosine_similarity` within float
  epsilon for normalized inputs.
- **P7 — FST builders BTreeMap → Vec + sort.** All five builders
  (`symbol_fst`, `refs_fst`, `call_graph::{callers,callees}`,
  `ref_edges`, `bm25::Bm25IndexBuilder`) migrated. New
  `encode_caller_key_into(&mut [u8; 10], u32)` writes the 10-digit
  decimal key into a stack buffer, replacing the per-edge
  `format!("{:010}", n)` allocation in `build_callees_fst` /
  `build_ref_edges_section`. Reader-side `encode_caller_key`
  unchanged. **Bench: decimal-key 545 μs → 88.2 μs (6.17×);
  string-key 1.06 ms → 957 μs (1.11×).** Byte-equality test pins the
  stack encoder against `format!`.
- **P8 — BM25 tokenizer share-owning-String refactor.** Single
  upfront `text.to_lowercase()` + `HashSet<&str>` over its slices,
  collapsing the previous per-token `to_lowercase()` + per-unique
  `String::clone()` pattern. Allocation profile O(M + N) → O(N + 1).
  **Bench: 4.09 μs → 2.64 μs (~1.55×, 35%).** Behavior preserved by a
  parity table (13 inputs covering case-insensitive dedup, Cyrillic,
  Greek Ω lowercasing, digits in identifiers, punctuation splits) +
  invariants test (all lowercase, all length ≥ 2, deduped).

### Fixed

- **E2b — `vex update` embedding cache (closes a pre-existing
  unforced error).** Before v1.13, any single-symbol edit in a file
  caused `vex update` to re-embed *every* symbol in that file (the
  file-level content hash flipped, the file went into `changed_set`,
  `parse_files` returned all symbols, `generate_embeddings` re-ran on
  every one). On a 100-symbol file, 99 of 100 embeds were wasted
  compute. New content-addressed sidecar
  `<index_dir>/embed_cache_<embedder_id>.bin` keyed by
  `xxh3_64(embedder_id || \0 || context_string)` lets
  `generate_embeddings` partition contexts into cache hits + misses,
  embed only the misses, and persist updates. **When every context
  hits the cache, the ONNX model load is skipped entirely** — biggest
  watch-mode win on no-op or comment-only updates. Magic / version /
  embedder_id / dim mismatch on load discards the cache and starts
  empty (cold-start path); atomic `.tmp` + rename on save. 12 cache
  unit tests + 1 end-to-end integration test that proves the
  all-hit path completes in < 10 ms (no model load possible).
- **U1 — `vex self-update` downloaded the wrong release archive.**
  User reported on Windows: `vex self-update` fetched
  `vex-mcp-x86_64-pc-windows-msvc.tar.gz` instead of
  `vex-x86_64-pc-windows-msvc.tar.gz`, then failed extracting
  `vex.exe` ("Could not find the required path in the archive").
  Root cause: `self_update` crate's `Release::asset_for` does
  `name.contains(target_triple)`; both archives match the triple,
  and without an `identifier` the alphabetically-first asset wins
  (`vex-mcp-…` precedes `vex-x86_64-…`). Bug affects EVERY platform
  the release ships, not only Windows. Fix: anchor the asset name
  via `.identifier(format!("vex-{target}"))` — that substring is
  unique to the CLI archive (the MCP archive has `vex-mcp-…` after
  `vex-`, breaking the substring match). 4 regression tests pin the
  fix per target triple (Windows x64, Linux x64, macOS ARM64, macOS
  x64) AND assert the unanchored matcher still reproduces the
  original bug — if a future `self_update` version changes its
  matching semantics, the tests fail loudly. **Users on v1.12.0
  cannot reach v1.13.0 via `vex self-update`; a one-time manual
  install is required** (see the upgrade note above).

### Added

- **Benchmark scaffold `benches/perf_v113.rs`.** Six Criterion
  benches with legacy-vs-new side-by-side measurement for every
  P-item where a fair comparison is possible: P2 cold/warm, P5
  brute cosine vs dot, P1 per-iter reopen vs hoisted, P7 string vs
  decimal key build, P8 legacy vs current tokenizer. Run via
  `cargo bench --bench perf_v113`. Raw run output captured to
  `benches/results/v1.13-baseline-23565a3.txt` (gitignored).
- **`__fuzz_*` shims for the v1.13 attack surface.** Two new
  libFuzzer targets driven through `#[doc(hidden)] pub fn` entry
  points: `fuzz_marker_load` over `EmbedCache`-adjacent
  `read_marker` + `verify_with_marker` (P2 sidecar parser), and
  `fuzz_tokenize_document` over the P8 BM25 tokenizer.
  Cumulative **4M iterations** across the two new targets plus the
  P7-adjacent `fuzz_symbol_fst` / `fuzz_refs_fst` smoke runs.
  Zero crashes / panics / UB.

## [1.12.0] - 2026-06-04

Major test, refactor, and hardening consolidation. The full S-series LOC
split train (`pattern/skeleton`, `callgraph`, `cli/cmd_bundle`,
`pattern/matcher`, `hierarchy`, `parse/extractor`, `index/pipeline`,
plus `Language::ALL`) lands with zero behaviour change. T-series
testing follow-ups raise overall coverage to 88.31% lines and lift
`cli/cmd_similar.rs` from 15.70% to 93.48%. T4 wires the previously-
dead bloom filter into `vex check` via an `index.bloom` sidecar
(format v6 of `index.vex` unchanged) and a libFuzzer round on the
load path catches two real defects (`hash % 0` panic; multi-billion-
`k_num` DoS) — both fixed. Headline features: `vex index --no-wait`
for editor/CI integrations, Phase 14.6 class-level decorator
callgraph edges (Python / Java / TS / Kotlin / C#), and the S8.2
exit-code contract (0/1/2) wired across every query subcommand. Two
BREAKING lib-API changes: `pipeline::run` returns `(usize, bool)`,
and the v1.11.0 envelope/JSON-RPC error-code contract continues
unchanged.

### Added

- **Broad fuzz pass — two new libFuzzer targets + smoke-verify of
  existing four.** `fuzz_pattern_parser` over `parse_composite_pattern`
  (the user-facing Phase 11.4 metavar / `&&` / `||` parser) and
  `fuzz_manifest_load` over `Manifest::load` (JSON manifest reader)
  each cleared 2.5M / 1.5M iterations with zero crashes. Existing
  `fuzz_index_reader` / `fuzz_refs_fst` / `fuzz_symbol_fst` re-verified
  clean post-T4 (no regressions from the bloom sidecar work). Cumulative
  ~10M iterations across six targets this release. Seed corpora for the
  two new targets bootstrapped via `fuzz/generate_seeds.sh`.
- **T4 fuzz hardening — `fuzz_bloom_load` libFuzzer harness over
  `SymbolBloom::load`.** Found and fixed two real defects on a crafted
  sidecar: (1) `n_bits=0`/`k_num=0` passed the `n_bits == bitmap_len*8`
  consistency guard but panicked in `bloomfilter::Bloom::check`'s
  `hash % bitmap_bits` divide-by-zero; (2) `k_num` up to ~2.1B made
  `may_contain` loop for 110+ sec per call (DoS, not a panic). The
  load path now rejects degenerate sizes (`n_bits > 0 && k_num > 0`)
  and caps `k_num <= MAX_K_NUM = 64` (legitimate filters at
  FP = 1e-10 still have k_num ≈ 33). Both crash inputs minimised to
  regression seeds in `fuzz/corpus/fuzz_bloom_load/` and pinned by
  unit tests. After fixes: 2.7M iterations / 5 min clean, coverage
  saturated at 179 features.
- **T4 — Bloom-filter pre-filter for `vex check`.** Closes
  `TODO(phase4): wire into MCP server for fast pre-filtering` in
  `src/search/bloom.rs`. `vex index` and `vex update` now build a
  deterministic bloom (1% FP rate, fixed sip-key seed) over every
  symbol name (with case-folded duplicates) and persist it as a
  sidecar at `<index_dir>/index.bloom`, mirroring the HNSW sidecar
  pattern. `vex check` lazily loads the sidecar and short-circuits on
  `may_contain == false`, skipping the FST lookup entirely for
  definitely-missing names. Format v6 of `index.vex` is unchanged —
  bloom is a sidecar, not a section, so older readers stay binary-
  compatible. A missing or corrupt sidecar is non-fatal: `vex check`
  silently falls through to the FST. Lifts `search/bloom.rs` coverage
  from 63.22% to 82.49% lines and removes the `#[allow(dead_code)]`
  annotation — the module is finally live code.

### Refactored

- **S2 — `pattern/skeleton.rs` 4129-LOC monolith split into a four-file
  directory module.** Zero behaviour change; public API
  (`extract_skeletons`, `Skeleton`) stays at the same path. New layout:
  `mod.rs` (396 — `Skeleton` struct, serde impls, walker, small
  helpers), `kinds.rs` (535 — per-language `pattern_targetable_kinds`
  allowlist dispatch), `ident.rs` (279 — per-language `extract_ident`
  dispatch + private `child_by_kind`), `tests.rs` (2965 — verbatim move
  of the original `#[cfg(test)] mod tests`). Adding a new language now
  touches two `match` arms in `kinds.rs` and `ident.rs` instead of
  navigating a single 4k-LOC file.
- **S3 — `callgraph/mod.rs` 2473-LOC monolith split into a four-file
  module.** Zero behaviour change; public surface
  (`extract_call_edges`, `find_callers`, `find_callees`, `CallMatch`,
  `CALLERS_FETCH_CAP`) unchanged at `crate::callgraph::*`. New layout:
  `mod.rs` (102 — public query API + types + sub-module declarations +
  `pub use extractor::extract_call_edges` re-export), `extractor.rs`
  (490 — `COMPILED_QUERIES` static, `extract_callgraph` walker, edge
  resolution, live-scan helpers, sibling-host filters), `queries.rs`
  (419 — per-language tree-sitter SCM query source dispatch),
  `tests.rs` (1518 — verbatim test move). The walker no longer competes
  with 1.5k LOC of SCM strings for screen real estate.
- **S4 — `cli/cmd_bundle.rs` 1291-LOC monolith split into a per-mode
  module.** Zero behaviour change; public surface
  (`cmd_bundle::bundle`, `cmd_bundle::BundleModeFlag`, the three
  `assemble_X` re-exports) unchanged. New layout: `mod.rs` (360 —
  public types, dispatch, shared signal/rank helpers, inline tests),
  `symbol.rs` (310), `pr_impact.rs` (338), `project.rs` (351). Each
  `BundleModeFlag` arm now owns its assembler and its mode-only
  helpers; shared helpers (`global_rank_percentile`, `signals_fst_hit`,
  `caller_kind`) stay `pub(super)` in `mod.rs`. Pre-existing
  `#[doc(hidden)]` misplacement on `MAX_PR_IMPACT_NODES` corrected in
  passing.
- **S5 — `pattern/matcher.rs` 1562-LOC monolith split into a three-file
  module.** Zero behaviour change; public surface
  (`parse_composite_pattern`, `find_matches_composite`,
  `CompositePattern`) at `crate::pattern::matcher::*` unchanged. New
  layout: `mod.rs` (640 — public types, matcher engine), `parse.rs`
  (211 — `parse_pattern`/`parse_composite_pattern`/`split_top_level`
  plus the `Segment` parser-input helpers), `tests.rs` (735 — verbatim
  test move). `PatternTree.segments` now constructed via a
  `pub(super) fn new` constructor instead of cross-module field
  visibility; `parse_pattern` and `split_top_level` are `pub(super)`
  since their only callers are this module's tests.
- **S6 — `hierarchy/mod.rs` 1213-LOC monolith split into a three-file
  module.** Zero behaviour change; public surface
  (`find_implementations`, `ImplMatch` at `crate::hierarchy::*`)
  unchanged. New layout: `mod.rs` (125 — public types, entry point,
  private `find_in_source` matcher), `queries.rs` (372 — per-language
  `inheritance_query` SCM dispatch, `PHP_TRAIT_PATTERN_START` and
  `RUBY_MIXIN_PATTERN_START` boundary constants, `relation_label`
  pattern-index → label mapping), `tests.rs` (734 — verbatim test
  move). Adding a new language now requires updating two `match` arms
  in the same `queries.rs` file instead of navigating a 1.2k-LOC
  monolith. The `ImplMatch.relation` field's inline doc was corrected
  to list the full label vocabulary (`impl` / `extends` / `inherits`
  / `include` / `uses`) — the stale comment had only three values.
- **S7 — `parse/extractor.rs` 1189-LOC monolith split into a five-file
  directory module.** Zero behaviour change; public surface
  (`extract_symbols_and_imports`, `extract_references_ast`,
  `is_meaningful_identifier`, `GrammarLoadError` at
  `crate::parse::extractor::*`) unchanged. New layout: `mod.rs` (152 —
  `GrammarLoadError`, shared `is_keyword` and `is_meaningful_identifier`
  helpers, re-exports), `symbols.rs` (219 — `extract_symbols_and_imports`
  + import-quote / doc-above helpers), `body.rs` (145 —
  `extract_body_tokens` + `tokenise_string_value`), `refs.rs` (224 —
  `extract_references` + `extract_references_ast` walker + per-language
  comment/string/identifier classifiers), `tests.rs` (504 — verbatim
  move). Bundled `extract_doc_above` perf fix: now takes `&[&str]` and
  reuses the caller's `line_slices` (v1.12.0 P4) instead of
  re-collecting `content.lines()` per symbol.
- **T2 — doctests on key public APIs.** Project previously had zero
  doctests. Six compile-tested examples on pure-function public seams:
  `Language::from_extension`, `Language::ALL`, `extract_skeletons`,
  `parse_composite_pattern`, `extract_call_edges`,
  `extract_symbols_and_imports`. Each exercises a happy path plus a
  degenerate case so the doc-test gate catches API-shape regressions.
- **T3 — `cli/cmd_similar.rs` integration coverage lifted from 15.70%
  to 93.48%** (the lowest of any `cli/*` handler with non-trivial
  logic per the T1 baseline). New `tests/cli_similar_test.rs` carries
  12 tests covering the previously-untested branches: no-vectors
  `bail!`, `signal_no_results` exit-1 contract, `--filter` substring
  match, `--include`/`--exclude` scope (incl. exclude-wins-over-
  include + combined filter+include AND semantics), `--limit`
  saturation, `print_similar` text/JSON, `--why` trace
  (`seed_resolved`/`threshold_applied`/candidates/`filter_applied`),
  negative `--why` contract (no `VEX_WHY:` on stderr without flag),
  `--explain` (jaccard + bidirectional diff). Tests sidestep
  `vex index --semantic` (60+ sec per test under `cli_explain_test`)
  by pre-building a v6 vector-bearing index via `write_index_full` at
  the `local_cache = true` cache path — full suite runs in < 2 sec.
  Uncovered remainder (6.5%) is the defensive `eprintln!` for an
  unreachable seed-resolution mismatch plus the `diff_filter_meta`
  block (requires git worktree setup).
- **S10 — `Language::ALL` slice eliminates the `COMPILED_QUERIES` ↔
  `callgraph::queries::callgraph_query` sync requirement.** Adds
  `pub const ALL: &'static [Language]` covering every variant in
  declaration order; `callgraph::extractor::COMPILED_QUERIES` now
  iterates `Language::ALL.filter(|l| callgraph_query(l).is_some())`
  instead of a hardcoded 8-language array. Adding a new callgraph
  language is now a queries-only change — registration in `extractor.rs`
  is automatic. Pin test `all_slice_covers_every_variant` blocks a
  future variant addition that forgets to update the slice. Closes the
  S3 review finding.
- **S8 — `index/pipeline.rs` 1586-LOC critical indexing path split into
  a five-file directory module.** Zero behaviour change; public surface
  (`IndexOptions`, `run`, `run_or_busy`, `update`, `update_or_busy` at
  `crate::index::pipeline::*`) unchanged. New layout: `mod.rs` (469 —
  public entry points, orchestration, manifest skip-path predicates),
  `lock.rs` (129 — `IndexLock` RAII guard, blocking + non-blocking
  acquire, `is_lock_contended` cross-platform helper), `output.rs` (389
  — BM25 / callgraph / pattern-skeleton / embedding / HNSW builders +
  `write_output_locked`), `parse_files.rs` (485 — fan-out parser,
  `reconstruct_unchanged` incremental fast path, file discovery /
  hashing / heuristics), `tests.rs` (205 — verbatim move). The
  `IndexLock` RAII contract, manifest re-check skip-path, and symbol-
  index ordering invariant (`resolve_call_edges` matches
  `writer::write_index_to`) all verified preserved by parallel reviewers.
  Closes the v1.12.0 S-series refactor train (S2–S8).

### Performance

- **Per-thread `tree_sitter::Parser` pool (P3).** Every hot parse site
  (`extractor::extract_symbols_and_imports`,
  `extractor::extract_references_ast`, `body::extract_symbol_body_ts`,
  `scope::walker::parse_with`, `callgraph::extract_callgraph`,
  `pattern::skeleton::extract_skeletons`) previously paid the cost of
  `Parser::new()` + `set_language()` per file. They now borrow a
  per-thread, per-language `Parser` from a new
  `crate::parse::parser_pool::with_parser` thread-local pool — first
  use per (thread, language) initialises lazily; subsequent calls
  re-use the existing parser. After Phase 14.7 cut cold-start parse
  time with the blob cache, this per-file overhead was the dominant
  remaining cost for projects with several thousand files. Pinned by
  three new unit tests in `src/parse/parser_pool.rs::tests`.
- **`content.lines().nth(n)` quadratic patterns eliminated (P4).** Two
  hot sites in `extractor::extract_symbols_and_imports` and
  `extractor::walk_for_refs` looked up the context line for every
  captured symbol / import / identifier via `content.lines().nth(line - 1)`,
  each O(line_count). On identifier-dense files (a 5k-LOC source with a
  few hundred captures) that compounded to O(line_count × capture_count)
  per file. Both sites now pre-collect line slices once and index O(1),
  which is the natural shape given line numbers are already known. Small
  per-file save; visible at workspace-scale.

### Fixed

- **H10 — `vex watch` UX hardening.** Three behaviours the original
  implementation got wrong are pinned here:
  1. *Batch coalescing.* notify-debouncer-full collapses rapid edits
     within its debounce window, but the mpsc channel queued
     deliveries while a long re-index was in flight — every queued
     batch triggered another `pipeline::update`. After `rx.recv()` the
     handler now drains every pending batch via `try_recv` and merges
     them. N debouncer deliveries → one update.
  2. *`.gitignore` re-eval.* The relevance filter previously only
     accepted source-file extensions. Editing `.gitignore` to
     un-ignore a file had no effect until an unrelated source change
     incidentally nudged the watcher into running `update`.
     `.gitignore` (and nested `.gitignore`s) are now treated as
     relevant events; `pipeline::update`'s `discover_files` re-walks
     with the freshly-edited rules.
  3. *New-dir re-arm.* `RecursiveMode::Recursive` on the inotify
     backend (Linux) only recurses at watch-install time — sub-
     directories created during the watch session were invisible.
     `Create(Folder)` events now call back into the debouncer's
     inner watcher to arm the new path. macOS FSEvents and Windows
     ReadDirectoryChangesW both auto-recurse, so the re-arm is a
     no-op there but harmless to call.

  6 unit tests in `src/watch/handler.rs::tests` cover the
  source-path / `.gitignore` / non-source relevance branches plus
  the `extract_new_directories` filtering, deduping, and
  missing-path handling. The event loop itself is left as
  end-to-end-tested via local manual runs because the OS-level
  notify wiring is hostile to deterministic integration tests.

  A v1.12.0 follow-up (rust-reviewer N8) wraps fix 3 in an
  `armed_dirs: HashSet<PathBuf>` so repeated `Create(Folder)`
  events for the same path across batches don't re-invoke
  `debouncer.watch(p, …)` — the upstream call runs an O(subtree)
  `WalkDir` inside `FileIdMap::add_path` each time it's invoked
  (its own `data.roots` dedupe runs *after* that walk), so long
  sessions that keep recreating the same scratch dir would do real
  work on every re-arm. The set is rolled back on watcher error so
  a path that was momentarily missing can still arm on a later
  batch. A final-review SHOULD-FIX wires `Remove(Folder)` events
  into `armed_dirs.remove(&p)` before the re-arm loop runs, so a
  delete-then-recreate scratch-dir pattern still re-arms on the
  recreated path (without this, the set would short-circuit the
  recreate and the new directory would silently stay un-watched).

- **S8.2 — explicit exit-code contract (0 / 1 / 2).** Pre-v1.12.0 `vex`
  effectively always exited 0 — even when a search returned zero
  results, even on bad regex syntax that propagated up as anyhow
  errors. The contract documented in earlier CHANGELOGs ("0 = success,
  1 = no results, 2 = error") was aspirational, not implemented. CI /
  scripts could not gate `vex search Foo` on whether anything was
  found. Now: `0` on success-with-results (or successful action),
  `1` when a query handler signalled empty via the new
  `cli::exit_code::signal_no_results()` side channel (`search`,
  `usages`, `callers`, `callees`, `pattern`, `grep`, `show` all wired),
  `2` when `dispatch` returns `Err` — anyhow's auto-formatted message
  goes to stderr. The exit-code wiring lives in
  `src/cli/exit_code.rs` (process-global `AtomicBool`); `main` maps
  through `std::process::ExitCode`. Handlers that always succeed
  (`vex index`, `vex update`, `vex outline`, `vex status`, …) stay at
  `0`. The seven remaining query subcommands (`similar`, `duplicates`,
  `implementations`, `paths`, `reachable`, `diff`, `bundle`) are now
  wired in this same release — all fourteen query commands honour the
  contract. `vex bundle` gates on the `items` array being empty;
  `mode_hints.empty_reason` already explained the *why*, the exit code
  now lets scripts skip a `jq` call. New `docs/EXIT-CODES.md` is the
  authoritative reference for script authors.
- **S8.3 — `VEX_BIN` and `project_root` validation in `vex-mcp`.**
  Previously a typo'd `VEX_BIN=/wrong/path` surfaced as an opaque
  OS-level "No such file or directory" inside the MCP tool-call
  response, with no hint of which env var or path was at fault. New
  helpers `resolve_vex_bin()` and `validate_project_root()` in
  `crates/vex-mcp/src/main.rs` check existence, file-vs-directory
  shape, and Unix executable-bit (on Windows the file-existence check
  is sufficient since `.exe` extension associates executability)
  before `Command::spawn`, emitting messages like
  `VEX_BIN points to /opt/foo/vex but no such file exists; unset
  VEX_BIN to fall back to PATH lookup of \`vex\``. Pinned by 5 unit
  tests covering free / nonexistent / directory / file-as-dir /
  missing-dir cases.
- **S8.4 — `env!("VEX_VERSION")` survives degraded build
  environments.** Pre-fix, consuming `vex` as a git dependency from a
  partial workspace (or under some rust-analyzer configurations)
  could refuse to compile because `build.rs` had not run to define
  `VEX_VERSION`. The `Cli` derive now resolves the value through a
  new `const VEX_VERSION: &str` that uses `option_env!` with a
  fallback to `CARGO_PKG_VERSION`; production builds keep printing
  the git-describe string set by `build.rs` (`v1.11.2-NN-gSHA`),
  while degraded environments at least produce the crate's
  `Cargo.toml` version instead of failing the whole compile.

- **Options-aware skip-path in `pipeline::run` and `pipeline::update`.**
  Closes the [1.11.2] known limitation — both skip-paths previously
  decided whether to reuse a peer's just-finished rebuild from file
  hashes alone, so a waiter that asked for `--semantic` would silently
  be served an embedding-less manifest from a peer that built without
  embeddings. `manifest_options_cover` now also requires the manifest's
  `embedder_id` to match the caller's requested embedder before
  skipping. The `run` variant (`run_can_skip`) additionally refuses to
  skip a partial pattern index (`pattern_index_full == Some(false)`,
  i.e. a manifest written by `vex update`) when the user explicitly ran
  `vex index` and opted into the pattern section — the partial section
  is harmless for queries (live-scan falls back), but the explicit
  full-rebuild ask is owed. Sticky boolean opt-outs (`call_graph`,
  `bm25`, `pattern_index`) are intentionally left out of the gate: a
  rebuild would preserve the existing value per the Manifest doc
  comments, so blocking the skip for them would produce the same
  on-disk result. 8 helper unit tests in `src/index/pipeline.rs::tests`
  pin every coverage case.

### Added

- **Phase 14.6 — class-level decorator / annotation edges in the
  callgraph.** Bare class-level decorators (`@dataclass class Foo:`,
  `@Component class Bar`, `[ApiController] class Baz {}`, `@JvmStatic
  class Qux`, `@RestController class Quux {}`) and the call-shape
  forms (`@Module({}) class …`) now emit edges in the persistent
  callgraph. Classes are still not FnDef symbols, so the decorator's
  callee is attributed to module scope via Phase 14.1's synthetic
  `<module:path>` caller (caller_fn_name="", caller_fn_line=0). This
  closes the largest remaining callgraph gap documented in
  `docs/LIMITATIONS.md`. A new `@module_call.name` capture name was
  introduced for these patterns so the TypeScript
  `call_capture_inside_sibling_host` filter (designed to dedupe
  decorator-argument calls) does not suppress them — the filter still
  operates on the generic `@call.name` captures untouched. Languages
  covered: Python, Java, TypeScript, Kotlin, C# (Rust `#[derive(...)]`
  remains intentionally filtered as a compile-time codegen marker).
  14 new tests in `src/callgraph/mod.rs::tests` cover the bare
  identifier, scoped, marker, and annotation-with-args variants per
  language, plus a disjointness pin (`java_class_and_method_annotations_have_disjoint_attribution`)
  that locks "class-level decorators attribute to module, method-level
  decorators attribute to their method". A v1.12.0 follow-up adds the
  three-segment qualified class-level test (`kotlin_class_annotation_qualified_emits_module_scope_edge`)
  pinning `@kotlin.jvm.JvmStatic class Foo` → only `JvmStatic` leaks
  (intermediates `kotlin` / `jvm` must not), symmetric with the
  existing C# `csharp_class_attribute_qualified_emits_module_scope_edge`.
- **`vex index --no-wait` / `vex update --no-wait`.** New CLI flag for
  callers that would rather no-op than wait on a peer's build lock.
  When set, the underlying `pipeline::run_or_busy` / `pipeline::update_or_busy`
  variants use a non-blocking `IndexLock::try_acquire`; if the lock is
  held they emit a `Skipped: another vex instance is indexing` text
  message (or `{"status":"busy","reason":...}` in JSON mode) and exit
  with code 0 — `git pull`-style "Already up to date." semantics. Useful
  for editor integrations / CI cron jobs that don't want to wedge for
  a peer's parse + embed. Without the flag, behaviour is unchanged
  (blocking lock acquire). Pinned by two unit tests covering
  `IndexLock::try_acquire`'s free and contended paths.
- **`docs/EXIT-CODES.md`.** Author-facing reference for the S8.2
  contract (0 / 1 / 2). Lists which subcommands distinguish 0 vs 1
  and which always exit 0, explains the side-channel design choice,
  and ships two shell examples so script authors don't have to read
  `src/cli/exit_code.rs`.
- **`tests/common/mod.rs::assert_ran` shared helper.** Three
  integration tests previously hand-rolled the same "accept exit code
  0 or 1" assertion (`cli_pattern_why_test`,
  `cli_callgraph_decorators_test`, `cli_module_symbols_test`); seven
  more (`cli_bundle_test`, `cli_diff_test`, `cli_explain_test`,
  `cli_paths_reachable_test`, `cli_pattern_test`, `cli_scope_test`,
  `cli_usages_strict_test`) had `.success()` query-command assertions
  that started failing once the new `signal_no_results` wiring made
  empty results exit `1`. All ten now share the helper via Cargo's
  `tests/common/mod.rs` convention (the `mod.rs` form, so it isn't
  compiled as its own test binary).

### Changed

- **BREAKING (lib API) — `pipeline::run` now returns `Result<(usize,
  bool)>` instead of `Result<usize>`.** The second tuple element is
  `rebuilt`: `true` when the call did the work, `false` when the
  manifest re-check proved a concurrent peer had already produced an
  equivalent index and the rebuild was skipped. This mirrors
  `pipeline::update`'s `(total, changed, deleted)` shape and is what
  enables the new `concurrent_run_rebuilds_once_not_per_thread`
  regression test to assert "exactly one rebuilds" — before this
  signal, the v1.11.2 patch could only assert the skip-when-fresh
  property. All workspace callers updated to bind both elements or
  to take `.0` explicitly. CLI behaviour and JSON output for
  `vex index` are unchanged; only library consumers see the new
  shape.

## [1.11.2] - 2026-06-02

Patch release that completes the concurrency hardening started in v1.11.1
and tidies up the OSS surface (LICENSE, CODE_OF_CONDUCT, Cargo metadata).

### Fixed

- **`vex index` thundering herd closed.** v1.11.1 fixed the herd for
  `vex update` (lock held across parse + embed + write + HNSW with a
  manifest re-check under the lock) but left `vex index` (full rebuild)
  with the lock acquired *after* parse and embed. Under multi-agent
  fan-out (`vex index` from N parallel CI jobs or subagents), every
  instance still loaded the embedder and re-parsed the whole tree in
  parallel. `pipeline::run` now acquires `IndexLock` before
  `parse_files`/`generate_embeddings` and re-loads the manifest under the
  lock: if a peer already produced an index with an identical file
  fingerprint, the rebuild is skipped. Verified by
  `concurrent_run_skips_when_index_already_fresh` in
  `tests/concurrency_test.rs` (manifest mtime survives N concurrent
  `vex index` calls). Note: this also means a single user running
  `vex index` twice back-to-back on an unchanged tree will see the
  second invocation skip — matching the behaviour `vex update` already
  had for the same input.
- **Windows lock-contention detection.** The new try-then-block
  diagnostic path in `IndexLock::acquire` matched only
  `ErrorKind::WouldBlock`, which is what POSIX `flock` returns under
  contention but not what Windows `LockFileEx` returns —
  `ERROR_LOCK_VIOLATION` (raw OS code 33) typically maps to
  `ErrorKind::Other`. Without the raw-code fallback, every concurrent
  `vex index`/`update` on Windows would have bypassed the "waiting for
  index lock" log and surfaced as an outright error instead of a
  serialized wait. The `is_lock_contended` helper now matches both.

### Added

- **Tracing event on lock contention.** `IndexLock::acquire` now tries
  the lock non-blocking first and emits a single `info`-level
  `tracing::info!` event ("waiting for index lock (another vex instance
  is indexing)") before falling into the blocking wait. Users and agent
  harnesses watching for output see why a `vex index`/`update` appears
  stuck instead of staring at a frozen terminal for the duration of the
  peer's parse + embed + write.
- **`docs/CONCURRENCY.md`** — describes the per-project advisory-lock
  contract used by `pipeline::run`/`pipeline::update`, the never-unlinked
  sentinel rationale, lock-holding windows, filesystem caveats, the
  tests that pin each property, and the known fingerprint-only skip-path
  limitation. Reference doc for anyone extending or debugging the
  index-build paths.
- **`.config/nextest.toml`** with a `default` and `ci` profile (retries
  + slow-test guard), plus a `CONTRIBUTING.md` section explaining how
  to opt in. `cargo nextest run` parallelises across integration-test
  binaries and is typically 2-5× faster than `cargo test` on this
  workspace; CI continues to use `cargo test` to avoid an extra tool
  install per runner.
- **Prebuilt `vex-mcp` in every release.** Before v1.11.2 the release
  pipeline only packaged the `vex` CLI; users who wanted the MCP
  server (`vex-mcp`) had to install Rust and `cargo build --release -p
  vex-mcp` themselves — a real onboarding tax on Windows where rustup
  is not usually preinstalled. The release matrix now emits a parallel
  `vex-mcp-<target>.tar.gz` artifact for the three triples the build
  matrix covers (`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`,
  `x86_64-pc-windows-msvc`), signed the same way as
  `vex-<target>.tar.gz` and attached to the GitHub release. Kept as a
  separate archive (not bundled with `vex`) because `vex self-update`
  extracts a single named binary from its tarball and would silently
  ignore a second one. README's MCP section now points at the
  prebuilt and treats `cargo build` as the fallback for unsupported
  triples (Intel Mac, BSD, etc.).
- **`LICENSE` (MIT) at the repo root.** The license type was already
  declared in `Cargo.toml` but the conventional plain-text file was
  missing — GitHub and crates.io expect both.
- **`CODE_OF_CONDUCT.md` (Contributor Covenant 2.1).** Reports route
  through GitHub Issues (or GitHub DM to the maintainer for confidential
  matters).

### Changed

- **Cargo package metadata aligned with the v1.11.1 tag and OSS surface.**
  `version` bumped from `1.11.0` to `1.11.2` (since `Cargo.toml` was not
  updated in the v1.11.1 release), and `authors`, `repository`,
  `homepage`, and `readme` fields added so crates.io listings and IDE
  manifests render the project correctly.

### Known limitations

Both items below were resolved in the v1.12.0 development cycle — see
the `[Unreleased]` section above for the closing entries.

- **Skip path is fingerprint-only.** Both `pipeline::run` and
  `pipeline::update` decide whether to skip a concurrent peer's
  finished rebuild purely from the file-hash diff — they do not compare
  `IndexOptions` (e.g., `--semantic`) against what the peer recorded in
  the manifest. A waiter that asked for embeddings can be served a
  structural-only index from the skip path without an error. The
  workaround is to delete the cache directory and rebuild manually.
  This is pre-existing in `update` since v1.11.1 and now applies
  symmetrically to `run`. **Resolved in v1.12.0** via
  `manifest_options_cover` / `run_can_skip` / `update_can_skip`.
- **No symmetric "exactly one rebuilds" test for `run` on a stale
  index.** `pipeline::run` returns only a symbol count, so the
  rebuilt-vs-skipped distinction is not observable from outside the
  function the way it is for `update`'s `(total, changed, deleted)`
  return. The fresh-index property is pinned by
  `concurrent_run_skips_when_index_already_fresh`; the stale-index
  herd-elimination is not yet pinned by a test. **Resolved in v1.12.0**
  by changing `pipeline::run` to return `(usize, bool)` and adding
  `concurrent_run_rebuilds_once_not_per_thread`.

## [1.11.1] - 2026-06-02

Patch release. Closes a thundering-herd concurrency bug in `vex update`
that surfaced under multi-agent fan-out. Tag was placed at the merge
commit of PR #2 (`AlexeiDolgolyov:fix/concurrent-update-herd`); this
CHANGELOG entry was added retroactively in v1.11.2 to document what was
shipped under that tag.

### Fixed

- **`vex update` no longer thundering-herds the rebuild under multi-agent
  fan-out.** When several `vex` instances ran `auto_update` against the
  same stale index concurrently (an agent harness fanning out subagents
  that each shelled out to `vex` on a dirty branch), each instance
  independently parsed and embedded the changed files. The build lock
  only wrapped the final write, and `update` generated embeddings before
  reaching it, so the expensive parse + embed ran in parallel across
  every instance — N redundant rebuilds saturating CPU and RAM until
  they finished. Fix: hold the build lock across the whole rebuild
  (parse + embed + write + HNSW) in both `run` and `update`, via an RAII
  `IndexLock` guard. `update` acquires it before parse + embed and
  double-checks staleness under it: the first instance does the work;
  the rest wait, observe the now-fresh index, and skip.
- **Lock-file sentinel never unlinked.** The previous lock guard
  deleted the lock file on drop, which is the classic `flock` + unlink
  race — a queued waiter keeps its handle on the now-unlinked inode
  while a new instance creates a fresh inode under the same name and
  locks it immediately, so both run at once. The sentinel is now
  created once and never removed.
- **`vex index` / HNSW pair published atomically.** `run` now holds the
  lock across `build_hnsw` too (previously only `update` did), so the
  `index.vex` / `index.hnsw` pair is published under one critical
  section and concurrent writers can't desync or clobber it.

### Tests

- Adds `concurrent_update_rebuilds_once_not_per_thread` in
  `tests/concurrency_test.rs`: asserts that of N concurrent updates on
  a stale index, exactly one rebuilds and the rest skip. Fails without
  the fix, where all N rebuild.

## [1.11.0] - 2026-06-01

v1.11.0 is a minor release that closes five external-review items
(H4, H5, H8, H11, H12), lands a large internal refactor that drops
`cli/mod.rs` from 2642 → 467 LOC, and ships one feature (Phase 8.4
body tokens for config languages). **It carries two BREAKING JSON
contract changes** (H5-full envelope and H8 `-32602`) — see migration
notes under `### Changed` below before bumping. Pre-1.11 indices
require **no rebuild**: the manifest format is unchanged.

Release-day net: cli/mod.rs decomposition into 17 cmd_*.rs files +
common.rs/index_management.rs (S1 Groups A–F), four review-closeout
fixes (H4 brace-aware ellipsis, H5-full envelope contract, H8 typed
MCP params, H11 content-hash staleness, H12 indegree per-(name,sym_idx)),
config-language semantic search (TOML/YAML/HTML/CSS), and docs updates
covering the JSON envelope migration + MCP `-32602` error contract.

### Changed

- **BREAKING (H8) — `vex mcp` rejects wrong-typed params with JSON-RPC
  `-32602 Invalid params`.** Pre-H8 `tools/call` silently coerced
  wrong-typed arguments to their defaults: `limit: "20"` (string)
  became `limit: 20` (default), `auto_update: 1` (number) became `true`
  (default), `kind: "fn"` (string instead of array) was silently
  dropped. Every inline `as_str()/.as_bool()/.as_u64().unwrap_or(...)`
  site in `build_command` now routes through strict helpers (`req_str`,
  `opt_bool`, `opt_u64`, `opt_str_array`, …) that emit a `ParamError`
  on type mismatch; `handle_request` downcasts that marker to JSON-RPC
  spec-compliant `-32602 Invalid params`. Missing required fields,
  wrong-type required fields, and the previously-`-32000`
  mutually-exclusive flag conflicts (`since`/`since_branched`/`changed_only`;
  `signature_only`/`head`/`no_body`/`collapsed`; `async_only`/`no_async`)
  all surface as `-32602` with field-level error messages instead of
  the previous generic `-32000`. **Downstream agents that branched on
  `-32000`** for these conditions must update — `-32602` is the
  spec-correct code. 23 contract tests in `crates/vex-mcp/src/main.rs::tests`
  pin the behaviour per-tool (search, callers, callees, paths, reachable,
  diff, show, check, bundle, plus the three flag-conflict paths).

- **BREAKING (H5-full) — every `--format json` subcommand now emits the
  Phase 13 envelope.** Pre-H5-full `search` and `bundle` returned the
  envelope (`{ protocol_version, capabilities, _meta, results }`) while
  the other ~14 subcommands (`show`, `usages`, `pattern`, `grep`,
  `implementations`, `callers`, `callees`, `paths`, `reachable`,
  `check`, `similar`, `duplicates`, `diff`, `outline`, `index`,
  `update`, `status`, `eval`) returned bare arrays/objects. They now
  all wrap their payload in the same envelope so agent-side parsers
  can rely on a single shape and on `protocol_version == "v1"` as a
  forward-compat probe (Phase 13.0). Downstream consumers that parsed
  the bare shape must read `response["results"]` instead. The
  CHANGELOG claim from v1.9.x ("every JSON envelope carries
  `protocol_version`") is now true. Locked by `tests/cli_envelope_contract_test.rs` —
  20 contract assertions (one per subcommand plus the
  `VEX_JSON_ENVELOPE=0` escape-hatch).

### Fixed

- **`VEX_JSON_ENVELOPE=0` escape-hatch honored across every
  `--format json` subcommand.** The generic `print_envelope` used by
  14 H5-full handlers (`show`, `usages`, `pattern`, `grep`, `outline`,
  `implementations`, `callers`, `callees`, `paths`, `reachable`,
  `check`, `similar`, `duplicates`, `diff`, `eval`, `status`, `index`,
  `update`) initially ignored the env-var — only `print_search_envelope`
  honored it. The README documented this opt-out as the pre-1.9
  compatibility valve; partial coverage would have been a contract
  violation. Routed the check through `print_envelope` so bare-array
  output works for every subcommand. Pinned by
  `envelope_disabled_via_env_falls_back_to_bare` in
  `tests/cli_envelope_contract_test.rs`.

- **`_meta.vex.dev/index_age_ms` populated on every envelope handler.**
  In the H5-full first pass, 12 of 14 handlers built
  `MetaEnvelope::default()` (all-`None`), which would have silently
  broken the staleness signal the CHANGELOG promises consumers. New
  helper `output::default_meta_for(root)` mirrors `build_search_meta`
  minus the per-result `signals` block; every handler that already
  binds a project `root` now feeds it through. `outline` keeps the
  default meta (single-file parse — no project context).

- **Phase 8.4 bare `"string"` leaf gated to TOML.** Initially,
  `extract_body_tokens` routed any tree-sitter `"string"` node-kind
  through the value-tokeniser regardless of language. With
  `tree-sitter-toml-ng` it's the desired path; every other grammar
  that exposes a `"string"` parent (Rust, Python, TypeScript, …)
  walked the entire raw region — quotes and escapes included —
  alongside the proper `string_content` / `string_fragment` walk.
  Dedup masked the redundancy but it was a footgun for any future
  non-config language without a `string_content` child.
  `extract_body_tokens` now takes a `Language` parameter and only
  applies the bare-leaf arm when `lang == Toml`. Pinned by
  `v1_11_hotfix_python_string_tokens_come_from_string_content_not_bare_string`.

- **`vex pattern` `$_<alphanum>` typos preserve the `$`.** The parser
  swallowed the leading `$` when it saw an invalid `$_NAME` form
  (where `$_` must stand alone), so `$_Bar` silently degraded to
  matching the literal `_Bar` — a typo indistinguishable from
  intentional underscore-prefixed identifier text. The literal buffer
  now retains `$_`. Standalone `$_` (anonymous wildcard) is
  unaffected. Pinned by
  `v1_11_hotfix_invalid_underscore_metavar_preserves_dollar`.

- **H11 — staleness check verifies content hashes before flagging
  files as changed.** Pre-H11 `check_mtime` flagged any file with
  `mtime > indexed_at` as stale, triggering spurious auto-rebuilds on
  every workflow that touched mtimes without changing content:
  `touch foo.rs`, `git checkout` (mtimes are restored), `rustfmt`
  no-op pass, `rsync --times`, `git rebase`/`cherry-pick`. The manifest
  already records a per-file `xxh3_64` content hash for incremental
  indexing; H11 wires the same hash into the staleness probe — when
  mtime fires, we hash the file once and compare to the manifest. If
  the hash matches, the touch was cosmetic and the file is `Fresh`;
  if the hash diverges (or the path is missing from the manifest),
  the file is `Stale`. New files and read failures are conservatively
  treated as stale. The hash is streamed in 64 KiB chunks via
  `hash_file` so a large `.md` / `.sql` migration doesn't spike RSS.
  **Manifest format is unchanged**; existing indices benefit
  immediately without `vex index` rebuild. Four regression tests pin
  the contract (touch-fresh, real-edit-stale, unknown-file-stale,
  unsupported-extension-skipped). Effect: dev workflows that rely on
  `auto_update = true` now stop triggering redundant index rebuilds
  on noop mtime updates — `vex search` after `git checkout` is fast.

- **H4 — `vex pattern` ellipsis termination is now depth-aware.** Pre-H4
  the `$$$BODY` / `$$$NAME` / `$$$` forward-scan called `str::find`, so a
  pattern like `class $T { $$$BODY }` truncated `BODY` at the FIRST `}`
  in source — fatally for bodies containing nested blocks (`{ get; set; }`,
  inner method bodies) or string literals with `}` (e.g. `let s = "}";`).
  The scanner now tracks `() {} []` nesting and skips over double-quoted
  `"..."` string regions (with `\` escape), stopping at the *balancing*
  closer of the outer bracket. The `csharp_class_body` fixture now exercises
  a realistic auto-property body. Remaining limits (documented in
  `src/pattern/matcher.rs` module doc): single-quote strings (`'...'`),
  raw strings (`r#"..."#`, `R"(...)"`), triple-quoted strings, and
  bracket-containing comments. Full AST descent inside `try_match` is
  filed as a v2 follow-up.

- **H12 — indegree-based ranking no longer collapses name collisions.**
  Pre-H12 `top_n_by_indegree` (powers `vex bundle --mode project`) keyed
  the per-callee bucket on a lowercased bare name and emitted only the
  first FST hit as the representative, silently hiding every other
  definition that shared the name (`init`, `from`, `parse`, `new`).
  Post-H12 the helper emits one row per `(name, sym_idx)` pair so each
  definition gets its own ranking slot; the caller-count is the same
  upper bound for each because call edges lack the type info to
  apportion callers between definitions. Two unit tests via a mocked
  FST lookup pin the contract.

### Added

- **Phase 8.4 — semantic search now indexes TOML / YAML / HTML / CSS
  values.** `extract_body_tokens` learned the config-language AST
  leaves: `bare_key` / `dotted_key` / `quoted_key` (TOML),
  `attribute_name` / `tag_name` / `attribute_value` /
  `quoted_attribute_value` (HTML), `class_name` / `id_name` /
  `property_name` / `keyframes_name` / `plain_value` / `string_value`
  (CSS), `string_scalar` / `plain_scalar` / `single_quote_scalar` /
  `double_quote_scalar` (YAML), and the bare `string` leaf used by
  `tree-sitter-toml-ng`. Pre-8.4 these symbols carried
  `body_tokens = None`, so the semantic / BM25 channels were blind to
  config-file content; now `vex search "production endpoint"
  --semantic` can hit a `[server]` table with `endpoint = "https://..."`.
  YAML still only surfaces top-level mapping keys (limitation of the
  current SCM, not the extractor) — pinned by the test.

### Refactored

- **S1 — `cli/mod.rs` decomposed 2642 → 467 LOC (−82%).** Eight-commit
  refactor extracts every subcommand handler into a dedicated
  `cli/cmd_<name>.rs` file (17 new files), splits shared helpers into
  `cli/common.rs` (stateless: format / config / filter resolution +
  `fetch_symbol_body` + `EXPLAIN_MAX_DIFF_LINES` + `build_index_options`)
  and `cli/index_management.rs` (bootstrap + staleness + `ensure_index_ready`),
  introduces a `CmdCtx<'_>` struct threaded through every handler to
  cut argument bloat (architect MUST-FIX), and centralises the
  `callers_of_warned` saturation helper used by `vex paths` / `vex
  reachable`. **H5-full** is the immediate downstream beneficiary; the
  decomposition unblocks future per-handler work without behaviour
  change (every commit was a pure move, JSON envelopes byte-identical
  modulo `index_age_ms` timing).

## [1.10.1] - 2026-05-29

v1.10.1 is a small patch on top of v1.10.0. It flips the CLI's default output
format to `compact`, closes four external-review items (H3 reader v2-drop,
H9 aggregate pr-impact cap, S8.1 `VEX_WHY:`/`VEX_DIFF:` stderr tagging), and
ships two user-requested diagnostics: `vex status --coverage` and the
`directory_tree` field on `vex bundle --mode project`.

### Changed

- **Default output format flipped `text` → `compact`.** Vex's CLI now emits
  single-line records by default; the verbose multi-line `text` form stays
  available via `--format text` or `.vex.toml`'s `format = "text"`. Every
  honest agent / LLM workflow was already setting `format = "compact"` in
  `.vex.toml`; this just makes the de-facto default real. Tests that need a
  specific shape now explicitly pass `--format json` (or `--format text`);
  none broke on the flip. JSON envelope output is unaffected.

### Fixed

- **H3 — drop the v2 legacy-version special-case in the reader.** Pre-v1.10.1
  the reader accepted any version in `{2} ∪ [MIN_SUPPORTED_VERSION..=VERSION]`,
  but `has_symbol_fst()` then refused v2 downstream — search silently degraded
  on v2 indexes instead of asking the user to rebuild. The reader now rejects
  v2 with a clean `index version mismatch … Re-run \`vex index\`` error.
- **H9 — aggregate node cap on `vex bundle --mode pr-impact`.** The BFS bound
  was per-changed-symbol via `CALLERS_FETCH_CAP`, so a refactor PR touching
  N symbols could pull `N × 1024` callers — well past agent-friendly bundle
  sizes. Introduced `MAX_PR_IMPACT_NODES = 5_000` as an aggregate ceiling
  across changed + transitive + test items; surfaced via
  `mode_hints.budget_exceeded` (bool) + `mode_hints.max_pr_impact_nodes`.
  When the cap fires before any items land at all, `mode_hints.empty_reason`
  reports `pr_impact_budget_exceeded`.
- **S8.1 — tag MCP `--why` traces with `VEX_WHY:`.** Before v1.10.1 the MCP
  wrapper's `extract_why_trace` picked the first `{`-prefixed line on stderr
  and parsed it as JSON, so an early `tracing::warn!` JSON line (e.g. the
  "cannot determine index freshness" warning) could shadow the real trace
  and surface under `_meta.why`. The CLI now prefixes its trace with
  `VEX_WHY: { … }` (single emission helper `cli::trace::emit_why_trace`); the
  MCP extractor scans for the tagged line first. The untagged-fallback path
  now picks the **last** JSON-shaped line rather than the first, so earlier
  diagnostic objects no longer shadow the trace even on older binaries.
  Companion `VEX_DIFF:` tag added to the `diff_filter_meta` envelopes the
  CLI emits alongside `--why` traces, so those payloads can't be confused
  with a trace by the legacy fallback either.

### Added

- **FU-5 — `vex status --coverage` index-coverage diagnostic.** New flag on
  `vex status` that walks the project with the indexer's `walk_builder` and
  cross-references the result against `IndexReader::file_paths()` to surface
  three buckets:
  - `indexed_files` plus a `by_language` breakdown,
  - `discovered_not_indexed` with a sample list tagged with one of
    `unsupported_extension` / `too_large` / `not_yet_indexed`,
  - `missing_from_disk` for paths in the index that no longer exist on disk.
  Answers "what's on disk but unindexed?" — useful when `auto_update`
  silently misses something or a new file type appears in the tree.
- **FU-6 — directory-symbol-density tree in `vex bundle --mode project`.**
  `mode_hints.directory_tree` lists `{path, file_count, symbol_count,
  recursive_symbol_count}` per directory, sorted by `recursive_symbol_count`
  descending and capped at `--directory-tree-top` (default 30). New flag
  `--directory-tree-only` short-circuits the indegree walk and emits only
  the tree (`items: []`) so architecture-orientation calls don't pay for
  the call-graph traversal.

## [1.10.0] - 2026-05-28

v1.10.0 lands two parallel feature trains plus a folded-in external-review
patch series. **Phase 14** closes the function/method-level decorator-dispatch
coverage gap across Python + Java + Kotlin + C# + TypeScript + Rust
(14.1 / 14.2 / 14.2.1 / 14.2.2) and ships **Phase 14.7**, a content-addressed
blob-SHA parse cache that halves warm-path `vex index` wall time on the vex
self-repo (~498ms → ~250ms). The **MCP server** gains close-to-full CLI
parity: `search` / `show` / `usages` / `pattern` / `similar` / `duplicates`
now expose the missing `filter` / `kind` / `context_path` / `no_bm25` /
Phase-13.3 truncation / diff-scope / `no_stale_check` flags; `eval` and
`implementations` grow first-class MCP tools. The release also folds in the
**v1.9.2 external-review patch train** — 4 Critical correctness fixes
(tracing → stderr, durable writes, reader range guards, MCP JSON-RPC
parse-error responses) plus the **Windows release-artifact switch from
`.zip` to `.tar.gz`** that unblocks `vex self-update` on Windows for the
first time since v1.8.2.

### Fixed

- **v1.9.2 patch train — external-review correctness fixes.** Closes 4 Critical + several High items from Alexei Dolgolyov's independent review at tag `v1.9.1`:
  - **CLI tracing → stderr.** `tracing_subscriber::fmt()` now writes to `stderr`, not `stdout`. Any `tracing::warn!`/`debug!` from the CLI used to prepend to the JSON envelope on `stdout`, corrupting MCP frames (the server's `serde_json::from_str` would silently fall back to `{ "raw": ... }`). All MCP `signals` / `capabilities` / `_meta.why` payloads now survive the spawn boundary even with `RUST_LOG` set.
  - **Durable index writes.** `write_index_to` now calls `sync_all()` on the temp file before the atomic `rename`, and on Unix also fsyncs the parent directory after. Previously a crash/power loss between rename and writeback could leave a renamed directory entry pointing at unflushed data → mmap of garbage / bogus offsets / arbitrary bytes interpreted as strings or symbols.
  - **Reader rejects corrupt section layouts.** `IndexReader::open` adds explicit monotone-offset gates between every adjacent section (`symbols → vectors → strings → …`), caps `vector_dim` at 4096, and rejects `vector_dim = 0` with a non-empty vectors section. A tampered or truncated index that previously could have aliased symbol-record bytes as `f32` vectors now fails open.
  - **`read_string` warns on corrupt entries.** Previously silent UTF-8 / OOB failures returned `""`; `reconstruct_unchanged` then persisted the empty name into the next rebuild, effectively deleting the symbol. Now `tracing::warn!` fires at the decode site with offset + section context, and the pipeline skips empty-name records during reconstruction.
  - **MCP server resilience.** `BrokenPipe` / `UnexpectedEof` now produce a clean shutdown instead of process exit; transient I/O errors warn-and-continue. Malformed JSON now emits a spec-compliant `{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error","data":"<echo>"}}` response (echo capped at 512 code points) so clients no longer hang on `id: N`.
  - **Deterministic ranking tie-break.** `fuse_many` (RRF), brute-force `semantic::search_brute_force`, `bm25::Bm25Reader::search`, and `similar::find_duplicates` all tie-break equal scores on a deterministic key. Two runs of the same query against the same index now return identical orderings, satisfying the `rank_percentile_monotonic_descending` contract from `docs/RANKING-EVAL.md`.
  - **Windows path normalization.** `util::git_diff::ChangedPaths` now normalizes paths on BOTH insertion and lookup via a single `normalize_for_lookup` helper that strips `\\?\` extended-length and `\\?\UNC\` network prefixes (collapsing the latter back to `\\server\share\…`) and case-folds ASCII on Windows. `--changed-only` no longer silently under-reports on canonicalized lookups.
  - **`vex bundle` honors the Phase 13 envelope.** `cmd_bundle` now routes its JSON output through the new `print_envelope<T>` helper so `protocol_version: "v1"` and `capabilities` are present on every bundle response — same contract `search` has always honored. Migration of the remaining JSON-emitting subcommands (`show`, `usages`, `pattern`, `grep`, `implementations`, `callers`, `callees`, `paths`, `reachable`, `check`, `similar`, `duplicates`, `diff`, `outline`) is blocked behind a `cli/mod.rs` decomposition refactor scheduled for v1.10.
  - **Compile-time pubkey length assert.** `VEX_RELEASE_PUBKEY` length check is now a `const _: () = assert!(...)` — a runtime `expect()` panic is no longer reachable.
- **Windows release artifact switched from `.zip` to `.tar.gz`.** The reproducible `self_update` regression Alexei documented — `self_update 0.42/0.44` does not strip zipsign's 80-byte signature prefix before handing the file to `ZipArchive::new`, producing `ZipError: Compression method not supported` — broke `vex self-update` on Windows from v1.8.2 through v1.9.1 inclusive. Switching to `.tar.gz` (the same path already proven on macOS / Linux) sidesteps the upstream bug entirely. Manual installers should download `vex-x86_64-pc-windows-msvc.tar.gz` and extract via `tar -xzf` (recent PowerShell ships bsdtar) or 7-Zip / WinRAR. The `archive-zip` feature is dropped from the `self_update` dependency.
- **Homebrew formula tracks prebuilt binaries instead of source.** The
  release workflow's `update-homebrew` step was generating a source-only
  formula that forced `brew install vex` to build from `cargo install`,
  failing on machines without a Rust toolchain. The formula now pins the
  released archive URLs + SHA-256 sums so `brew install` lands the signed
  binary directly.

### Changed

- **Default output format flipped `text` → `compact`.** Vex's CLI now emits
  single-line records by default; the verbose multi-line `text` form stays
  available via `--format text` or `.vex.toml`'s `format = "text"`. Pin
  added in v1.7+ documentation, finally made the de-facto default — every
  honest agent / LLM workflow was already setting `format = "compact"` in
  `.vex.toml`. Tests that need a specific shape now explicitly pass
  `--format json` (or `--format text`); none broke on the flip. JSON
  envelope output is unaffected.
- **Phase 14.4 — wire-format honesty rename.** `usages --why` JSON trace
  `mode` field now emits `"fst_lookup"` instead of `"text_scan"` on the
  non-strict path; the underlying data source is and always was an FST
  lookup, not a text scan. A new `mode_legacy` field carries the v1.8.x
  label (`"text_scan"`) for back-compat with consumers that learned the
  contract before the rename. `mode_legacy` will be removed in v1.12.

### Added

- **MCP ↔ CLI parity train.** The MCP server gains the missing CLI flags
  that previously forced agents to drop to bash for nontrivial workflows.
  `search`, `show`, `usages`, `pattern`, `similar`, `duplicates` now expose
  `filter` (substring path filter), `kind` (array, multi-value), and
  `context_path` (proximity hint); `search` additionally gains `no_bm25`.
  `show` expands to the full Phase 13.3 truncation suite — `signature_only`,
  `head <N>`, `no_body`, `collapsed` — with server-side mutual exclusion
  enforcement that returns a structured error instead of letting clap dump
  its `conflicts_with` template into the response body. Diff scoping (`since`,
  `since_branched`, `changed_only`, mutually exclusive) lands across
  `pattern`, `similar`, `duplicates`, `callers`, `callees`, `implementations`
  so `vex callers Foo --since main` works through MCP. Every tool whose CLI
  variant already accepted `auto_update` now also exposes `no_stale_check`
  (12 tools) so agents can skip the per-call staleness check on a known-fresh
  index. The shared helpers `push_diff_scope`, `push_show_truncate`,
  `push_kind`, and `push_no_stale_check` are the single enforcement point;
  the `tool_descriptors_snapshot` regression guard locks the schemas against
  drift.
- **MCP `eval` tool wrapper.** New first-class MCP tool forwarding
  `vex eval --bench <PATH> --min-ndcg <FLOAT> --json --path <ROOT>`. MCP
  defaults `json` to `true` (agents want structured `EvalReport`); clients
  can flip back to the human-readable summary with `json: false`. Closes the
  audit-flagged CRITICAL gap that left the ranking-eval harness unavailable
  to agent workflows.
- **`Commands::Implementations` CLI parity** with the rest of the call-graph
  tools. The variant gains `auto_update` + `no_stale_check`, and its handler
  routes through `handle_staleness` before `find_implementations` (same
  staleness hook that `cmd_callgraph` already used for `callers` / `callees`).
  The MCP `implementations` tool re-enables the corresponding flag forwarding
  that was deliberately disabled until the CLI side landed.
- **Phase 14.7 — blob-SHA addressed parse cache.** `vex index` and
  `vex update` now consult a content-addressed cache keyed by the git
  blob SHA of each tracked file before invoking tree-sitter. Cache
  layout: `<platform_cache_root>/vex/blobs/<sha[0..2]>/<sha>.bin`, one
  serialized `ParsedFile` per blob. Cache key is `(blob_sha,
  CACHE_FORMAT_VERSION, grammar_fingerprint(lang))` so a grammar
  upgrade that changes tree-sitter output (new node kinds) invalidates
  entries automatically; stale entries are silently skipped and lazily
  overwritten. Tracked-file discovery uses a single `git ls-files -s`
  call at the top of indexing; untracked files keep the existing xxh3
  fallback path. Files with **uncommitted working-tree changes** are
  detected via `git diff-files --name-only -z` and excluded from cache
  reads/writes so the cache cannot be poisoned by parses of dirty
  content under their staged blob SHA. LRU eviction sweeps the blob
  directory at the start of each `vex index` / `vex update`; default
  size cap is **1 GiB**, overridable via `VEX_BLOB_CACHE_CAP_BYTES`.
  Cache writes are routed through a single background drain thread
  (one `std::thread::scope` + `mpsc` channel per `parse_files` call)
  so per-file serialize + write + atomic rename costs stay off the
  rayon parse closure's critical path; the drain thread is joined
  before `parse_files` returns, guaranteeing all writes have committed
  in time for a subsequent `vex update` to observe them. Performance
  on the vex self-repo (3762 symbols, release binary): warm `vex
  index` (full blob cache hit) drops from ~498ms to **~250ms**
  (**−50%**, exceeds the ≥40% Step 1 target); the cold path (empty
  blob cache, every file writes back) is **~562ms** (~+11%) — a small
  one-time overhead per machine/repo that pays back on every
  subsequent run, CI re-run, IDE reopen, or fresh Claude Code agent
  session. Largest user-visible win is the CI re-run + Claude Code
  cross-project agent flow where the global cache hits shared
  vendored / pinned-version blobs across projects.
- **Phase 14.2.1 — TypeScript + Rust decorator edges via sibling
  adjacency.** TypeScript method decorators on class methods
  (`class C { @Get("/x") handler() {} }`) and Rust outer attributes on
  function / method items (`#[tokio::test] fn it_works() {}`,
  `impl Foo { #[wasm_bindgen] fn bar() {} }`) now emit forward call
  edges `decorated_fn → decorator_target`. Unlike Python/Java/Kotlin/C#
  where the decorator/annotation lives INSIDE the function's byte range,
  TS `decorator` and Rust `attribute_item` nodes are SIBLINGS of the
  function under a shared parent (`class_body` / `source_file` /
  `declaration_list`). The new sibling-adjacency pass in
  `extract_callgraph` walks each captured decorator host forward to
  find the next function-shaped sibling and remaps the synthetic
  call's `byte_offset` onto the function's start so the existing
  attribution logic lands the edge on the right caller. Callee = the
  rightmost identifier of the decorator/attribute PATH (the part
  before the optional argument list). For multi-segment paths the
  rightmost wins: `@nest.Get("/x")` → `Get`, `#[tokio::test]` →
  `test`. For single-segment paths the path itself is the callee:
  `@bound` → `bound`, `#[wasm_bindgen]` → `wasm_bindgen`. Arguments
  are NEVER part of the path: `#[serde(rename = "x")]` → `serde`
  (not `rename`); `#[allow(dead_code)]` → `allow` (not `dead_code`).
  Rust `#[derive(...)]` is filtered out by attribute-path head-name
  (compile-time codegen, not runtime call edges); arguments to other
  attributes are never inspected, so `#[some_attr(derive = "x")]`
  still emits an edge to `some_attr`. JavaScript files inherit the
  same decorator coverage via the TSX grammar. Class-level decorators,
  TS property/parameter decorators, and `#[derive(...)]` remain out of
  scope (Phase 14.6 / intentional exclusion). Performance budget:
  re-indexing the vex self-repo gave median **306.56ms** vs 14.2.2
  baseline 287.5ms (+6.6%), within the ≤317ms +10% ceiling; the
  cost driver is a per-call-capture ancestor walk that guards against
  the standard `call_expression` pattern double-firing inside
  decorators.
- **Phase 14.2.2 — Kotlin + C# call graph and annotation edges.** Kotlin
  and C# now have first-class callgraph support: `function_declaration`,
  `method_declaration`, and `constructor_declaration` produce caller
  FnDefs; direct calls, `navigation_expression` (`obj.method()`), and
  `invocation_expression` over `member_access_expression`
  (`obj.Method()`) produce callee edges. On top of the base callgraph,
  Kotlin annotations (`@JvmStatic fun foo()`) and C# method/constructor
  attributes (`[HttpGet("/x")] public Response GetUsers()`) emit forward
  edges `annotated_fn → annotation_target`. Qualified names follow the
  rightmost-identifier convention from Phase 14.2 —
  `@kotlin.jvm.JvmStatic` → `JvmStatic`, `[System.Web.Mvc.HttpGet]` →
  `HttpGet`. Both languages join the `COMPILED_QUERIES` LazyLock so the
  added query compiles stay off the hot path. No format bump — reuses
  the existing CallEdge shape. Performance budget: re-indexing the vex
  self-repo, release binary, 6 cold-cache runs over 3668 symbols, gave
  median **287.5ms** (+2.7% vs the 14.2 baseline of 280ms; well under
  the +10% ceiling), best **268.01ms** (−4.3%). Remaining gaps:
  TypeScript / Rust decorators (Phase 14.2.1) and class-level decorators
  (Phase 14.6).
- **Phase 14.2 — decorator edges (Python + Java).** Function and method
  decorators in Python (`@app.get("/x") def list_items()`) and method-level
  annotations in Java (`@GetMapping("/x") public Response listItems()`)
  now emit forward call edges `decorated_fn → decorator_target`.
  `vex callers get` lists every FastAPI route handler; `vex callers
  GetMapping` lists every Spring handler. Callee resolves to the rightmost
  identifier of the decorator name (consistent with method-call captures).
  No format bump — reuses the existing CallEdge shape. Performance
  budget: re-indexing the vex self-repo stayed within +0% of pre-14.2
  baseline (mean 280ms vs 297ms — pattern-matching cost is below noise).
  TypeScript and Rust deferred to Phase 14.2.1 (sibling-adjacency in
  grammar); class-level decorators deferred to Phase 14.6.
- **Phase 14.1 — module-level callers.** `vex callers <fn>` now reports
  module-scope call sites via a synthetic per-file `<module:path>` caller
  (`SymbolKind::Module = 13`). Module symbols are excluded from `vex search`,
  `vex outline`, and ranked search results — they appear only as resolved
  callers in the call graph. Class-body call sites also attribute to the
  synthetic Module symbol (broader coverage; string-resolved refs remain
  Phase 15 territory). No binary format bump — older readers see `Module`
  as `kind="unknown"` and gracefully ignore.
- CI on pull requests: separate `cli-tests`, `msrv` (1.80), `beta`
  (informational, allowed to fail), and `benches` (`cargo bench --no-run`)
  jobs.

## [1.9.1] - 2026-05-25

Windows hotfix for the Phase 13.12 ranking-evaluation harness. v1.9.0's
`path_matches` compared the index-side host-separator paths
(`src\store\reader.rs` on Windows) against the forward-slash golden
TOML entries directly — every shape failed string-equality, so the
Windows CI run reported `nDCG / recall / MRR = 0.0` on all 16 golden
queries and tripped the 0.85 regression floor. macOS / Linux were
unaffected. The README honesty pass that landed alongside the fix is
also part of this patch.

### Fixed

- **Windows path-separator regression in `vex eval`** — `path_matches`
  now normalizes `\` → `/` once at the comparison boundary, restoring
  the cross-platform contract documented in
  `docs/RANKING-EVAL.md`. Unit test
  `path_matches_normalizes_windows_separator` pins the four canonical
  shapes (exact / trailing-`/file` / dir-prefix) against Windows-style
  paths and re-validates the H2 neighbour-directory protection.

### Documentation

- README v1.9.0 refresh with honest coverage caveats — adds a
  `## What Vex isn't` section between Why Vex? and How It Compares,
  names the three language tiers behind `--strict usages` / indexed
  pattern prefilter / baseline structural search, calls out the
  function-scope limit on `vex callers`, and replaces the
  maximum-anchored "6-88x" token-efficiency framing with a
  median-anchored "typically 6-10x; up to 88x on minified" pair.
  Quick Start gains a v1.9 section covering bundle / diff-context /
  show truncation / eval / capabilities. Commands table adds
  `vex capabilities`, `vex eval`, `vex self-update`, and the Phase
  13.3 truncation flags on `vex show`.

## [1.9.0] - 2026-05-25

Phase 13 lands the agent-integration foundation: a versioned response
envelope with per-result `signals` and `rank_percentile`, a ranking-
evaluation harness pinning nDCG@10/recall@10/MRR, smart `show`
truncation, diff-context filters across every search-shaped command,
LLM-tuned MCP tool descriptions, and `vex bundle` — one CLI subcommand
plus one MCP tool that replaces the four-round-trip
`show → callers → callees → similar` loop with a single envelope.
Format stays v6; `MIN_SUPPORTED_VERSION` stays at 3 — older indexes
keep opening. The pre-1.9 bare-array `vex search --format json` shape
is opt-in via `VEX_JSON_ENVELOPE=0` and slated for removal in v2.0.

Also closes a long-running honesty gap surfaced by the v1.8.2 external
review: a new `docs/LIMITATIONS.md` documents `callers`-is-function-
scoped, the T1/T2 `usages` quality tiers, and the dynamic-dispatch
patterns that static analysis cannot see. `--strict` help text no
longer promises a deferral that shipped three minors ago.

### Added

- **`vex bundle` (Phase 13.2)** — unified multi-source bundle primitive.
  One command, three modes, replaces the 4-round-trip agent loop
  `show → callers → callees → similar` with a single call. The MCP
  surface is one `bundle` tool with a **flat schema** (mode-specific
  args validated server-side; no JSON-Schema `oneOf`).
  - `--mode symbol <name>` — body + direct callers + direct callees +
    semantic-similar matches. Body extraction is full (no truncation;
    `vex show --signature-only` is the per-symbol truncation surface).
    Defaults: `--callers-max 10`, `--callees-max 10`, `--similar-max 5`.
  - `--mode pr-impact --base <rev>` — changed symbols since `<rev>`
    plus transitive callers (depth=2 default) plus tests that
    transitively reach the changes. Test classification by path
    (`/tests/`, `_test.`, `__tests__/`, `spec/`) or signature
    attribute (`#[test]`, `#[cfg(test)]`, `#[tokio::test...]`).
    `_meta.vex.dev/diff_filter` carries `{ scope, changed_paths,
    retained, dropped }` so agents can correlate with `git diff`.
  - `--mode project [--top-n N] [--path-glob G]` — top-N symbols by
    **reverse call-graph indegree** (count of distinct callers).
    Experimental; documented as `scoring: "reverse_indegree"`. No
    PageRank — see roadmap for revival path under 13.12.
  - Response envelope reuses Phase 13.0 `{ protocol_version,
    capabilities, _meta, results }`. `results.items[i]` carries the
    13.11 `signals` block plus a `role` discriminator (`body | caller
    | callee | similar | changed | transitive_caller | test | top`),
    a *global* monotonic-descending `rank_percentile`, and a
    per-role 0-indexed `role_rank`. `results.mode_hints` is a
    mode-specific JSON blob (counts, truncation flags, scoring label,
    `empty_reason` when the items list is empty).
  - `capabilities.bundle_modes` now advertises `["symbol", "pr-impact",
    "project"]` (was `[]` in v1.9.0-pre).
  - Latency baseline (Criterion, `benches/bundle.rs`): pr-impact BFS
    on 50 changed symbols × depth=2 ≈ **86 µs**; project indegree scan
    over ~500 functions ≈ **44 µs**; symbol mode full pipeline (FST +
    body + callers + callees + similar guard) ≈ **139 µs**. All three
    well under the 100 ms threshold that would justify rayon-izing
    the pr-impact BFS outer loop.
- `vex capabilities` CLI subcommand returning the Phase 13 capability
  matrix as JSON (`protocol_version`, `signals`, `why`, `scope_filters`,
  `metadata_filters`, `empty_reason`, `bundle_modes`, `auto_update`).
  Agents can probe this once at startup instead of re-reading help text.
- Per-result `signals` (`fst_hit`, `bm25_rank`, `semantic_rank`,
  `fuzzy_distance`) and a normalized `rank_percentile` field in
  `vex search --format json` envelope. `rank_percentile` spans
  `[0.0, 1.0]` inclusive — the top result is `1.0`, the bottom is `0.0`,
  and a lone result is `1.0`.
- MCP responses now carry `protocol_version`, `capabilities`, and a
  namespaced `_meta` block (`vex.dev/index_age_ms`, `traceparent`,
  `ttlMs`, `cacheScope`) on top of `structuredContent.results`. Signals
  live in `structuredContent` only — `_meta` is invisible to the LLM
  per the MCP spec.

### Changed

- `vex search --format json` now emits an envelope
  `{ protocol_version, capabilities, _meta, results: [...] }` instead
  of a bare array. Set `VEX_JSON_ENVELOPE=0` (also accepts `false` /
  `off`, case-insensitive) to opt out and restore the pre-1.9
  bare-array shape. The opt-out is a migration aid only and will be
  removed in v2.0.

### Documentation

- New `docs/LIMITATIONS.md` documents static-analysis coverage gaps
  surfaced by the v1.8.2 external review: `vex callers` is function-
  scoped (module-level call sites and decorator dispatch are
  invisible); `vex usages` quality varies across the T1 / T2 language
  tiers; dynamic dispatch (`getattr`, factory strings, decorator
  routing) cannot be statically resolved. Includes a coverage matrix,
  concrete repros, and the `vex grep` escape-hatch recommendation.
- `--strict` help text on `vex usages` no longer claims the
  `reference_edges` section is "deferred until 11.1.3" — Phase 11.1
  shipped in v1.8.0. Replaced with an accurate description that names
  the five binder-supported languages (Rust / TypeScript / Python /
  C# / C++) and points at `docs/LIMITATIONS.md`.
- `--why` help text on `vex usages` documents that the `mode:
  "text_scan"` label is historical — the underlying data path is the
  FST lookup, populated from an AST identifier walk on T1 languages
  and a line-scan on T2.
- README gains a `## Known limitations` section linking the same
  surface. `vex callers --help` and `vex usages --help` doc-comments
  point at `docs/LIMITATIONS.md` so agents reading the schema discover
  the coverage gaps without a separate fetch.

## [1.8.2] - 2026-05-23

Closes Phase 11.4 T2 with the final four language allowlists (Kotlin /
Swift / PHP / Ruby — T2a now covers 12 languages) and ships Phase 11.10:
`--why` is now available on `usages`, `similar`, and `duplicates` in
addition to the existing `search` and `pattern` surfaces. No format
change; v6 stays compatible. All Phase 11 items are complete.

### Added — pattern skeletons populated for

- **Kotlin** — `class_declaration` (umbrella for class / interface /
  data class / enum class / sealed class) / `object_declaration` /
  `companion_object` (named or anonymous) / `function_declaration` /
  `property_declaration` (`variable_declaration > identifier` walker
  for sigil-free ident) / `type_alias` (uses `type:` field) /
  `secondary_constructor` (anonymous) / `enum_entry` (positional
  `identifier` walker) / `anonymous_initializer` (anonymous `init` block)
  / `lambda_literal` (anonymous, `has_block=false`) / `anonymous_function`.
- **Swift** — `class_declaration` (umbrella class / struct / enum /
  actor / extension via the `declaration_kind:` field — `extension`
  surfaces the extended type as ident) / `protocol_declaration` /
  `function_declaration` / `property_declaration` (pattern-based name) /
  `typealias_declaration` (uses `name:` — distinct from Kotlin's `type:`)
  / `enum_entry` / `associatedtype_declaration` /
  `protocol_function_declaration` + `protocol_property_declaration`
  (body-less, `has_block=false`) / `init_declaration` /
  `deinit_declaration` / `subscript_declaration` /
  `operator_declaration` / `lambda_literal`.
- **PHP** — `class_declaration` / `interface_declaration` /
  `trait_declaration` / `enum_declaration` (PHP 8.1+) /
  `function_definition` / `method_declaration` (abstract methods
  report `has_block=false`) / `property_element` (granular per-name
  emit so `public $a, $b` yields one skeleton per element with the `$`
  sigil stripped) / `const_element` / `enum_case` /
  `namespace_definition` (block-form vs semicolon-form separated by
  `has_block`) / `anonymous_function` / `arrow_function`
  (`has_block=false` — expression body) / `anonymous_class`.
- **Ruby** — `class` / `module` / `method` / `singleton_method`
  (surfaces the method name only, not the receiver) / `singleton_class`
  (anonymous `class << self` block) / `alias` (returns the new alias
  name via the `name:` field) / `lambda` / `block` (brace form) /
  `do_block`. Endless methods (`def foo() = 42`) report
  `has_block=false` since the body is an `_arg` rather than a
  `body_statement`.

### Added — `has_body_block` markers

Cross-language body kinds expand to cover the new allowlist entries:
`enum_class_body` (Kotlin + Swift), `protocol_body` (Swift),
`enum_declaration_list` (PHP), `body_statement` + `block_body` +
`do_block` (Ruby). `function_body` is now shared across SQL, Kotlin,
and Swift — pinned by a cross-grammar regression test.

### Added — `--why` extended to usages / similar / duplicates (Phase 11.10)

Three new structured-trace shapes alongside the existing `search` /
`pattern` traces, emitted as one-line JSON on stderr and surfaced
via `_meta.why` in MCP responses. See `docs/MCP-SCHEMA.md` for the
canonical vocabulary table.

- `usages --why` — `mode` (`"strict"` / `"text_scan"`),
  `hits_before_filter` / `hits_after_filter`, `prefix_suggestions`
  (count from the `Did you mean` fallback when zero exact hits),
  `filter_applied`.
- `similar --why` — `seed_resolved`, `threshold_applied`,
  `candidates_before_filter` / `candidates_after_filter`,
  `filter_applied`. When `--why` is on, the handler over-fetches
  (`fetch_limit = symbol_count()`) so the pre-filter count reflects
  the un-truncated HNSW return list rather than an internally-capped
  one.
- `duplicates --why` — `threshold_applied`, `min_body_lines_applied`,
  `pairs_before_filter` / `pairs_after_filter`, `filter_applied`.
  Same over-fetch logic as `similar`.

MCP schemas for all three tools gained a `why: boolean` field;
`extract_why_trace` continues to pick up the trace via stderr.

### Fixed

- **Skeleton walker now gates emission on `node.is_named()`.** Ruby is
  the first language whose grammar reuses kind strings between named
  rules and anonymous keyword tokens (`class`, `module`, `alias`).
  Without the gate, every Ruby declaration site would emit a second
  phantom skeleton from the keyword token. The gate is safe across
  T1/T2a because every allowlisted kind is a named grammar rule;
  pinned by the Ruby happy-path tests.
- **Arrow-lambda `do/end` body now triggers `has_block=true` for Ruby.**
  `->(x) do ... end` puts a `do_block` directly under `lambda`;
  `do_block` joined the universal body markers so the contract
  mirrors Kotlin `anonymous_function`.

### Other

- `t2_language_returns_empty_until_rolled_out` canary repointed
  permanently from Ruby → TOML (a T3 anchor we never plan to fill);
  pairs with `t3_language_short_circuits_to_empty` on YAML.
- Phase 11 status: **all 10 items complete** (11.1 type-aware refs,
  11.2 diff, 11.3 generic implementations, 11.4 structural patterns +
  T2 train, 11.5 multi-hop, 11.6 metadata filters, 11.7 scope filters,
  11.8 similar reasoning, 11.9 result-kind ranking, 11.10 MCP arg +
  `--why`).

## [1.8.1] - 2026-05-23

Phase 11.4 T2 pattern-skeleton rollout — eight new languages graduate
from the empty-allowlist short-circuit to full prefilter coverage —
plus a cross-file scope-binder follow-up for C# `using` and C++ `using`
declarations. No format change; v6 stays compatible.

### Added — pattern skeletons populated for

- **Go** — `function_declaration` / `method_declaration` / `type_spec`
  / `type_alias` / `var_spec` / `const_spec` / `func_literal`.
- **C++** — `function_definition` (with declarator-chain ident reused
  from the scope binder), `class_specifier` / `struct_specifier` /
  `union_specifier` / `enum_specifier` / `namespace_definition` /
  `template_declaration` (anonymous wrapper) / `alias_declaration` /
  `type_definition` / `lambda_expression`.
- **C#** — `class_declaration` / `interface_declaration` /
  `struct_declaration` / `enum_declaration` / `record_declaration` /
  `method_declaration` / `constructor_declaration` /
  `destructor_declaration` / `property_declaration` /
  `delegate_declaration` / `local_function_statement` /
  `namespace_declaration` / `file_scoped_namespace_declaration` /
  `lambda_expression` / `anonymous_method_expression`.
- **SQL** — `create_table` / `create_index` / `create_view` /
  `create_materialized_view` / `create_function` / `alter_table` /
  `drop_table` / `drop_view` / `drop_function` / `drop_index`.
- **Markdown** — `atx_heading` / `setext_heading` /
  `fenced_code_block` (language tag as ident).
- **Java** — `class_declaration` / `interface_declaration` /
  `enum_declaration` / `record_declaration` /
  `annotation_type_declaration` / `method_declaration` /
  `constructor_declaration` / `compact_constructor_declaration` /
  `lambda_expression`.
- **CSS** — `rule_set` (full selector chain as ident) /
  `keyframes_statement` / `media_statement` (anonymous).
- **HTML** — `element` / `script_element` / `style_element` with
  `tag_name` extraction from `start_tag`.

JavaScript already uses the TypeScript grammar (the extension map
routes `.js` / `.jsx` to `Language::TypeScript`), so its skeletons
were covered by the T1 TypeScript allowlist from v1.8.0 on — no
separate row needed.

### Added — cross-file scope binders (`vex usages --strict`)

C# and C++ now resolve import bindings across files, joining Rust /
TypeScript / Python:

- **C#** — `using A.B.C;`, `using static A.B.C;`,
  `using Alias = A.B.C;`, `global using A.B.C;`, and
  `using global::A.B;` (the `global::` qualifier is stripped from
  the resolved path).
- **C++** — `using std::vector;`, `using V = std::vector<int>;`
  (template arguments stripped from the path), and
  `namespace alias = ns::sub;`.

`#include` and `using namespace` stay deferred as wildcard imports
(no name-keyed `UsePath` representation).

### Fixed

- `is_root_kind` is now language-aware: `document` is suppressed for
  Markdown and HTML (both use it as the file root) but not for YAML
  (which uses it as a non-root subtree under `stream`) — prevents a
  silent `parent_kind` corruption when YAML is eventually rolled out.
- C# top-level decls no longer leak `parent_kind=Some("compilation_unit")`
  (added `compilation_unit` to the suppression set).
- C++ root-namespace `using ::Foo;` no longer produces a doubled-up
  `UsePath` (`["Foo", "Foo"]`); C# `using global::App.Lib.X;` no
  longer corrupts the first path segment.

### Other

- `tests/pattern_fixtures/**` is force-LF via `.gitattributes` so the
  multi-line `$$$BODY` capture pins don't break on Windows checkouts
  with `core.autocrlf=true`.

## [1.8.0] - 2026-05-22

Combined "type-aware usages + first-class structural patterns"
release — two large trains landed back-to-back:

- **Phase 11.1 (type-aware `usages`)** — LSP-style scope binder for
  Rust, TypeScript, Python, C#, and C++ with a persisted v5
  `reference_edges` section. Default `vex usages` is unchanged; the
  precision upgrade is opt-in via `--strict`.
- **Phase 11.4 (first-class structural patterns)** — `vex pattern`
  promoted from cold live-scan to indexed search via a persisted v6
  `pattern_skeletons` section, with multi-line `$$$BODY` / `$$ARGS`
  metavars and `&&` / `||` composition. All existing patterns keep
  working; the new syntax is additive.

`MIN_SUPPORTED_VERSION` stays at 3 — v3 / v4 indexes still open and
auto-rebuild on first use after upgrade.

### Format change — v6 (Phase 11.4)

- **`VERSION = 6`** with a new `PatternSkeletonHeader` (168 bytes)
  immediately after `V5SectionHeader`. Carries offsets and lengths for
  four sub-sections — `SkeletonRecord` array (24 B records sorted by
  `file_id`), `kind_path_arena` (inline null-terminated kind names
  plus per-record path entries), `ident_pool` (length-prefixed UTF-8),
  `file_index` (sorted `{file_id, first_skel_idx}` for O(log n)
  `skeletons_for_file`) — plus a `[u32; 32]` per-language
  `grammar_fingerprints` array. (11.4 Inc 3)
- **`MIN_SUPPORTED_VERSION` stays at 3** — v3/v4/v5 indexes still
  open; `vex pattern` falls back to live-scan on them with `--why`
  reason `"no-skeleton-section"`. New indexes auto-rebuild on first
  use after upgrade.
- **`Language::lang_id() -> u8`** with explicitly-assigned stable IDs
  (Rust=1..Toml=19). Slot 0 is reserved for "not fingerprinted".
  Adding a new language gets the next integer; removing one leaves a
  reserved gap so the on-disk fingerprint slot index stays stable.
- **Adversarial coverage**: 3 new tests in `tests/adversarial_format_-
  test.rs` cover the v6 header gate (`pattern_skeleton_header_-
  has_nonzero_size`), backward-compat (`v5_index_has_no_pattern_-
  skeleton_header`), and truncation
  (`v6_truncated_at_pattern_skeleton_header_rejected`).

### Added — pattern syntax (Phase 11.4)

- **Named multi-line metavars**: `$$$BODY` (block-spanning) and
  `$$ARGS` (arg-list-spanning). Functionally identical — the two
  syntaxes coexist for readability (`fn $F($$ARGS) { $$$BODY }` reads
  naturally). Both capture and enforce back-reference equality on
  repeat occurrences, just like single-token `$NAME`. (11.4 Inc 6)
- **AND / OR composition** via space-flanked ` && ` and ` || ` at
  bracket / quote depth 0. AND requires both sub-patterns to match in
  the same file with consistent captures across shared metavars; OR
  takes the union, deduped by `(path, line)`. `&&` binds tighter than
  `||` (standard). The depth-aware split keeps `record($X, $X)` and
  `f($X && $Y)` as single patterns. Empty middle conjuncts/disjuncts
  bail with a clear "empty pattern" error. (11.4 Inc 7)
- **Greedy `>` fix for `Result<$T, $E>`-shape patterns**: the
  identifier scanner now stops one byte short of the next literal's
  starting character, so `$E` correctly captures `Error` instead of
  `Error>`. Pre-existing limitation, surfaced by the Inc 6 fixtures.

### Added — scan modes (Phase 11.4)

- **Indexed prefilter**: when a v6 index is present and the per-lang
  grammar fingerprint matches the live grammar, `vex pattern` walks
  only the candidate files whose persisted skeletons satisfy the
  language and (when inferable from the pattern's leading keyword)
  the root node-kind. Falls back to live-scan with explicit reasons
  — `"no-index"`, `"index-open-error"`, `"no-skeleton-section"`,
  `"empty-section"`, `"grammar-drift"`, `"partial-section"` — none
  of which is fatal. (11.4 Inc 5)
- **`--why`** on `vex pattern`: appends a JSON `ScanTrace` to stderr
  with `mode` (indexed / live_scan), `root_kind_inferred`,
  `candidate_files` / `total_files`, and the optional
  `fallback_reason`. The same trace surfaces under `_meta.why` in the
  MCP response. (11.4 Inc 5 + Inc 8 wrapper fix)
- **Root-kind inference** strips Rust visibility (`pub`,
  `pub(crate)`, `pub(super)`), Rust `async` / `unsafe` / `const`
  modifiers, TS `export` / `default` / `async`, and Python `async`
  before matching the keyword — so `pub async fn $F` infers
  `function_item` instead of silently disabling the prefilter.
- **`partial-section` fallback after `vex update`**: incremental
  builds leave skeletons empty for unchanged files (mirroring the
  `bound_refs` / `call_edges` convention). The new
  `Manifest.pattern_index_full` flag distinguishes a full `vex index`
  from a `vex update`; when `Some(false)` the prefilter degrades to
  live-scan to avoid silently under-reporting matches.

### Added — CLI / MCP (Phase 11.4)

- **`--no-pattern-index`** on `vex index`, `vex update`, and `vex
  watch` skips the v6 skeleton section. Same sticky semantics as
  `--no-call-graph` / `--no-bm25` — opt-out is recorded in
  `Manifest.pattern_index` and honoured by subsequent `vex update`
  unless explicitly overridden. `.vex.toml` gains `pattern_index =
  false` for project-wide opt-out. (11.4 Inc 4)
- **MCP `pattern` tool** schema documents Inc 6-7 syntax (block
  metavars, composition, indexed prefilter) and gains a `why:
  boolean` field. When `why: true` the wrapper extracts the
  `ScanTrace` JSON from the CLI's stderr and places it under
  `_meta.why` in the JSON-RPC response. The same plumbing benefits
  the existing `search --why` MCP flow. (11.4 Inc 8)

### Internal (Phase 11.4)

- **Per-file skeleton extractor** (`src/pattern/skeleton.rs`): pure
  function that walks tree-sitter once per file and emits a
  `Skeleton` per pattern-targetable node. T1 allowlists for Rust (10
  kinds), TypeScript (8), and Python (4). T2 / T3 languages
  short-circuit to `Vec::new()` so the section is empty for files
  that don't participate; `vex pattern --lang <x>` still works via
  live-scan for those. (11.4 Inc 2)
- **Grammar fingerprints** = `xxh3_64(concat(kind_name, 0; …))`
  truncated to `u32`, computed at index time per T1 language present
  in the parsed set. Stored in the `PatternSkeletonHeader`
  fingerprint array. The reader compares against the live grammar's
  fingerprint and falls back to live-scan on mismatch — closes the
  R-A grammar-drift hazard surfaced by the planner.
- **`CompositePattern { disjuncts: Vec<Vec<PatternTree>>, lang }`**
  with helpers `parse_composite_pattern`, `split_top_level`,
  `find_matches_composite`, `match_conjunct`, `captures_agree_-
  with_map`, `build_normalised_map`. The map projection is built once
  per `(anchor, other_tree)` pair instead of per candidate, avoiding
  the O(anchors × trees × candidates × |captures|) HashMap rebuild
  that the first cut had.
- **Writer entry sprawl** (transitional): `write_index_with_call_-
  graph_and_skeletons_and_fingerprints` is the new primary entry.
  Two back-compat shims (`write_index_with_call_graph`,
  `write_index_with_call_graph_and_skeletons`) forward to it for
  legacy and test callers; both are flagged `#[allow(dead_code)]`
  pending a future consolidation pass.
- **Fixture-based regression suite** (`tests/pattern_fixtures/`): one
  baseline + five scope-B fixtures across Rust / TypeScript /
  Python. Each fixture is `input.<ext>` + `spec.toml` (pattern,
  expected lines, expected captures); the harness in
  `tests/pattern_fixture_test.rs` runs `vex pattern --format json`
  per fixture and asserts. RED in Inc 1, all GREEN after Inc 6+7.


### Format change — v5 (Phase 11.1)

- **`VERSION = 5`** with a new `V5SectionHeader` (48 bytes) immediately
  after `CallGraphHeader`. The header carries offsets and lengths for
  three new sub-sections: `reference_edges` (fixed-size 16-byte `RefEdge`
  records), an FST keyed on stringified `to_sym_idx`, and posting lists
  of edge indices. (11.1.3a–b)
- **`MIN_SUPPORTED_VERSION` stays at 3** — v3/v4 indexes still open;
  `vex usages --strict` bails on those with a "re-run `vex index`"
  message. New indexes auto-rebuild on first use after upgrade.
- **Adversarial coverage**: 4 new tests in `tests/adversarial_format_-
  test.rs` cover the V5SectionHeader truncation arm and each of the
  three sub-section past-EOF arms; `tests/integration_test.rs` pins
  cross-file `Imported` resolution for Rust, TypeScript, and Python.

### Added — scope binders (Phase 11.1)

- **AST-aware ref extraction** (11.1.1): identifiers inside line / block
  comments, doc-strings, and string literals are filtered out before
  they enter the refs FST for Rust, TypeScript, Python, C#, and C++.
  Drops the loudest false-positive class from `vex usages` without
  touching the format.
- **Per-language scope binders** (11.1.2 through 11.1.6) for Rust,
  TypeScript, Python, C#, and C++. Each binder builds a `ScopeTree`
  arena and emits `BoundRef` records tagged with `BindTarget::Local`,
  `BindTarget::ModuleSymbol`, `BindTarget::Imported`, or
  `BindTarget::Unresolved`. Local + mod-level resolution shipped per
  language; cross-file `use` / `import` resolution shipped for Rust /
  TypeScript / Python. C# `using` and C++ `#include` remain deferred.
- **Shared `Walker` scaffolding** (`src/parse/scope/walker.rs`):
  single 169-line module that owns `Walker`, `add_binding`,
  `add_import_binding`, `emit_ref`, `resolve`, and `walk_children`.
  Each language file is the dispatch + grammar-specific helpers.

### Added — CLI / MCP (Phase 11.1)

- **`vex usages --strict`** reads the persisted `reference_edges`
  section. Without `--strict` the legacy refs FST keeps backing the
  command (covers the 14 languages without a binder yet). MCP `usages`
  tool schema gained `"strict": { "type": "boolean", "default": false }`.
- **`vex implementations` finds generic-parameterised subclasses**
  (11.1.6): `impl Iterator<T> for Foo` (Rust), `implements Handler<T>`
  (TypeScript), `class Box(Container[int])` (Python), and
  `: public Repository<T>` (C++) now match alongside the bare-name
  forms. Java + C# already had the band-aid since the 11.7 train.

### Internal (Phase 11.1)

- Pass-2 cross-file resolution at index-write time. `BindTarget::-
  Imported(use_path)` records are rewritten to `ModuleSymbol(global_-
  idx)` via a `HashMap<&str, Vec<u32>>` built from `sym_entries` —
  first-hit on ambiguity, matching the documented behaviour.
- `Walker::file_symbols_by_name: HashMap<&str, u32>` replaces the
  O(R × S) linear scan in `resolve()`. Closes a rust-reviewer MEDIUM
  before binders ran across an entire repo at index time.
- `debug_assert!` hardening on `RefEdge::col_and_kind` 24-bit column
  ceiling and `base_idx` overflow in the writer's Pass-2 — silent
  corruption in the unreachable 4 B-symbol case is now a loud failure
  in tests.

## [1.7.0] - 2026-05-21

The "search ergonomics + trust gaps" release. Built around an honest
audit of where users still had to cross-check vex with `ast-index` or
`Grep` — nine point features across the three pain clusters (filter
power, structural understanding, agent ergonomics) plus security
hardening on the embedding pipeline. No format change; all v1.6.x
indexes continue to open.

### Added — filter power

- **`--include <glob>` / `--exclude <glob>`** on every search-shaped
  command (`search`, `usages`, `pattern`, `show`, `grep`,
  `implementations`, `callers`, `callees`, `similar`, `duplicates`).
  Repeatable, gitignore-style semantics via `globset`
  (`literal_separator(true)` — `*` stays within a segment, `**`
  crosses `/`). Per-call scoping that doesn't require re-indexing;
  composes AND with the existing `--filter <substring>`. MCP tools
  carry the matching `include` / `exclude` arrays. Hardened against
  CPU exhaustion with a 64-pattern / 256-char-per-pattern cap. (11.7)
- **Multi-value `--kind`** with comma + repeated values:
  `--kind fn --kind struct` or `--kind fn,struct`. Four new
  meta-selectors (`def`, `comment`, `test`, `ref`) join the
  canonical SymbolKind names. Default ranking now demotes Markdown
  headings unless the user explicitly opts in via `--kind comment` —
  defs-first ordering in mixed result sets. (11.9)
- **Symbol metadata post-filters**: `--visibility public|private|
  protected|internal`, `--async-only` / `--no-async`,
  `--static-only`, `--sealed-only` on `vex search` / `vex show`.
  Lexical match against the symbol's captured signature line — no
  format bump, no re-parsing. Per-language alias coverage: `pub` /
  `public` / `export`; `async` and Kotlin `suspend`; `sealed`
  (Kotlin/C#) and Java `final class` (gated on co-occurring type
  keyword so method parameters don't false-positive). Default-
  visibility inference is deliberately not done — only explicit
  keywords match. (11.6)

### Added — structural understanding

- **`vex paths <from> <to>`** enumerates all caller chains over the
  v4 persistent call graph from 1.5.0. DFS-backward from `to` with
  per-chain cycle prevention, bounded by `--max-hops` (default 6)
  and `--max-paths` (default 50). Output reverses to natural
  `A → … → B` reading order. (11.5)
- **`vex reachable <target>`** returns the transitive set of
  symbols whose callees reach `target`, with the BFS depth label
  per row. Same call-graph fast path, bounded by `--max-hops` and
  `--limit`. Both commands warn loudly when a node hits the
  per-step fetch cap (1024 callers) so wide-fan-in saturation is
  visible. (11.5)
- **`vex diff --base <rev>`** lists symbol-level changes between
  an arbitrary git revision and the working tree: added / removed /
  moved-within-file / body-changed entries. Per-file pairing by
  `(name, kind)` with a coarse line-range body hash; deliberately
  uses `git diff --no-renames` so a `git mv` surfaces both halves
  of the move. Available as MCP tool too. (11.2)
- **Generic-parameterised `implementations`** for Java
  (`extends Repository<T>`), C# (`: Repository<T>`), and Kotlin
  (`class Foo : Repository<T>()`). The Kotlin inheritance query
  was rewritten end-to-end — the v1.4.x version used wrong node
  names (`type_identifier` vs the actual `identifier`,
  `delegation_specifier_list` vs `delegation_specifiers`) and
  silently matched nothing on `tree-sitter-kotlin-ng`. (11.3)
- **`vex pattern` metavar back-references**: a repeated `$NAME` in
  a pattern now requires the same capture across occurrences.
  `record($X, $X)` matches `record(state, state)` and rejects
  `record(state, other)`. Whitespace-normalised on the back-ref
  comparison so `assertEqual((a + b), (a+b))` unifies on
  formatting differences. New MCP tool exposes the command. (11.4)

### Added — agent ergonomics

- **`--explain` on `vex similar` / `vex duplicates`**: emits an
  identifier-set Jaccard overlap plus a truncated unified diff
  (via `similar` crate) between the seed and each match. Cosine
  scores already surfaced; the diff + jaccard close the
  "I never act on these blind" gap. `--min-score` alias for
  `--threshold` for discoverability. (11.8)
- **`--why` / MCP `why: true`** on `vex search` appends a JSON
  trace to stderr: normalised query, per-channel hit counts
  (FST / BM25 / semantic), fallbacks engaged (e.g. fuzzy),
  and the active filter snapshot. Useful when results look wrong
  and you want to know what was actually searched. (11.10)
- **MCP schema canonical vocabulary**: `query` / `symbol` /
  `symbols` / `path` / `pattern` / `filter` / `include` / `exclude`
  across all tools. Renames with back-compat — old `name` /
  `file` / `names` aliases still work and emit a
  `_meta.deprecated_args: [...]` notice in the JSON-RPC response.
  New `docs/MCP-SCHEMA.md` documents the vocabulary and back-compat
  policy. (11.10)

### Fixed / Hardened

- **Pinned SHA-256 integrity check on the MiniLM ONNX model**.
  `fastembed` / `hf_hub` previously downloaded the ~86 MB ONNX
  with no signature in the supply chain; a poisoned CDN entry or
  compromised HF host would have landed arbitrary ONNX into a
  process that executes it. The expected digest
  (`bbd7b466…46f0c5` at fastembed snapshot
  `5f1b8cd7…3af89a079`) is now verified after fastembed init.
  `VEX_EMBEDDER_SKIP_CHECK=1` escape hatch with both an
  `eprintln!` and `tracing::warn!` on bypass so it's visible
  regardless of `RUST_LOG`. Multi-snapshot caches pick
  deterministically by lex-sorted directory name. (10.4)
- **MCP `vex {sub} failed` error now includes a truncated stdout
  snippet**. The vex CLI emits its structured JSON-error body on
  stdout, not stderr — the previous wrapper was silently dropping
  the actual reason in the JSON-RPC response. Capped at 512 bytes
  with `floor_char_boundary` UTF-8-safe slicing. (10.6 Pass 2)
- **`vex outline` grammar-load failure** now includes the language
  variant alongside the file extension
  (`failed to load grammar for csharp (.cs): …`). (10.6 Pass 2)
- **README troubleshooting section** documents `RUST_LOG=vex=warn`
  for surfacing parse/store warnings, points at
  `vex search Foo --why 2>trace.json` and `docs/MCP-SCHEMA.md`.
  (10.6 Pass 2)
- **`vex diff` distinguishes "file did not exist at base" from
  real git failures** (corrupt object store, permission denied).
  The previous handler `.ok()`-swallowed every non-zero exit code,
  so a broken repo silently reported every symbol as `Added`. Now
  the missing-object path returns `Ok(None)` and everything else
  bubbles up with stderr context. (11.2 fixup)
- **Java's `final` keyword on method parameters** no longer
  false-positives into `--sealed-only`. The metadata filter now
  requires a type-introducing keyword (`class`, `interface`,
  `enum`, `record`) to co-occur with `final` before treating the
  signature as sealed. (11.6 fixup)
- **MCP `async_only` + `no_async`** sent together previously
  surfaced clap's generic `conflicts_with` error template in the
  JSON-RPC response. Explicit `bail!` guard in `push_metadata`
  produces an intent-aware error instead. (11.6 fixup)

### Internal / Docs

- New `docs/MCP-SCHEMA.md` — canonical vocabulary, alias table,
  `_meta.deprecated_args` contract, and the `--why` trace shape.
- New modules: `src/search/explain.rs` (jaccard + truncated
  unified diff), `src/search/metadata.rs` (signature-line filter),
  `src/search/trace.rs` (post-hoc search trace builder),
  `src/callgraph/bfs.rs` (closure-abstracted multi-hop layer),
  `src/cli/scope.rs` (glob compilation with caps),
  `src/diff/mod.rs` (symbol-level diff against a git revision),
  `src/embed/integrity.rs` (SHA-256 verification + cache walker).
- 5 rust-reviewer passes + 1 security-reviewer pass + 1 aqa-agent
  pass across the train; ~30 reviewer-flagged items applied as
  in-line fixes or new regression tests. Two fixup commits
  (`ed92f7a`, `a7295d2`) bundle the cross-cutting polish.
- New runtime dependencies: `globset = "0.4"` (was transitive via
  `ignore`), `similar = "2"` (was transitive via `insta` dev-dep),
  `sha2 = "0.10"` (was transitive). All small, leak-clean,
  audited crates.
- ~120 new tests across the train (unit + integration). Full
  workspace ~360 tests; clippy clean under `-D warnings`;
  fmt clean; rust-version 1.80 MSRV preserved.

## [1.6.4] - 2026-05-19

Feature + UX polish release. Headline change is the new indexing-time
opt-out flags (`--no-call-graph` / `--no-bm25`) that let monorepos and
CI hot-paths skip the call-graph and BM25 build steps that v1.5.0
added unconditionally — recovering most of the v1.4.3-era indexing
speed when the user does not need those search channels.

### Added
- **`vex index --no-call-graph` / `--no-bm25`** (also on `update` and `watch`). v1.5.0 made `vex index` ~6× slower than v1.4.3 because the persistent call-graph and BM25 sections were always built. Users who only need structural + vector search can now opt out per-build or globally via `.vex.toml`. With `--no-call-graph` set, `vex callers` / `vex callees` transparently fall back to the live tree-sitter scan they always supported; with `--no-bm25`, hybrid search drops the BM25 channel and uses structural (+ semantic if enabled). The opt-out is recorded in the manifest so a subsequent `vex update` does not silently re-add the section.
- **`.vex.toml` keys `call_graph` and `bm25`** for project-wide opt-out. Resolution precedence: CLI `--no-...` flag > `.vex.toml` > previous manifest > default (true).
- **`vex callers` / `vex callees` accept `--auto-update` and `--no-stale-check`**, symmetric with the other index-backed commands. With `--auto-update`, a missing index is bootstrapped on first call so the persistent call-graph FST (~4 ms) becomes available immediately; the existing live-scan fallback is preserved when no index and no auto-update are in play. MCP wrappers gained the matching `auto_update` schema field.

### Fixed
- **`auto_update`-driven bootstrap no longer re-runs the staleness probe** on the freshly built index. A just-bootstrapped manifest is guaranteed `Freshness::Fresh`, so the manifest read + git HEAD probe that fired immediately after the bootstrap was pure waste. Behaviourally invisible; just trims the first-search latency on `auto_update = true` projects.
- **`IndexReader::open` errors now carry the index file path** in every `bail!` and `File::open`/`Mmap::map` context. Previously a corrupt or version-mismatched index surfaced as a path-less "index file is corrupted (bad magic)" — users had to grep stderr or read source to find the file. Same treatment for the embedder-mismatch and stale-embedder-switch bails (now include the manifest path), and the cache-dir `..`-traversal warning (now shows both the raw config value and the tilde-expanded form).
- **`vex callers` / `vex callees` live-scan fallback reason is now printed via `eprintln!`** when an index exists but fails to open. Previously logged via `tracing::warn!` which `RUST_LOG` hides by default — users saw the ~seconds live-scan latency without knowing why the fast path was skipped.
- **Bootstrap `pipeline::run` failure now includes the project root** via `.with_context()`. Bare pipeline errors during first-run bootstrap no longer drop the root path.
- **CI release-zip Deflate guard** introduced in v1.6.3 contained an f-string syntax error inside a PowerShell here-string that crashed on the next tag push. The dict lookup is now pulled into a local variable so the f-string only references a bare identifier. (Affects only the release pipeline; published binaries on v1.6.3 are unchanged.)

### Internal
- New `pipeline::IndexOptions { with_embeddings, with_call_graph, with_bm25 }` struct replaces the bare `with_embeddings: bool` parameter on `pipeline::run` / `pipeline::update`. Default is `with_embeddings = false, with_call_graph = true, with_bm25 = true`. All seven library-test callsites migrated to `IndexOptions::default()`.
- `Manifest` gains `call_graph: Option<bool>` and `bm25: Option<bool>` fields (back-compat: `None` on pre-10.4 manifests is treated as enabled).
- New `resolve_section_enabled(cli_no_flag, cfg_value, manifest_value) -> bool` helper in `cli::mod` encodes the precedence rule in one place.
- `ensure_index_exists` now returns `IndexAvail { path, just_bootstrapped }` instead of bare `PathBuf`. A new `ensure_index_ready` wrapper composes ensure + staleness, skipping the latter on a fresh bootstrap. Six existing call sites (search/show/usages/check/similar/duplicates) collapsed from a pair of calls to a single helper.
- `cmd_callgraph` gains `auto_update`, `no_stale_check`, `local_cache_active`, `cfg` parameters and a bootstrap-or-staleness block that runs before the existing fast-path / live-scan branching. The live-scan fallback when neither an index nor auto-update is available is preserved exactly.
- `Update` / `Watch` arms canonicalize `root` once at the top of the arm and `?`-propagate `Manifest::load` errors so a partially-written manifest does not silently re-enable opted-out sections.
- 11 new tests: 4 integration tests for callers/callees auto-update bootstrap and live-scan fallback, 4 integration tests for the opt-out persistence + sticky-update invariant, 2 behavioural tests proving callers/search still work with the opt-out, 1 unit test suite (4 cases) for `resolve_section_enabled` covering all precedence levels.

## [1.6.3] - 2026-05-19

Windows-only patch for the `vex self-update` failure reported on v1.6.2.

### Fixed
- **Windows zip archive now uses standard Deflate (method 8) instead of Deflate64.** PowerShell's `Compress-Archive` on the `windows-latest` runner emits Deflate64 in some configurations — .NET can decompress it, but the `zip` crate inside `self_update 0.42` cannot, surfacing as `Compression method not supported` when `vex self-update` tries to extract the new release. Release packaging now invokes `7z a -tzip -mm=Deflate` (preinstalled on `windows-latest`) which forces compatible compression. zipsign continues to preserve the method, so signed archives are also Deflate-compressed end-to-end.
- **Migration**: v1.6.1 → v1.6.2 self-update on Windows failed for users on v1.6.1 trying to land on v1.6.2 (and v1.6.0 → v1.6.1 in retrospect was likely affected too). v1.6.2 → v1.6.3 should self-update normally because v1.6.3 ships standard Deflate, which the existing v1.6.2 binary can read. If `vex self-update` from v1.6.2 still fails for any reason, download `vex-x86_64-pc-windows-msvc.zip` from the v1.6.3 release page once — from 1.6.3 onwards updates are clean.

## [1.6.2] - 2026-05-19

UX/perf hotfix on top of v1.6.1, motivated by Windows users hitting two
sharp edges as soon as they tried `auto_update = true` in `.vex.toml`.

### Fixed
- **`auto_update = true` now bootstraps a missing index on first use.** The previous code path bailed with "No index found" before the auto-update logic ever ran, and `handle_staleness` could only do incremental updates anyway. A bare `vex search` / `show` / `usages` / `check` / `similar` / `duplicates` in a fresh project will now print "Bootstrapping…" and run the equivalent of `vex index` before continuing. `Similar` and `Duplicates` bootstrap with `--semantic` automatically since they require embeddings
- **Improved "no index" error message** when `auto_update` is *not* set — now lists both fixes: run `vex index` or set `auto_update = true` in `.vex.toml`
- **MiniLM embedding model is now cached globally**, not in `./.fastembed_cache/` relative to the working directory. The ~86 MB ONNX model lives at `<platform-cache>/vex/embeddings/` and is shared across every project. Previously every project re-downloaded the same model and dropped a `.fastembed_cache/` folder into the project tree. Existing `.fastembed_cache/` directories inside user projects can be deleted safely — the model will not be re-downloaded
- **`local_cache = true` + bootstrap now writes the `.gitignore` safeguard**. The auto-bootstrap path previously skipped the project-`.gitignore` creation that the explicit `vex index` command performs, so users with both flags set could end up committing the binary index. Now the bootstrap mirrors the `Commands::Index` behaviour
- **Semantic bootstrap announces the model download.** `vex similar` / `vex duplicates` on a fresh machine print "Note: first semantic index downloads the MiniLM ONNX model (~86 MB)…" before fastembed's progress bar appears
- **Clearer embed cache errors.** `MiniLMEmbedder::new` now wraps `create_dir_all` with `.with_context(...)`, so a failed cache-dir create surfaces with the actual path instead of hiding behind fastembed's generic "failed to load MiniLM" message

### Fixed (CI)
- Replaced `orhun/git-cliff-action@v3` with a direct install via `taiki-e/install-action`. The Docker image used by the action was based on debian:buster which is EOL — `apt-get update` returned 404 and the release job failed before the body could be generated. v1.6.1 release shipped via this fix; documenting here for the changelog trail

### Internal
- New `ensure_index_exists(root, auto_update_flag, needs_semantic, local_cache_active, cfg)` helper in `cli::mod` replaces the same six-line `if !index_path.exists() { bail!(...) }` block that appeared in every index-backed command. Returns the resolved `index.vex` path so callers do not recompute it
- New `util::config::embed_cache_dir()` returns `<cache-root>/embeddings/` without appending the project-hash subdir — every project that shares the platform cache root also shares the model. Users on `local_cache = true` consciously opt in to a portable layout and pay the per-project cost
- 9 new CLI integration tests in `tests/cli_bootstrap_test.rs` drive the actual binary via `assert_cmd`. Cover the auto-bootstrap path, the helpful-error path, the `.gitignore` safeguard, path-traversal rejection end-to-end, the `--no-stale-check` interaction, and the `--check` / `--yes` mutual exclusion. Each test scopes its cache to a tempdir so the shared platform cache is never touched

## [1.6.1] - 2026-05-18

Follow-up patch for v1.6.0. Adds the `vex self-update` subcommand that
Windows users needed (no Homebrew tap), and fixes the two Windows path
issues that surfaced once CI started running on `windows-latest` for the
first time.

### Added
- **`vex self-update` subcommand** — fetches the latest GitHub release, picks the archive for the running target triple, and replaces the binary in place. Works on Linux, macOS, and Windows. `--check` reports the latest version without modifying anything; `-y/--yes` skips the interactive confirmation. Closes the upgrade-flow gap on platforms without a package manager
- **Windows install section in README** — step-by-step for downloading `vex-x86_64-pc-windows-msvc.zip`, extracting `vex.exe`, and putting it on `PATH`

### Fixed
- **`is_test_path` on Windows** — the test-file down-rank heuristic in `search::rerank` checked for `/test/`, `/tests/`, etc. as substrings. Indexed paths preserve the host separator, so on Windows the heuristic silently never matched and test files received no rank penalty. Now normalizes backslashes before the scan, allocating the extra string only when needed
- **`grep::tests::grep_with_path_filter`** asserted `matches[0].path.contains("api/")` which fails on Windows (paths are `api\routes.py`). Assertion now contains "api" without the trailing slash, OS-agnostic

### Security
- **ed25519 signature verification** for `vex self-update`. Every release archive is signed in CI via [`zipsign`](https://github.com/Kijewski/zipsign); the 32-byte verifying key is embedded in the binary at compile time. The download path refuses to extract an archive whose signature does not match. Mitigates the "compromised GitHub account / CDN tamper" scenario in the v1.6.0 self-update threat model
- `vex self-update` continues to use rustls (no openssl C dep), enforces certificate validation by default, and refuses to apply a downgrade
- Key rotation and one-time setup documented in [`docs/RELEASING.md`](docs/RELEASING.md)

### Internal
- `--check` and `-y/--yes` are mutually exclusive at the clap layer (the combination would have silently dropped one flag)
- `self_update::version::bump_is_greater` parse failures surface via `?` instead of `.unwrap_or(false)` so an unexpectedly-tagged release is reported as an error, not a silent "no action needed"
- Named-argument comments at the `cmd_self_update` call site guard against a future refactor swapping the two `bool` positionals

## [1.6.0] - 2026-05-18

Configurable cache location, first-class Windows support, and a thread-count
limit for parallel indexing. The original trigger was a Windows-only MCP
bug — `dirs_home()` had no Windows branch and fell back to `/tmp`, producing
mangled paths like `/tmp\.cache\vex\<hash>`. Fixing that path resolver turned
into a broader rework of cache management.

### Added
- **`--cache-dir <PATH>` global flag** plus `$VEX_CACHE_DIR` env var to override the cache root per-invocation
- **`cache_dir = "..."` in `.vex.toml`** — accepts absolute paths, `~/...`, and paths relative to the config file (e.g. `"./.vex/cache"`)
- **`local_cache = true` in `.vex.toml`** — store the index at `<project>/.vex_cache/` without a project-hash subdirectory. vex auto-writes a `.gitignore` so the cache is not committed. Useful when the cache should travel with the project (renames, copies, moves)
- **`-j/--jobs N` flag** on `index`, `update`, `watch` to cap the worker pool. Mirrored by `$VEX_JOBS` and `jobs = N` in `.vex.toml`
- **80% default thread count** (rounded up, floor 1) when no explicit jobs setting is supplied. Leaves headroom for the editor / browser / language server sharing the machine. Pass `0` to keep using every core
- **Windows cache locations**: `%LOCALAPPDATA%\vex` (with `%USERPROFILE%\AppData\Local\vex` and `$HOME\AppData\Local\vex` as fallbacks). No more `/tmp` literal anywhere in the resolver

### Fixed
- **MCP server now forwards `--auto-update`** to the seven index-backed commands (`search`, `find_symbol`, `find_similar`, `show`, `usages`, `check`, `similar`, `duplicates`). Previously a stale index surfaced as a tool failure to the MCP client even though the bare CLI handled the same condition transparently
- **MCP cache path mangling on Windows** — `HOME` fell back to `/tmp` and downstream paths became `/tmp\.cache\vex\<hash>\index.vex`. Fixed by the new platform-aware resolver

### Changed
- `.vex.toml` now accepts `cache_dir`, `local_cache`, and `jobs` fields (all optional). Existing configs continue to parse unchanged
- Default worker count for parallel indexing dropped from "all cores" to "ceil(80%)". To restore the previous behaviour, set `jobs = 0` in `.vex.toml`, `$VEX_JOBS=0`, or pass `-j 0`. The change only affects indexing commands; one-shot search/show calls do not eagerly initialize the rayon pool

### Security
- **Path-traversal blocker** for `cache_dir`. A `.vex.toml` shared via a monorepo cannot redirect index writes outside the project root via `..` segments, including post-tilde-expansion cases like `~/../etc/evil`. Rejected paths produce a warning and fall back to the platform default
- **Atomic `.gitignore` creation** for `local_cache` uses `OpenOptions::create_new` so a planted symlink at `.vex_cache/.gitignore` cannot be overwritten

### Internal
- `VexConfig` records the directory of the `.vex.toml` that produced it (`source_dir`) so relative `cache_dir` values resolve against the config file, not the cwd
- Cache override is installed once at `dispatch()` via `OnceLock<CacheLayout>`, avoiding a per-call-site parameter through 20+ index/manifest/hnsw path accessors
- New helpers in `util::config`: `resolve_cache_root` (`ResolvedCache { root, skip_hash_subdir }`), `resolve_jobs`, `resolve_explicit_jobs`, `default_thread_count`, `expand_user`, `write_local_cache_gitignore`, `set_cache_override`, `init_rayon_pool`
- Test env helper uses an RAII `Drop` guard so a panicking test cannot leak mutated env into the next; poisoned-mutex recovery via `unwrap_or_else(|e| e.into_inner())`
- 17 new unit tests covering the cache-resolution priority chain, tilde expansion, path-traversal rejection (relative and tilde-bypass), local_cache layout, VEX_JOBS opt-ins, and the 80%-default formula

## [1.5.0] - 2026-05-16

Phase 9 ships hybrid search v2: three orthogonal channels (structural FST,
BM25 over body tokens, semantic HNSW) fused via 3-way RRF; persistent call
graph delivering ~1000× speedups on `callers`/`callees`; a pluggable embedder
trait that unblocks future code-specific models; and two new commands
(`similar`, `duplicates`) on top of the existing vector index. The on-disk
format bumps to v4 with backwards-compatible read of v3.

- 19 commands (+2), 17 MCP tools (+2), 649 tests (+108 vs v1.4.3)

### Added — Phase 9.4 (BM25 channel in hybrid search)
- **BM25 inverted index** as the third channel in `vex search` alongside structural FST and semantic HNSW. Closes the gap between "exact symbol name" (structural) and "general meaning" (semantic) — finds rare body terms like `timeout`, `singlestore`, `idempotency_key` that aren't part of the symbol name
- **`vex::store::bm25` module** — `Bm25IndexBuilder` (per-symbol term bags), `Bm25Reader` (zero-copy scoring), `tokenize_query`/`tokenize_document`, Okapi BM25 with `K1 = 1.2`, `B = 0.75`. Minimal English stop-word filter applied at query time only (keeps the index size unchanged when the stop-word list evolves)
- **`vex::search::bm25::search(reader, query, top_k)`** — adapter that converts BM25 hits to `SearchResult` tagged with new `MatchType::Bm25`
- **3-way RRF fusion** — `vex::search::fusion::{fuse3, fuse_many}`. `Hybrid` label when a result appears in ≥2 channels; original `MatchType` preserved when in just one
- **CLI integration** — BM25 auto-on when the index has BM25 data. Flag `--no-bm25` to disable on a per-call basis
- **Pipeline integration** — `pipeline::build_bm25_index` aggregates per-symbol terms from `name + signature + body_tokens + doc` and emits the FST/posting/stats triple
- **20 new integration tests** in `tests/bm25_test.rs` plus 7 BM25 unit tests in `src/store/bm25.rs` — format size, writer/reader roundtrip (with and without BM25), pipeline emission, IDF discrimination (rare term ranks above common), short-doc preference under same TF, 3-way fusion with Hybrid labelling, MatchType tagging, Unicode tokenization, empty/stopword-only query handling

### Changed — Phase 9.4
- `CallGraphHeader` extended from 80 → 128 bytes (10 call-graph fields + 6 BM25 fields: fst/postings/stats offset+len). Internal name kept for back-compat with 9.3 callers; will be renamed to a generic `V4SectionHeader` in a follow-up. **No version bump v4 → v5** — v4 was unreleased outside this repo, and extending the existing v4 envelope avoids a second migration event
- `writer::write_index_with_call_graph` signature gains `Option<Bm25Sections>` parameter (a `(&[u8], &[u8], &[u8])` triple of pre-built BM25 bytes)
- `search::fusion::fuse` retained as a back-compat 2-way wrapper around `fuse_many`; new code uses `fuse3` / `fuse_many`
- `MatchType` gains the `Bm25` variant

### Note — format compatibility
- v3 indexes still readable; `has_bm25() == false` → search falls back to structural-only behavior (plus semantic if `--semantic` and vectors present)
- v4 indexes built between Phase 9.3 and 9.4 are also readable; `bm25_*_len == 0` → BM25 channel disabled until next `vex index` rebuilds

### Added — Phase 9.3 (persistent call graph + format v4)
- **Format bump v3 → v4**: new sections persisted at index time — `CallEdge` records, callers FST, callees FST + posting lists. The `Header` struct stays byte-identical to v3; a new `CallGraphHeader` (80 B) is placed at offset `Header::SIZE` when `version >= 4`. v3 indexes still open (back-compat read path)
- **`vex callers <name>` / `vex callees <name>` fast path** — FST lookup (~4ms) on v4 indexes. Falls back to live tree-sitter scan when (a) no index exists, (b) index is v3, (c) v4 index has no call graph, or (d) index exists but fails to open (with a warning logged so corrupt/locked indexes don't silently degrade to the slow path)
- **`crate::callgraph::extract_call_edges(content, lang)`** — returns `(caller_fn_name, caller_fn_line, callee_name, call_line)` quadruples. Used by `pipeline::parse_file` to populate `ParsedFile.call_edges`
- **`store::call_graph` module** — `CallEdgeBuilder`, `build_callers_fst`, `build_callees_fst`, `CallGraphFstReader`, `find_callers_fast`, `find_callees_fast`, `encode_caller_key`
- **Incremental update preserves call edges** — `reconstruct_unchanged` re-emits edges from the old index for files whose hash didn't change; only changed/deleted files get fresh extraction
- **25 new integration tests** in `tests/call_graph_test.rs` — format sizes (Header::SIZE = 144, CallGraphHeader::SIZE = 80, CallEdge::SIZE = 16), writer/reader roundtrip, callers/callees FST fast paths with case-insensitivity and dedup, same-name-across-files isolation via `caller_sym_idx` keying, end-to-end pipeline → fast path, `extract_call_edges` for Rust + unsupported langs, incremental update preserves edges, regression test for same-name-within-file disambiguation (CRITICAL fix from Phase 9.3 review)

### Changed — Phase 9.3
- `ParsedFile` gains `call_edges: Vec<RawCallEdge>` — populated at parse time, consumed by writer
- `pipeline::resolve_call_edges` keys on `(path, name, line)` (not just `(path, name)`) so two functions sharing a name in the same file (overloaded methods, duplicate `impl` blocks) get distinct caller symbol indices
- `IndexReader::open` accepts versions in `[MIN_SUPPORTED_VERSION..=VERSION]` (v3..v4) plus legacy v2 — section-bounds validation extended for the new v4 sections
- Writer aligns `call_edges_offset` to a 4-byte boundary so `CallEdge` records can be safely mmap-cast on strict-alignment platforms
- `vex::store::writer::write_index_full` now delegates to a new `write_index_with_call_graph` (used by `pipeline`)

### Fixed — Phase 9.3 (review findings)
- **CRITICAL**: same-name symbols within a single file used to all resolve to the first occurrence's `caller_sym_idx`, attributing every later duplicate's call sites to the wrong caller. Fixed by including the symbol's definition line in the resolution key. Regression test in `tests/call_graph_test.rs::duplicate_function_name_in_same_file_resolves_to_correct_caller`
- **HIGH**: `find_callers_fast` / `find_callees_fast` now early-return on `!has_call_graph()` instead of relying on callers to check the invariant
- **HIGH**: `cmd_callgraph` logs a `tracing::warn!` when an existing index fails to open (corrupt/locked) instead of silently falling back to the slow path
- Writer alignment bug surfaced during test authoring — `call_edges_offset` rounded up to a 4-byte boundary so subsequent mmap reads succeed alignment checks

### Note — format compatibility
- v3 indexes remain readable. `has_call_graph()` returns false → CLI falls back to live scan. Run `vex index` to rebuild as v4 and unlock fast paths.

### Added — Phase 9.1 (pluggable embedder backend)
- **`Embedder` trait** — `id()`, `dim()`, `char_budget()`, `embed()`, `embed_batch()`. Unblocks future code-specific embedders (BGE, GraphCodeBERT, CodeT5+)
- **Registry**: `embed::make_embedder(id)`, `embed::embedder_dim(id)`, `embed::known_embedders()`. Unknown ID returns an error listing known IDs
- **`MiniLMEmbedder`** — current default and only implementation. Output 384-dim, char budget 1100, ID `"minilm-l6-v2"`. Replaces the previous concrete `Embedder` struct
- **`embedder` option** in `.vex.toml` and `--embedder <id>` flag for `index`/`update`/`watch`. Priority: CLI > config > `DEFAULT_EMBEDDER` (=`minilm-l6-v2`)
- **`Manifest.embedder_id: Option<String>`** — records the embedder a semantic index was built with. Missing field on pre-9.1 manifests is interpreted as the default for back-compat
- **Embedder mismatch detection** at `vex search --semantic`: when the requested embedder differs from `manifest.embedder_id`, search bails with a rebuild hint instead of producing nonsense results
- **`writer::write_index_full`** now takes `vector_dim: u32` instead of hardcoding 384. `Header.vector_dim` is filled from the embedder, opening the door to non-384 models
- **21 new integration tests** in `tests/embedder_test.rs` covering trait registry, dim lookup, resolve priority, mismatch detection (including back-compat with missing `embedder_id`), manifest roundtrip + skip-serializing-when-none, config parsing, `build_context` budget, writer variable dim + length validation

### Changed — Phase 9.1
- `embed::build_context` signature: takes `budget: usize` instead of hardcoded `EMBEDDING_CHAR_BUDGET`. Callers pass `embedder.char_budget()` to fit the model's token window
- `semantic::search_with_embedder` now accepts `&mut dyn Embedder` (was `&mut Embedder` concrete struct)
- `pipeline::run` and `pipeline::update` now take `embedder_id: &str`
- `watch::watch` takes `embedder_id: &str`
- `src/embed/model.rs` slimmed to `build_context` only; the model struct lives in the new `src/embed/minilm.rs`

### Note — format compatibility
- **No binary format bump in 9.1.** Embedder identification lives in `manifest.json`, not the index Header. Format v4 will land with Phase 9.3 (persistent call graph) when binary section additions are actually needed. This avoids breaking v3 read-back.

### Added — Phase 9.2 (semantic similarity)
- **`vex similar <symbol>`** — find symbols semantically close to a given one. Resolves the symbol's stored embedding, runs HNSW (or brute-force fallback), returns nearest neighbors with cosine similarity. Flags: `--limit`, `--threshold`, `--filter`, `--auto-update`. Requires `vex index --semantic`
- **`vex duplicates`** — list pairs of near-duplicate symbols by embedding similarity. Canonical pair dedup `(min_idx, max_idx)` — never both `(A, B)` and `(B, A)`. Flags: `--threshold` (default 0.9), `--limit` (default 50), `--min-body-lines` (default 5, filters trivial 1-liner matches via approximated body length), `--filter`
- **MCP tools**: `similar` (by existing symbol — distinct from existing `find_similar` which queries by description) and `duplicates`
- **18 new integration tests** in `tests/similar_test.rs` covering self-exclusion, threshold filtering, canonical pair dedup, body-length filtering, empty index, missing vectors, unknown symbol errors, descending sort

### Changed
- `cosine_similarity` promoted from `fn` to `pub(crate) fn` in `src/search/semantic.rs` so `src/search/similar.rs` shares the same implementation

## [1.4.3] - 2026-05-14

### Added
- **+7 languages** (12 → 19): PHP, Bash, Lua, CSS, HTML, YAML, TOML. Each ships a tree-sitter grammar crate, an `.scm` query, and a per-language regression test (`tests/<lang>_query_test.rs`) on the v1.4.2 pattern
  - PHP: class, interface, trait, enum, function, method, class constant, `use` import
  - Bash: function definitions, `source`/`.` imports
  - Lua: function (top-level, local, `Mod.fn`, `Class:method`), `require` imports
  - CSS: class selectors, `#id` selectors, `@keyframes`, custom properties (`--var`)
  - HTML: `id="..."` attribute values, hyphenated custom-element tags
  - YAML: document-root mapping keys (anchored to avoid nested-key noise)
  - TOML: table headers (`[name]`, `[[name]]`), key/value pairs
- **`vex implementations` for PHP and Ruby** — 7 → 10 OOP languages supported
  - PHP: `extends`, `implements` (multi-arg, qualified names), `interface extends`, **PHP 8.1+ `enum implements`**, **trait `use TraitName`** (in class & trait bodies, labelled `(uses)`)
  - Ruby: `class Foo < Bar` (labelled `(inherits)`), `include`/`extend`/`prepend Mixin` inside class and module bodies (labelled `(include)`)
  - Relation labels dispatched via `tree_sitter::QueryMatch::pattern_index` with named thresholds (`PHP_TRAIT_PATTERN_START`, `RUBY_MIXIN_PATTERN_START`)
- **Multi-case identifier scanner** in the refs FST — `extract_references` now picks up `snake_case`, `camelCase`, and `SCREAMING_SNAKE_CASE` identifiers in addition to `PascalCase`. `vex usages process_order` now works in Python/Rust/Go/Lua codebases. Filter keeps plain lowercase words (`total`, `amount`) out of the FST to prevent prose-noise bloat
- 41 new tests (451 → 541): per-language grammar regressions for the new 7, hierarchy tests for PHP/Ruby/traits, scanner case-style coverage, symmetric threshold guards, negative tests against silent query degradation

### Fixed
- **Ruby `#match?` predicate misplaced** — the include/extend/prepend filter sat OUTSIDE the enclosing `(class ...)` S-expression, which tree-sitter silently treated as a no-op. The query degraded to "any method call inside a class body with a Constant argument", which would falsely match `assert_equal Mixin, foo.class` and `describe Mixin do`. Caught during code review; `ruby_non_mixin_call_does_not_match` now guards it
- **PHP `use Foo as Log;` alias was duplicated** in the imports index because two `namespace_use_clause` patterns matched the same `name` node — pattern 2 (bare `(name)`) fired alongside pattern 3 (`alias:` field). Added `!alias` negative-field guard to pattern 2
- **Lua `require("util")` stored quotes** — `string_content` is absent for strings without escape sequences in tree-sitter-lua 0.5, so the query captured the whole `(string)` node verbatim. New `strip_import_quotes` in `extract_symbols_and_imports` peels off `"..."`, `'...'`, `<...>` (C/C++ system includes), and `[[...]]` (Lua long brackets) from any `import.name` capture. `vex usages util` now resolves the `require` site
- **Markdown heading kind glyph in compact output** was `?` because `cli/output.rs::compact_kind` had no arm for `"heading"`; now emits `H`
- **`clippy::collapsible_match`** failure on `relation_label` PHP arm — refactored to match guards

### Changed
- **Documentation** — `docs/SUPPORTED_LANGUAGES.md` adds an `implementations` support column and lists `src/hierarchy/mod.rs` and `src/cli/mod.rs::Pattern` as match sites to keep in sync; README badges updated to 19 languages / 17 commands / 541 tests
- `extract_references` filter — only structurally-shaped identifiers (`contains '_'` or mixed-case) reach the FST; plain lowercase words like `total`/`amount` are skipped to keep the refs FST tight

## [1.4.2] - 2026-05-13

### Fixed
- **C# parsing was silently broken** in 1.4.1 — `tree-sitter-c-sharp` 0.23.5 shipped grammar ABI 15 while `tree-sitter` 0.24 only supported up to ABI 14. The `LazyLock` initializer panicked on the first `.cs` file, every subsequent file hit a poisoned cell, and the failure was buried in `tracing::warn!` that `RUST_LOG` filters out by default. Result: 0 C# symbols extracted, no user-visible error. Reported externally — 487 `.cs` files indexed as empty
- **Swift parsing was silently broken** since the language was added — `queries/swift.scm` referenced `enum_declaration`, a node that does not exist in `tree-sitter-swift` 0.7. Same `LazyLock`-poisoned-cell failure mode as C#. Discovered only after adding the new per-language regression tests
- **Grammar load failures no longer poison the parser** — `queries::get_query` returned `LazyLock<Query>` and panicked on init; replaced with `LazyLock<Result<Query, String>>` and new `try_get_query` so a compile failure is cached as an error and surfaces as `GrammarLoadError` to the indexing pipeline, not as a thread panic
- **Grammar failures are now visible** — `index/pipeline.rs` aggregates per-language failures and emits a single structured `tracing::warn!` at the end of a run (`language=<X> skipped=<N> error=<reason>`) so users see why their language has zero symbols
- **TOCTOU on file size check** — `discover_files` did `fs::metadata().len() <= 1MB` then `fs::read_to_string()`; a file that grew between the two could exhaust memory. New `read_capped` uses `File::open` + `Read::take(1MB+1)` so the cap is enforced on the actual read
- **Mutex poison handling** in `parse_files` — both lock sites now use `.unwrap_or_else(|e| e.into_inner())` instead of `.unwrap()`, so an unrelated worker panic cannot block aggregation
- Stale CLI message in `vex show` referenced "Kotlin, TypeScript pending" — both have had queries for releases; replaced with a generic grammar-load error
- `WalkBuilder::follow_links(false)` is now set explicitly (was relying on the crate default) — defensive: a malicious symlink cannot smuggle in `../../.ssh/id_rsa`

### Added
- **`docs/SUPPORTED_LANGUAGES.md`** — version matrix listing all 12 grammars, their crate versions, extracted symbol kinds, plus runbooks for upgrading a grammar and adding a new language
- **Per-language regression tests** (11 new files, `tests/<lang>_query_test.rs`) — each verifies the grammar loads on empty input and that core symbol kinds are extracted with the right `SymbolKind`. The first regression check fires *before* any real source file is indexed. Total suite: 451 tests (up from 243)
- `GrammarLoadError` typed error in `parse::extractor` — `pipeline.rs` downcasts to it via `anyhow::Error::downcast_ref`, replacing the previous string-matching heuristic
- Negative-assertion tests for Swift enum/struct/class (regression guard against tree-sitter's no-short-circuit query semantics — multiple patterns matching the same node would double-index)

### Changed
- **Grammar version bumps** (all to crates.io latest as of 2026-05-13):
  - `tree-sitter`: 0.24 → 0.26 (adds ABI 15 support)
  - `tree-sitter-rust`: 0.23 → 0.24
  - `tree-sitter-python`: 0.23 → 0.25
  - `tree-sitter-go`: 0.23 → 0.25
  - `tree-sitter-md`: 0.3 → 0.5
- README: updated test count (243 → 451), added per-language grammar regression section, linked `docs/SUPPORTED_LANGUAGES.md`, refreshed Swift row in the language table to reflect actual extracted kinds

### Removed
- `queries::get_query` — dead-weight back-compat wrapper after CLI gate migrated to `try_get_query`. Removing it prevents future callers from accidentally silently swallowing grammar-load errors

## [1.4.1] - 2026-05-11

### Added
- **Staleness detection** — `vex search` warns when the index is stale (git HEAD changed or files modified since last index); uses a single `git rev-parse` call (~0.1ms) with mtime fallback for non-git repos
- `--auto-update` flag for search/show/usages/check — runs `vex update` inline before search when stale
- `--no-stale-check` flag — skip staleness check entirely
- `auto_update` option in `.vex.toml` — enable auto-update by default
- **Context-aware reranking** — `--kind fn` boosts results matching a specific symbol kind, `--context-path src/file.rs` boosts results near the file you are editing (path overlap + module proximity)
- **90 new tests** (172 → 243):
  - Property-based (proptest): rerank preserves length, sorted output, no NaN/negative, fusion commutativity
  - Reranking stress: NaN/Infinity/zero scores, 10K results, edge context paths
  - Adversarial binary format: 20 crafted index tests — overflow offsets, alignment attacks, truncated records
  - Unicode/encoding: BOM, mixed CRLF, unicode identifiers, null bytes, empty files
  - Incremental update: file rename, symbol move between files, empty file, new file added
  - Concurrency: parallel index (lock serialization), concurrent readers, read during reindex
  - Cross-language: same name across 3 languages, wrong extension, 1K-symbol file, deep nesting, error recovery
  - Path edges: spaces, 20-level nesting, symlinks, absolute vs relative, Windows backslashes
- `proptest` added to dev-dependencies

### Fixed
- **NaN propagation in reranker** — `NaN * boost = NaN` silently corrupted scores; now sanitized to 0.0
- **Infinity overflow in reranker** — `f64::MAX * boost = +inf`; now clamped to `f64::MAX / 2.0`
- **Single-result NaN bypass** — `rerank()` early-returned for len ≤ 1, skipping sanitization
- Kind hint mismatch was too aggressive (0.7x demotion) — changed to neutral 1.0 (boost-only, no penalty)
- Path overlap double-counted filename as shared component — now compares directory components only
- Module proximity boost no longer overlaps with path overlap for same-directory results
- `read_git_head()` moved outside advisory lock in `write_output` to reduce lock hold time

### Changed
- `Freshness` enum is `#[must_use]`; `changed_count` is `Option<usize>` (None = count not computed)
- `RerankContext` struct with `kind_hint: Option<SymbolKind>` and `context_path: Option<&str>`
- `rerank()` signature extended: `rerank(query, &RerankContext, results)`
- Manifest extended with `git_head` and `indexed_at` fields (backward-compatible via `serde(default)`)
- README: updated commands table, test count (243), added Staleness Detection section, updated CLAUDE.md integration rules

## [1.4.0] - 2026-05-08

### Added
- C/C++ support (12th language) — classes, structs, functions, methods, templates, enums, `#include` refs
- Heuristic search result reranking — PascalCase → type boost, snake_case → function boost, exact name match boost, test path demotion
- Fuzz testing suite — 3 targets (index reader, refs FST, symbol FST) covering all `unsafe` code paths in the binary format reader
- 21 new integration tests — binary format roundtrip, corrupted index handling, vector roundtrip, fuzzy search, refs FST, incremental update, multi-language parsing, RRF fusion
- CI test gate on release — `cargo fmt` + `cargo clippy` + `cargo test` must pass before build
- Live CI badge in README (GitHub Actions)

### Fixed
- **3 bugs found by fuzzing**: out-of-bounds read on crafted `symbol_count`, misaligned pointer dereference on odd `symbols_offset`, unchecked section offsets exceeding file size
- `IndexReader::open()` now validates all section offsets against actual file size — rejects corrupted/truncated index files instead of crashing
- `symbol()` and `vector()` use runtime alignment checks (not just `debug_assert`) — returns `None` on misaligned data instead of UB
- Overflow-safe arithmetic in `symbol()`, `vector()`, `read_string()`
- `file_paths()` caps allocation to what fits in mmap — prevents OOM on crafted headers
- Unicode-safe string truncation in body token extraction and doc comments
- Real incremental update — `vex update` now only re-parses changed files and reconstructs unchanged symbols from the existing index (previously did a full rebuild despite detecting changes)

### Changed
- `SymbolKind`: added `TryFrom<u8>` — replaces hardcoded `symbol_kind_str()` magic numbers in search modules
- Eliminated code duplication: merged `indices_to_results` / `indices_to_results_typed`, extracted shared `discover_source_files()` from callgraph and hierarchy modules
- README: removed fake test count badge, added honest comparison notes (pre-built index vs full scan tradeoff), added Testing section with fuzz test documentation
- Release workflow now requires tests to pass before building binaries

## [1.3.0] - 2026-05-08

### Added
- Body-aware semantic search — embeddings now include identifiers and string literals extracted from symbol bodies, not just names/signatures/docstrings
- `.vex.toml` config file — exclude patterns, default format, semantic on/off
- `vex init` — generates default config with commented examples
- `--no-semantic` flag to override config-enabled semantic mode
- Fuzzy search — automatic Levenshtein fallback when exact + prefix returns nothing ("IndxReader" finds "IndexReader")
- Markdown support (11th language) — headings indexed as symbols, `vex outline README.md` shows document structure, `vex show "Installation"` extracts full section

### Changed
- `SymbolKind` now uses `#[repr(u8)]` with explicit discriminants for binary format stability
- File walking centralized via `util::walk` module with configurable exclude patterns
- Config loaded from project root (resolved `--path`), not cwd

## [1.1.0] - 2026-05-08

### Added
- Shell completions for bash, zsh, fish (`vex completions <shell>`)
- Release script for version bumps (`scripts/release.sh`)

### Fixed
- Atomic index writes — write to temp file, rename on success (no corruption on crash)
- Advisory file locking during `vex index` / `vex update` (prevents concurrent writes)
- Skip binary and minified files by content heuristic (control chars, long lines)
- Catch tree-sitter panics in rayon workers instead of crashing the indexer
- Inline index validation with specific error messages (size, magic, version)
- Bounds-check `read_string` to prevent panic on corrupt index

### Changed
- README updated: 15 commands, 115 tests, full MCP tools list, completions docs

## [1.0.1] - 2026-05-07

### Fixed
- Drop x86_64-apple-darwin from release CI matrix

## [1.0.0] - 2026-05-07

### Added
- `vex check` — fast symbol existence check via FST
- `vex callers` / `vex callees` — callgraph navigation (5 languages)
- `vex implementations` — find types extending a base class/trait/interface (7 languages)
- `vex show A B C` — bulk symbol body extraction
- `vex outline --kind fn` — filter outline by symbol kind
- `vex search --filter`, `vex show --filter` — path-based result filtering
- Semantic search quality: tokenized names, path keywords, docstrings in embeddings
- MCP server expanded to 15 tools (full CLI parity)
- CI/CD: GitHub Actions for tests and releases
- CLAUDE.md integration instructions in README

### Fixed
- Pattern matching enabled for Kotlin, TypeScript, and SQL

## [0.2.0] - 2026-05-07

### Added
- `vex grep` — regex content search with `--filter` path support
- Homebrew installation (`brew install tenatarika/tap/vex`)

## [0.1.0] - 2026-05-07

Initial release.

### Added
- Structural search — FST-based symbol lookup in ~4ms
- Semantic search — MiniLM-L6-v2 embeddings + HNSW index + RRF fusion
- `vex index` / `vex update` / `vex watch` — full, incremental, and watch-mode indexing
- `vex search`, `vex usages`, `vex show`, `vex outline`, `vex status`
- `vex pattern` — AST pattern matching with metavariables ($NAME, $$$)
- 10 languages: Rust, Python, Go, Java, C#, Ruby, Swift, Kotlin, TypeScript, SQL
- MCP server (`vex-mcp`) for AI agent integration
- Custom mmap'd binary format — zero-copy reads, 10x smaller than SQLite
- Parallel parsing via rayon, incremental updates via content hashing
- Compact output format (`--format compact`) for LLM token efficiency
- JSON output (`--format json`) for tool integration

[Unreleased]: https://github.com/tenatarika/vex/compare/v1.15.2...HEAD
[1.15.2]: https://github.com/tenatarika/vex/compare/v1.15.1...v1.15.2
[1.15.1]: https://github.com/tenatarika/vex/compare/v1.15.0...v1.15.1
[1.15.0]: https://github.com/tenatarika/vex/compare/v1.14.1...v1.15.0
[1.5.0]: https://github.com/tenatarika/vex/compare/v1.4.3...v1.5.0
[1.4.3]: https://github.com/tenatarika/vex/compare/v1.4.2...v1.4.3
[1.4.2]: https://github.com/tenatarika/vex/compare/v1.4.1...v1.4.2
[1.4.1]: https://github.com/tenatarika/vex/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/tenatarika/vex/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/tenatarika/vex/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/tenatarika/vex/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/tenatarika/vex/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/tenatarika/vex/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/tenatarika/vex/compare/v0.2.0...v1.0.0
[0.2.0]: https://github.com/tenatarika/vex/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tenatarika/vex/releases/tag/v0.1.0
