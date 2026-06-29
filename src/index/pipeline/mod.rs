use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};

use crate::index::manifest::{self, Manifest};
use crate::util::config;

mod lock;
mod output;
mod parse_files;

use lock::IndexLock;
use output::{
    build_hnsw, build_hnsw_incremental, compute_hashes_for, generate_embeddings, prune_embed_cache,
    vector_dim_for, write_output_locked,
};

// v1.15.0 B1.2 — bench / test / fuzz reach into the real `build_hnsw_at`
// and `build_hnsw_incremental_at` via this re-export so they exercise
// the exact code path production does (mirrors the v1.12.0
// `__fuzz_*_bytes` doc-hidden export convention). Keeping the symbols
// `#[doc(hidden)]` keeps them out of rustdoc and out of the user-facing
// API contract; SemVer treats them as private.
//
// `#[allow(unused_imports)]` is load-bearing: rustc's dead-code
// analysis for the lib target cannot see the bench / integration test
// / fuzz crate's consumption of these names, so without the allow it
// flags the `pub use` as unused. The alternative — making `output` a
// `pub mod` — would leak every other `pub(super)` item in that
// module (`generate_embeddings`, `compute_hashes_for`, etc.) into the
// crate's public surface, breaking the visibility contract for the
// whole pipeline. The `#[allow]` here is the narrowest escape valve.
#[doc(hidden)]
#[allow(unused_imports)]
pub use output::{__fuzz_incremental_hnsw_bytes, build_hnsw_at, build_hnsw_incremental_at};
use parse_files::{
    build_blob_cache, discover_files, hash_files, parse_files, reconstruct_unchanged,
};

const CHUNK_SIZE: usize = 500;

/// Build-time toggles for [`run`] and [`update`].
///
/// `with_embeddings` is a transient run-time choice (the user decides per
/// invocation). `with_call_graph`, `with_bm25`, and `with_pattern_index`
/// are persisted into the manifest after a successful build so `update`
/// can keep the opt-out sticky across incremental rebuilds.
#[derive(Debug, Clone, Copy)]
pub struct IndexOptions {
    pub with_embeddings: bool,
    pub with_call_graph: bool,
    pub with_bm25: bool,
    /// Build the v6 pattern-skeleton side-section. When `false`, the
    /// section is written empty and `vex pattern` keeps using its
    /// live-scan path (today's behaviour). Default `true`. 11.4 Inc 4.
    pub with_pattern_index: bool,
    /// Phase 14.8 — build the `git_history` sidecar
    /// (`<index_dir>/index.git_history`) carrying every historical
    /// symbol reachable from `HEAD`. Default `false`: opt-in only.
    /// Triggered by `vex index --history` / `vex update --history`.
    pub with_history: bool,
    /// Phase 14.8 — cap the history walk at N newest commits (global,
    /// not per-file). `None` = unbounded.
    pub history_depth: Option<usize>,
    /// Phase 14.8 — drop the `git_history` sidecar + null out the
    /// manifest's `history_*` fields. Triggered by
    /// `vex update --no-history`. Mutually exclusive with
    /// `with_history`; clap enforces this at the CLI boundary
    /// (`conflicts_with = "history"`).
    pub drop_history: bool,
    /// v1.15.1 MEDIUM — opt-in destructive teardown of the semantic
    /// channel. When `true`, `run` deletes `index.hnsw`,
    /// `index.hashes`, and the embedder's `embed_cache_*.bin` even
    /// after a `--no-semantic` build. Default `false`: a
    /// `--no-semantic` rebuild now PRESERVES prior HNSW + sidecar +
    /// cache so a future `--semantic` build can reuse them.
    ///
    /// Pre-fix, `--no-semantic` unconditionally removed the HNSW +
    /// sidecar and orphaned the embed cache. Re-attaching required a
    /// fresh `--semantic` rebuild that re-ran every embedding from
    /// scratch (~minutes per 10k symbols). The field-test report at
    /// `.claude/Task/v1.15.1-amics-field-test-fixes.md` flagged this
    /// as "easy way to lose semantic search permanently without
    /// realizing", especially while the critical HNSW bug stood.
    pub drop_semantic: bool,

    /// Compute device for embedding generation (resolved from CLI/config/env
    /// via [`crate::embed::Device::resolve`]). Runtime-only — never persisted
    /// to the manifest, since vectors are model-defined, not device-defined.
    /// See `docs/GPU_SUPPORT.md`.
    pub device: crate::embed::Device,
    /// True only when GPU was selected by an EXPLICIT CLI `--gpu` / `--device`.
    /// Bypasses the miss-count gate (`docs/GPU_SUPPORT.md` §3.4). NOT set for
    /// `.vex.toml gpu = true` or `VEX_DEVICE` — config/env `Auto` stays gated
    /// so a tiny `vex update` still avoids GPU warm-up.
    pub gpu_explicit: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            with_embeddings: false,
            with_call_graph: true,
            with_bm25: true,
            with_pattern_index: true,
            with_history: false,
            history_depth: None,
            drop_history: false,
            drop_semantic: false,
            // Neutral baseline; the index/update paths override from
            // CLI/config/env. `with_embeddings: false` above makes device moot
            // for the default value.
            device: crate::embed::Device::Cpu,
            gpu_explicit: false,
        }
    }
}

