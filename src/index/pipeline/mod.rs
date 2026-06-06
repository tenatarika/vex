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
use parse_files::{
    build_blob_cache, discover_files, hash_files, parse_files, reconstruct_unchanged,
};

const CHUNK_SIZE: usize = 500;
const EMBED_BATCH_SIZE: usize = 256;

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
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            with_embeddings: false,
            with_call_graph: true,
            with_bm25: true,
            with_pattern_index: true,
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
        generate_embeddings(&all_parsed, embedder_id, root)?
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
    )?;

    if !vectors.is_empty() {
        build_hnsw(root, &vectors, &hashes)?;
        // E3 sweep — full-rebuild path: `hashes` from generate_embeddings
        // is already the full live set, no separate compute_hashes_for
        // needed. Reclaims entries for symbols deleted/renamed since
        // the previous build.
        let dim = vector_dim_for(embedder_id, &vectors);
        let _ = prune_embed_cache(root, embedder_id, dim, &hashes);
    } else {
        // Remove stale HNSW + hash-index sidecar from a previous
        // --semantic run to prevent wrong results when the user
        // re-runs without `--semantic`. Both files come and go as a
        // pair (v1.14.1 B1.1).
        let hnsw_path = config::hnsw_path(root);
        if hnsw_path.exists() {
            std::fs::remove_file(&hnsw_path).context("remove stale HNSW index")?;
        }
        let hash_index_path = config::hash_index_path(root);
        if hash_index_path.exists() {
            std::fs::remove_file(&hash_index_path)
                .context("remove stale HNSW hash-index sidecar")?;
        }
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

    let changed_set: HashSet<&str> = diff.changed.iter().map(|s| s.as_str()).collect();
    let deleted_set: HashSet<&str> = diff.deleted.iter().map(|s| s.as_str()).collect();

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
    let (unchanged_parsed, unchanged_vectors) = if index_path.exists() {
        let reader = crate::store::reader::IndexReader::open(&index_path)
            .context("open existing index for incremental merge")?;
        reconstruct_unchanged(
            &reader,
            &changed_set,
            &deleted_set,
            body_tokens_sidecar.as_deref(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    // v1.13 P5: existing index's `vectors_normalized` flag drives the
    // partial-normalize decision below. Loaded once before the merge so
    // we can skip re-normalizing already-unit vectors (avoiding
    // floating-point drift that would accumulate across many
    // `vex update` cycles in watch mode).
    let existing_normalized = crate::index::manifest::Manifest::load(&config::manifest_path(&root))
        .ok()
        .and_then(|m| m.vectors_normalized)
        .unwrap_or(false);

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
        generate_embeddings(&newly_parsed, embedder_id, &root)?
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