// ─── manifest skip-path predicates ──────────────────────────────────────────
//
// Decide whether a manifest from a peer's rebuild covers what *this*
// caller is asking for — i.e. whether the on-disk index is already what
// we would have produced. Used by `run_with_lock` and `update_inner` to
// short-circuit thundering-herd rebuilds. Skip-path policy belongs with
// the orchestrator rather than the output builders.

/// True when `manifest` was built with options that at least cover what `opts`
/// asks for — i.e. skipping a rebuild and reusing this index would not silently
/// downgrade the caller's request. Only `with_embeddings` / `embedder_id` are
/// checked here because they are *transient* (the user re-decides per
/// invocation). The boolean opt-outs (`call_graph`, `bm25`, `pattern_index`)
/// are *sticky* per their Manifest doc comments: the rebuild would preserve
/// the existing value rather than honor the caller's `opts.with_*=true`, so
/// blocking the skip for them would still produce the same on-disk result.
fn manifest_options_cover(manifest: &Manifest, opts: IndexOptions, embedder_id: &str) -> bool {
    if opts.with_embeddings {
        match manifest.embedder_id.as_deref() {
            Some(id) if id == embedder_id => {}
            _ => return false,
        }
    }
    // Phase 14.8 Step 5b: history coverage gates the skip path so a
    // `vex update --no-history` actually drops the sidecar instead of
    // short-circuiting before write_output_locked even runs.
    if opts.drop_history && manifest.state.history_indexed_at.is_some() {
        // We owe a drop — section still on disk. Don't skip.
        return false;
    }
    if opts.with_history && manifest.state.history_indexed_at.is_none() {
        // User wants history but the prior index has no section. Don't
        // skip — write_output_locked needs to build it.
        return false;
    }
    if opts.with_history && opts.history_depth != manifest.state.history_depth {
        // Explicit cap changed (e.g. `--history-depth 50` after a prior
        // unbounded build). The fast-path inside write_output_locked
        // would also reject this; rejecting here avoids the skip path
        // skipping the rebuild entirely.
        return false;
    }
    true
}

/// `run`-specific skip gate. Calls [`manifest_options_cover`] and additionally
/// rejects manifests that were written by `vex update` (incremental,
/// `pattern_index_full == Some(false)`) when the caller explicitly ran
/// `vex index` and opted into the pattern index — the partial pattern section
/// is harmless but the user asked for the full one, so the rebuild is owed.
fn run_can_skip(manifest: &Manifest, opts: IndexOptions, embedder_id: &str) -> bool {
    if !manifest_options_cover(manifest, opts, embedder_id) {
        return false;
    }
    if opts.with_pattern_index && manifest.pattern_index_full == Some(false) {
        return false;
    }
    true
}

/// `update`-specific skip gate. Only the option-coverage check matters —
/// `update` never produces `pattern_index_full == true`, so reusing a peer's
/// partial pattern index is identical to what `update` itself would have
/// emitted.
fn update_can_skip(manifest: &Manifest, opts: IndexOptions, embedder_id: &str) -> bool {
    manifest_options_cover(manifest, opts, embedder_id)
}

/// Full rebuild: index all files from scratch.
///
/// Returns `(symbol_count, rebuilt)`. `rebuilt` is `false` when the manifest
/// re-check under the build lock proved a peer just produced an equivalent
/// index — the caller sees the same symbol count it would have produced and
/// can distinguish a real rebuild from a thundering-herd skip for telemetry /
/// test purposes. Matches `pipeline::update`'s tuple-return shape.
pub fn run(
    root: &Path,
    opts: IndexOptions,
    embedder_id: &str,
    excludes: &[String],
) -> Result<(usize, bool)> {
    let (root, files, file_hashes) = run_setup(root, excludes)?;
    // Blocking acquire — original behaviour. Concurrent vex index calls
    // serialize here; the manifest re-check inside `run_with_lock` lets
    // peers that observe an equivalent build skip the redundant rebuild.
    let lock = IndexLock::acquire(&root)?;
    run_with_lock(&root, opts, embedder_id, files, file_hashes, lock)
}

/// v1.12.0 — non-blocking sibling of [`run`]. Returns `Ok(None)` when another
/// vex instance is currently holding the build lock, leaving the caller free
/// to no-op (CI cron jobs, editor integrations that don't want to wedge for
/// a peer's parse + embed). Otherwise returns `Ok(Some(_))` with the same
/// shape as [`run`].
pub fn run_or_busy(
    root: &Path,
    opts: IndexOptions,
    embedder_id: &str,
    excludes: &[String],
) -> Result<Option<(usize, bool)>> {
    let (root, files, file_hashes) = run_setup(root, excludes)?;
    let Some(lock) = IndexLock::try_acquire(&root)? else {
        tracing::info!("index lock held by another vex instance; --no-wait skipping rebuild");
        return Ok(None);
    };
    run_with_lock(&root, opts, embedder_id, files, file_hashes, lock).map(Some)
}

/// Pre-lock fixture: the canonical root, the discovered file set, and the
/// (rel-path, content-hash) tuples computed during the walk. Returned by
/// [`run_setup`] and consumed by the lock-acquiring entry points so both
/// the blocking and `--no-wait` variants do the same cheap pre-flight.
type RunSetup = (
    std::path::PathBuf,
    Vec<std::path::PathBuf>,
    Vec<(String, u64)>,
);

/// Pre-lock work shared by [`run`] and [`run_or_busy`]: canonicalize the
/// root, walk the file tree honoring excludes, and hash every file. Cheap
/// enough to be re-done by the `--no-wait` caller without violating its
/// "don't wait on a peer" contract.
fn run_setup(root: &Path, excludes: &[String]) -> Result<RunSetup> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files = discover_files(&root, excludes)?;
    tracing::info!(count = files.len(), "discovered files");
    let file_hashes = hash_files(&root, &files);
    Ok((root, files, file_hashes))
}

/// Heavy section of `vex index`: manifest re-check skip path, then parse +
/// embed + write + HNSW build. The lock is taken as an owned guard so it
/// stays held for the duration of every code path (including the early
/// skip return); only when the function returns is the build lock released.
fn run_with_lock(
    root: &Path,
    opts: IndexOptions,
    embedder_id: &str,
    files: Vec<std::path::PathBuf>,
    file_hashes: Vec<(String, u64)>,
    _lock: IndexLock,
) -> Result<(usize, bool)> {
    // Double-check under the lock: if a peer just completed a rebuild with an
    // identical file fingerprint AND the index file is actually on disk, skip
    // the redundant work. `vex index` is deterministic from its inputs (same
    // files + same options → same output), so a matching manifest means the
    // on-disk index is already what we'd produce. The `index_path.exists()`
    // gate matters for the cold-start case (empty directory or first-ever
    // index): a missing manifest loads as `Manifest::default()` with an empty
    // file map, which would otherwise diff equal to an empty file list and
    // skip the initial write.
    let manifest_path = config::manifest_path(root);
    let index_path = config::index_path(root);
    if index_path.exists() {
        if let Ok(current_manifest) = Manifest::load(&manifest_path) {
            let diff = manifest::diff_files(&file_hashes, &current_manifest);
            if diff.changed.is_empty()
                && diff.deleted.is_empty()
                && run_can_skip(&current_manifest, opts, embedder_id)
            {
                tracing::info!(
                    "index already built by a concurrent vex instance; skipping rebuild"
                );
                return Ok((existing_symbol_count(root)?, false));
            }
        }
    }

    // Phase 14.7 — blob-SHA parse cache. Construct once per index run and
    // share across the parse loop. The cache is best-effort; failures only
    // cost a re-parse, never correctness.
    let cache = build_blob_cache();
    let blob_map = crate::index::parse_cache::git_blobs::discover_tracked_blobs(root);

    let all_parsed = parse_files(root, &files, &blob_map, &cache)?;
    let symbol_count: usize = all_parsed.iter().map(|f| f.symbols.len()).sum();

    let (mut vectors, hashes) = if opts.with_embeddings && symbol_count > 0 {
        generate_embeddings(
            &all_parsed,
            embedder_id,
            root,
            opts.device,
            opts.gpu_explicit,
        )?
    } else {
        (Vec::new(), Vec::new())
    };
    // v1.13 P5: L2-normalize at write time. Brute-force similarity
    // (`search_brute_force`, `find_similar`/`find_duplicates`) then
    // collapses to a dot product — skipping the per-call sqrt + norms.
    // HNSW already uses cosine internally; unit-length input is
    // equivalent. Manifest's `vectors_normalized: Some(true)` is the
    // gate the readers consult.
    for v in vectors.iter_mut() {
        crate::search::semantic::normalize_in_place(v);
    }

    let manifest_embedder = if opts.with_embeddings && !vectors.is_empty() {
        Some(embedder_id.to_string())
    } else {
        None
    };
    let vector_dim = vector_dim_for(embedder_id, &vectors);
    write_output_locked(
        root,
        &all_parsed,
        &vectors,
        vector_dim,
        &file_hashes,
        manifest_embedder,
        opts,
        true, // is_full_rebuild — `vex index` always replaces everything
        &crate::index::types::IndexBuildArtefacts::default(),
    )?;

    if !vectors.is_empty() {
        build_hnsw(root, &vectors, &hashes)?;
        // E3 sweep — full-rebuild path: `hashes` from generate_embeddings
        // is already the full live set, no separate compute_hashes_for
        // needed. Reclaims entries for symbols deleted/renamed since
        // the previous build.
        let dim = vector_dim_for(embedder_id, &vectors);
        let _ = prune_embed_cache(root, embedder_id, dim, &hashes);
    } else if opts.drop_semantic {
        // v1.15.1 MEDIUM: opt-in destructive teardown. Only when the
        // caller explicitly passed `--drop-semantic` do we wipe the
        // HNSW + sidecar + embed cache. Pre-fix this branch ran on
        // every `--no-semantic` invocation and silently orphaned a
        // 200+ MB embed cache, forcing a full re-embed on the next
        // `--semantic` rebuild.
        //
        // Without `--drop-semantic`, a `--no-semantic` rebuild leaves
        // the prior HNSW + sidecar in place. They become stale
        // relative to the new symbol set — but the query path at
        // `src/search/semantic.rs:156` catches the size mismatch
        // (`hashes.len() != expected_symbols`) and bails to brute-
        // force semantic search. No wrong results, just slower
        // until the next `--semantic` build.
        let hnsw_path = config::hnsw_path(root);
        if hnsw_path.exists() {
            std::fs::remove_file(&hnsw_path).context("remove HNSW index")?;
        }
        let hash_index_path = config::hash_index_path(root);
        if hash_index_path.exists() {
            std::fs::remove_file(&hash_index_path).context("remove HNSW hash-index sidecar")?;
        }
        let embed_cache_path = config::embed_cache_path(root, embedder_id);
        if embed_cache_path.exists() {
            std::fs::remove_file(&embed_cache_path).context("remove embed cache")?;
        }
        tracing::info!(
            embedder = embedder_id,
            "semantic channel dropped (--drop-semantic): HNSW + sidecar + embed cache removed"
        );
    }

    tracing::info!(
        symbols = symbol_count,
        vectors = vectors.len(),
        "indexing complete"
    );
    Ok((symbol_count, true))
}

/// Symbol count of the existing on-disk index, or 0 if there is none. Shared by
/// `update`'s early-return paths that skip a rebuild.
fn existing_symbol_count(root: &Path) -> Result<usize> {
    let index_path = config::index_path(root);
    if index_path.exists() {
        Ok(crate::store::reader::IndexReader::open(&index_path)
            .context("open existing index for symbol count")?
            .symbol_count())
    } else {
        Ok(0)
    }
}

/// Incremental update: detect changed files, re-parse only those, merge with unchanged
/// symbols from the existing index. Returns (total_symbols, changed_count, deleted_count).
pub fn update(
    root: &Path,
    opts: IndexOptions,
    embedder_id: &str,
    excludes: &[String],
) -> Result<(usize, usize, usize)> {
    // The blocking variant cannot observe `Ok(None)` — that's only emitted
    // by `update_inner`'s `no_wait` branch, and the `no_wait = false` arm
    // routes through `IndexLock::acquire` which always returns `Self`.
    // Use `unreachable!` so the impossibility is documented as an
    // invariant rather than a runtime assertion.
    match update_inner(
        root,
        opts,
        embedder_id,
        excludes,
        /* no_wait = */ false,
    )? {
        Some(outcome) => Ok(outcome),
        None => unreachable!("update_inner(no_wait = false) never returns Ok(None)"),
    }
}

/// v1.12.0 — non-blocking sibling of [`update`]. Returns `Ok(None)` when
/// another vex instance is currently holding the build lock and the diff is
/// non-empty, so a `--no-wait` caller can no-op instead of wedging on a
/// peer's parse + embed cycle. The cheap "nothing to update" case is *not*
/// gated by the lock — if the working tree's hashes already match the
/// manifest there is no work to dedupe, so we return the `(count, 0, 0)`
/// outcome immediately like the blocking variant.
pub fn update_or_busy(
    root: &Path,
    opts: IndexOptions,
    embedder_id: &str,
    excludes: &[String],
) -> Result<Option<(usize, usize, usize)>> {
    update_inner(root, opts, embedder_id, excludes, /* no_wait = */ true)
}

fn update_inner(
    root: &Path,
    opts: IndexOptions,
    embedder_id: &str,
    excludes: &[String],
    no_wait: bool,
) -> Result<Option<(usize, usize, usize)>> {
    let root = root.canonicalize().context("canonicalize root")?;
    let manifest_path = config::manifest_path(&root);
    let old_manifest = Manifest::load(&manifest_path)?;

    let files = discover_files(&root, excludes)?;
    let file_hashes = hash_files(&root, &files);

    // Phase 14.7 — same blob cache as `run`. `vex update` benefits when
    // the user reverts changes or jumps between branches whose blobs were
    // already parsed in earlier sessions.
    let cache = build_blob_cache();
    let blob_map = crate::index::parse_cache::git_blobs::discover_tracked_blobs(&root);

    let diff = manifest::diff_files(&file_hashes, &old_manifest);

    // v1.12.0: skip only if the on-disk index satisfies every option we were
    // asked for. Before this gate `vex update --semantic` on a no-change
    // structural-only index would early-return and silently leave the
    // embedder request unfulfilled.
    if diff.changed.is_empty()
        && diff.deleted.is_empty()
        && update_can_skip(&old_manifest, opts, embedder_id)
    {
        tracing::info!(unchanged = diff.unchanged, "nothing to update");
        return Ok(Some((existing_symbol_count(&root)?, 0, 0)));
    }

    // Serialize concurrent rebuilds: take the build lock BEFORE the expensive
    // parse + embed below. Until this guard existed the lock only wrapped the
    // final write, so N vex instances auto-updating the
    // same stale index each loaded the embedding model and re-embedded in
    // parallel — a thundering-herd rebuild that pegs CPU and RAM under
    // multi-agent fan-out. Holding the lock here means exactly one instance
    // does the work; the rest wait and then skip via the re-check below.
    // v1.12.0 --no-wait: when the caller cannot block, bail out as `None`
    // here instead of queueing on the lock.
    let _lock = if no_wait {
        let Some(l) = IndexLock::try_acquire(&root)? else {
            tracing::info!("index lock held by another vex instance; --no-wait skipping update");
            return Ok(None);
        };
        l
    } else {
        IndexLock::acquire(&root)?
    };

    // Double-check under the lock: another instance may have refreshed the
    // index while we waited. Re-diff against the now-current manifest AND
    // re-check option coverage — a peer that built without our options'd
    // otherwise serve us a downgraded index.
    let (diff, current_manifest) = {
        let current_manifest = Manifest::load(&manifest_path)?;
        let diff = manifest::diff_files(&file_hashes, &current_manifest);
        (diff, current_manifest)
    };
    if diff.changed.is_empty()
        && diff.deleted.is_empty()
        && update_can_skip(&current_manifest, opts, embedder_id)
    {
        tracing::info!("index refreshed by a concurrent vex instance; skipping rebuild");
        return Ok(Some((existing_symbol_count(&root)?, 0, 0)));
    }

    tracing::info!(
        changed = diff.changed.len(),
        deleted = diff.deleted.len(),
        unchanged = diff.unchanged,
        "incremental update"
    );

    let mut changed_set: HashSet<&str> = diff.changed.iter().map(|s| s.as_str()).collect();
    let deleted_set: HashSet<&str> = diff.deleted.iter().map(|s| s.as_str()).collect();

    // === Cascade-then-reconstruct ordering invariant ===========================
    //
    // The three blocks immediately below — (1) Q4-B cascade discovery, (2)
    // merge cascade entries into `changed_set`, (3) Q4-A reconstruction
    // of unchanged files — MUST execute in that order. The whole point
    // of cascade is to demote importers from "reconstruct from stale
    // index" to "re-parse from source" so their `bound_refs` get rebound
    // against the new name table. If reconstruction reads `changed_set`
    // before cascade merges into it, every cascade importer will be
    // re-parsed AND reconstructed — the latter wins the merge at line
    // ~700, silently re-introducing the exact Q4-A regression Q4-B was
    // built to fix (architect audit A3, fuzz session 2026-06-17).
    //
    // Invariant: `cascade_paths ⊆ changed_set` AND `cascade_paths ∩
    // reconstructed_files == ∅`. Enforced by `debug_assert!` after each
    // step. Do NOT reorder these blocks without re-running the
    // `incremental_consistency_test::cascade_*` suite.
    //
    // Phase 11.1.10 (Q4-B): cascade-invalidate importers of
    // changed/deleted files. The Q4-A reconstruction path correctly
    // preserves refs for unchanged files, but when a changed file
    // renames/deletes an exported symbol, reconstructed refs from
    // unchanged importers silently drop (LIMITATIONS §4d for Q4-A).
    // Cascade fixes this by re-parsing importers — their bound_refs
    // get produced fresh against the new name table.
    //
    // Phase 11.1.11 (Q4-C): cascade now follows the `imported_by`
    // reverse graph TRANSITIVELY via BFS, bounded by `CASCADE_MAX_DEPTH`.
    // Q4-B's single hop missed transitive re-export chains (A imports
    // through B which re-exports from C — editing C left A's refs
    // stale). The visited-set + frontier-collapse pattern handles
    // cycles (A↔B) and stars naturally; depth-1 is now a special case
    // of the depth-N walk with N=1.
    //
    // Reuses `current_manifest` (post-lock fresh load at line 499)
    // instead of a third re-read: avoids a redundant disk I/O on every
    // `vex update` (watch-mode-hot-path concern) AND avoids reading a
    // pre-lock-stale `imported_by` when a concurrent peer wrote
    // between lock acquisition and the cascade build (third-pass
    // review HIGH).
    //
    // Depth cap rationale (16): idiomatic Rust/Go (direct `use foo::Bar`
    // imports) bottoms out at depth 1; Python/TypeScript re-export
    // façades typically need 2–4 hops. 16 covers every practical
    // re-export chain seen in the wild and keeps the worst-case BFS
    // bounded against pathologically deep stars. Saturating the cap
    // emits a `tracing::warn!` so the user knows refs at depth > 16
    // may still need a `vex index`.
    const CASCADE_MAX_DEPTH: usize = 16;
    let mut cascade_max_depth_hit: usize = 0;
    let mut cascade_saturated = false;
    let cascade_paths: Vec<String> = {
        let mut out: Vec<String> = Vec::new();
        if !current_manifest.state.imported_by.is_empty() {
            let mut seen: HashSet<String> = HashSet::new();
            // Frontier = files at the current BFS depth whose importers
            // we haven't yet explored. Initial frontier is the changed
            // + deleted set (depth 0); each loop iteration expands to
            // the next depth's frontier.
            let mut frontier: Vec<String> = changed_set
                .iter()
                .chain(deleted_set.iter())
                .map(|s| (*s).to_string())
                .collect();
            for depth in 1..=CASCADE_MAX_DEPTH {
                let mut next: Vec<String> = Vec::new();
                for trigger in &frontier {
                    let Some(importers) = current_manifest.state.imported_by.get(trigger) else {
                        continue;
                    };
                    for importer in importers {
                        if changed_set.contains(importer.as_str())
                            || deleted_set.contains(importer.as_str())
                        {
                            continue; // already being re-parsed
                        }
                        if seen.insert(importer.clone()) {
                            out.push(importer.clone());
                            next.push(importer.clone());
                            cascade_max_depth_hit = depth;
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                if depth == CASCADE_MAX_DEPTH && !next.is_empty() {
                    cascade_saturated = true;
                }
                frontier = next;
            }
        } else if current_manifest.state.imported_by_built.is_none()
            && (!changed_set.is_empty() || !deleted_set.is_empty())
        {
            // Bootstrap signal: only fire when the field is *absent*
            // (pre-11.1.10 manifest), NOT when the writer ran and the
            // map happens to be empty. Without this gate, every update
            // on a binder-less project (Go-only, fresh repo, no
            // cross-file refs) would spam this info! line — false
            // positive flagged by third-pass review M1.
            tracing::info!(
                "vex update: imported_by reverse map absent in manifest (pre-11.1.10 index); \
                 cascade skipped this turn — next update will be fully incremental \
                 (any refs to renamed/deleted symbols may need a full `vex index`)"
            );
        }
        out
    };
    // Path-normalization sanity: every cascade entry should already be
    // POSIX-relative because it came from `manifest.imported_by`, which
    // the writer populated from `file_paths_new`, which came from
    // `file_ids` keyed by `ParsedFile.path` (POSIX-normalized at parse
    // time via `to_rel_posix` — memory `reference_windows_path_normalize`).
    debug_assert!(
        cascade_paths.iter().all(|p| !p.contains('\\')),
        "cascade_paths must be POSIX-normalized, got: {:?}",
        cascade_paths
            .iter()
            .find(|p| p.contains('\\'))
            .map(String::as_str)
    );
    for path in &cascade_paths {
        changed_set.insert(path.as_str());
    }
    // Step (2) of the ordering invariant: every cascade importer is now
    // a member of `changed_set`. If this ever fires, the merge loop above
    // was edited to skip entries — `reconstruct_unchanged` would then
    // re-import stale refs from those importers (silent Q4-A regression).
    debug_assert!(
        cascade_paths
            .iter()
            .all(|p| changed_set.contains(p.as_str())),
        "cascade merge broken: cascade_paths contains entry absent from changed_set"
    );
    if !cascade_paths.is_empty() {
        tracing::info!(
            cascade_count = cascade_paths.len(),
            cascade_max_depth = cascade_max_depth_hit,
            "vex update cascade: re-parsing {} importer(s) of changed/deleted files at depths 1..={} (Phase 11.1.11 / Q4-C)",
            cascade_paths.len(),
            cascade_max_depth_hit,
        );
    }
    if cascade_saturated {
        tracing::warn!(
            cascade_max_depth = CASCADE_MAX_DEPTH,
            "vex update cascade hit CASCADE_MAX_DEPTH={CASCADE_MAX_DEPTH}; transitive importers deeper than this were NOT re-parsed. \
             Refs targeting renamed/deleted symbols via re-export chains longer than {CASCADE_MAX_DEPTH} hops may need a full `vex index`."
        );
    }

    // Reconstruct unchanged symbols (+ vectors) from existing index.
    // v1.15.0 B1.2 — also load the body_tokens sidecar (best-effort).
    // `None` means pre-v1.15 index OR sidecar load failed — the
    // reconstructed symbols carry `body_tokens: None` and the HNSW
    // incremental path falls back to full rebuild on this update.
    // `Some(vec)` means the sidecar loaded successfully (zero-length
    // is a legitimate empty-index state, NOT failure — keeping these
    // distinct prevents the BM25-regression warning from misfiring).
    let index_path = config::index_path(&root);
    let body_tokens_sidecar: Option<Vec<Option<String>>> = {
        let path = config::body_tokens_path(&root);
        if path.exists() {
            match crate::store::body_tokens::load(&path) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "body_tokens sidecar load failed; reconstructed symbols will \
                         fall back to body_tokens: None and HNSW update will be full rebuild"
                    );
                    None
                }
            }
        } else {
            None
        }
    };
    let (unchanged_parsed, unchanged_vectors, artefacts) = if index_path.exists() {
        let reader = crate::store::reader::IndexReader::open(&index_path)
            .context("open existing index for incremental merge")?;
        let recon = reconstruct_unchanged(
            &reader,
            &changed_set,
            &deleted_set,
            body_tokens_sidecar.as_deref(),
        );
        // Step (3) of the ordering invariant: no cascade importer
        // was reconstructed. `reconstruct_unchanged` skips anything
        // in `changed_set`, so this should hold transitively — but
        // pin it explicitly so a future refactor that swaps the
        // skip-list breaks the test instead of the user's refs.
        debug_assert!(
            {
                let cascade_set: HashSet<&str> = cascade_paths.iter().map(String::as_str).collect();
                recon
                    .parsed_files
                    .iter()
                    .all(|f| !cascade_set.contains(f.path.as_str()))
            },
            "cascade ∩ reconstructed must be empty — Q4-B cascade must run \
                 before Q4-A reconstruction (see ordering invariant above)"
        );
        (
            recon.parsed_files,
            recon.vectors,
            crate::index::types::IndexBuildArtefacts {
                reconstructed_refs: recon.reconstructed_refs,
                old_file_paths: recon.old_file_paths,
                reconstructed_unresolved_refs: recon.reconstructed_unresolved_refs,
            },
        )
    } else {
        (
            Vec::new(),
            Vec::new(),
            crate::index::types::IndexBuildArtefacts::default(),
        )
    };
    // v1.13 P5: existing index's `vectors_normalized` flag drives the
    // partial-normalize decision below. Reuses `current_manifest`
    // (post-lock fresh load at line 499) — same source the Q4-B
    // cascade now reads.
    let existing_normalized = current_manifest.vectors_normalized.unwrap_or(false);

    let unchanged_sym_count: usize = unchanged_parsed.iter().map(|f| f.symbols.len()).sum();
    tracing::info!(
        unchanged_symbols = unchanged_sym_count,
        unchanged_vectors = unchanged_vectors.len(),
        "reconstructed unchanged from index"
    );

    // Parse only changed/new files
    let changed_paths: Vec<std::path::PathBuf> = files
        .iter()
        .filter(|p| {
            // Use the POSIX-normalized rel so the lookup matches what
            // `hash_files` inserted into `changed_set` — without normalization
            // Windows backslashes leak into the filter key and the set lookup
            // silently under-matches.
            crate::util::paths::to_rel_posix(p, &root)
                .is_some_and(|r| changed_set.contains(r.as_str()))
        })
        .cloned()
        .collect();

    let newly_parsed = parse_files(&root, &changed_paths, &blob_map, &cache)?;
    let new_sym_count: usize = newly_parsed.iter().map(|f| f.symbols.len()).sum();

    // Generate embeddings for symbols in changed files. The E2b
    // embedding cache (`<index_dir>/embed_cache_<embedder_id>.bin`)
    // dedups by content-hash inside `generate_embeddings` — symbols
    // whose context_string is byte-identical to a previous run reuse
    // the stored vector and skip the embed step. When 100% of contexts
    // hit the cache, the ONNX model is never loaded.
    // v1.14.1 B1.1 — `generate_embeddings` now returns `(vectors,
    // hashes)`. We discard the changed-file hashes here and recompute
    // hashes for the full merged set below via `compute_hashes_for`;
    // that keeps the HNSW key space consistent across the changed +
    // reconstructed slices (reconstructed symbols come with
    // `body_tokens: None`, which produces a different hash than the
    // body-aware one `generate_embeddings` would emit for the same
    // symbol if it were freshly parsed).
    let (new_vectors, _new_hashes) = if opts.with_embeddings && new_sym_count > 0 {
        generate_embeddings(
            &newly_parsed,
            embedder_id,
            &root,
            opts.device,
            opts.gpu_explicit,
        )?
    } else {
        (Vec::new(), Vec::new())
    };

    // Merge: unchanged first (vectors align with symbol order)
    let mut all_parsed = unchanged_parsed;
    all_parsed.extend(newly_parsed);
    let symbol_count: usize = all_parsed.iter().map(|f| f.symbols.len()).sum();

    let unchanged_count = unchanged_vectors.len();
    let mut all_vectors = unchanged_vectors;
    all_vectors.extend(new_vectors);
    // v1.13 P5: normalize the merged set so the result is always
    // L2-normalized regardless of legacy state. Key subtlety: when the
    // existing index is ALREADY normalized (v1.13+), re-normalizing
    // unit vectors is a no-op mathematically but accumulates
    // floating-point drift over many `vex update` cycles (watch mode).
    // So skip the unchanged slice when the manifest confirms it's
    // already normalized; otherwise normalize everything (legacy
    // pre-1.13 lazy-promotion path).
    if existing_normalized {
        // Only the freshly-embedded tail needs normalization.
        for v in all_vectors[unchanged_count..].iter_mut() {
            crate::search::semantic::normalize_in_place(v);
        }
    } else {
        for v in all_vectors.iter_mut() {
            crate::search::semantic::normalize_in_place(v);
        }
    }

    let manifest_embedder = if opts.with_embeddings && !all_vectors.is_empty() {
        Some(embedder_id.to_string())
    } else {
        None
    };
    let vector_dim = vector_dim_for(embedder_id, &all_vectors);
    write_output_locked(
        &root,
        &all_parsed,
        &all_vectors,
        vector_dim,
        &file_hashes,
        manifest_embedder,
        opts,
        false, // is_full_rebuild — incremental update, skeletons partial
        &artefacts,
    )?;

    if !all_vectors.is_empty() {
        let all_hashes = compute_hashes_for(&all_parsed, embedder_id)?;
        // v1.15.0 B1.2: try the incremental HNSW path first.
        //   Ok(true)  — incremental applied; HNSW + hash-index sidecar
        //               on disk in post-update state. SKIP full rebuild.
        //   Ok(false) — function bailed before any disk mutation
        //               (cold start, tombstone threshold, dim mismatch,
        //               corrupt sidecar, usearch load failure). FALL
        //               THROUGH to full rebuild — safe because the
        //               on-disk HNSW from the previous build hasn't
        //               been touched.
        //   Err       — HNSW saved successfully but the sidecar rewrite
        //               then failed. The two files are inconsistent;
        //               propagate so the orchestrator surfaces the
        //               error loudly. (`HnswHandle::open` size-check
        //               will bail to brute force until the next
        //               successful update self-heals.)
        match build_hnsw_incremental(&root, &all_vectors, &all_hashes)? {
            true => tracing::debug!("HNSW incremental update applied; skipping full rebuild"),
            false => build_hnsw(&root, &all_vectors, &all_hashes)?,
        }
        // E3 sweep — update path: must use `all_hashes` over
        // `unchanged + newly_parsed`, NOT just `_new_hashes` from
        // generate_embeddings (that would evict every unchanged
        // symbol's cache entry and defeat the cache on the next run).
        let dim = vector_dim_for(embedder_id, &all_vectors);
        let _ = prune_embed_cache(&root, embedder_id, dim, &all_hashes);
    } else {
        // Same paired cleanup as the full-rebuild path — drop both
        // sidecars together so the next `vex search --semantic` gets
        // a consistent picture (or bails to brute force).
        let hnsw_path = config::hnsw_path(&root);
        if hnsw_path.exists() {
            std::fs::remove_file(&hnsw_path).context("remove stale HNSW index")?;
        }
        let hash_index_path = config::hash_index_path(&root);
        if hash_index_path.exists() {
            std::fs::remove_file(&hash_index_path)
                .context("remove stale HNSW hash-index sidecar")?;
        }
    }

    tracing::info!(
        total = symbol_count,
        reused = unchanged_sym_count,
        reparsed = new_sym_count,
        "incremental update complete"
    );

    Ok(Some((symbol_count, diff.changed.len(), diff.deleted.len())))
}

#[cfg(test)]
mod tests;
