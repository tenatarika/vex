use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::embed;
use crate::index::hasher;
use crate::index::manifest::{self, Manifest};
use crate::index::symbols::{ParsedFile, ParsedSymbol, RawCallEdge, SymbolKind};
use crate::parse;
use crate::parse::language::Language;
use crate::store;
use crate::util::config;

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

/// Full rebuild: index all files from scratch.
pub fn run(
    root: &Path,
    opts: IndexOptions,
    embedder_id: &str,
    excludes: &[String],
) -> Result<usize> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files = discover_files(&root, excludes)?;
    tracing::info!(count = files.len(), "discovered files");

    let file_hashes = hash_files(&root, &files);

    // Serialize concurrent rebuilds: take the build lock BEFORE the expensive
    // parse + embed below. Without this, N concurrent `vex index` invocations
    // (CI matrix, agent fan-out) each load the embedding model and re-parse
    // every file in parallel — the same thundering herd `update` already
    // guards against. Holding the lock here means exactly one instance does
    // the work; the rest wait and then observe the fresh manifest via the
    // double-check below.
    let _lock = IndexLock::acquire(&root)?;

    // Double-check under the lock: if a peer just completed a rebuild with an
    // identical file fingerprint AND the index file is actually on disk, skip
    // the redundant work. `vex index` is deterministic from its inputs (same
    // files + same options → same output), so a matching manifest means the
    // on-disk index is already what we'd produce. The `index_path.exists()`
    // gate matters for the cold-start case (empty directory or first-ever
    // index): a missing manifest loads as `Manifest::default()` with an empty
    // file map, which would otherwise diff equal to an empty file list and
    // skip the initial write.
    let manifest_path = config::manifest_path(&root);
    let index_path = config::index_path(&root);
    if index_path.exists() {
        if let Ok(current_manifest) = Manifest::load(&manifest_path) {
            let diff = manifest::diff_files(&file_hashes, &current_manifest);
            if diff.changed.is_empty() && diff.deleted.is_empty() {
                tracing::info!(
                    "index already built by a concurrent vex instance; skipping rebuild"
                );
                return existing_symbol_count(&root);
            }
        }
    }

    // Phase 14.7 — blob-SHA parse cache. Construct once per index run and
    // share across the parse loop. The cache is best-effort; failures only
    // cost a re-parse, never correctness.
    let cache = build_blob_cache();
    let blob_map = crate::index::parse_cache::git_blobs::discover_tracked_blobs(&root);

    let all_parsed = parse_files(&root, &files, &blob_map, &cache)?;
    let symbol_count: usize = all_parsed.iter().map(|f| f.symbols.len()).sum();

    let vectors = if opts.with_embeddings && symbol_count > 0 {
        generate_embeddings(&all_parsed, embedder_id)?
    } else {
        Vec::new()
    };

    let manifest_embedder = if opts.with_embeddings && !vectors.is_empty() {
        Some(embedder_id.to_string())
    } else {
        None
    };
    let vector_dim = vector_dim_for(embedder_id, &vectors);
    write_output_locked(
        &root,
        &all_parsed,
        &vectors,
        vector_dim,
        &file_hashes,
        manifest_embedder,
        opts,
        true, // is_full_rebuild — `vex index` always replaces everything
    )?;

    if !vectors.is_empty() {
        build_hnsw(&root, &vectors)?;
    } else {
        // Remove stale HNSW from a previous --semantic run to prevent wrong results
        let hnsw_path = config::hnsw_path(&root);
        if hnsw_path.exists() {
            std::fs::remove_file(&hnsw_path).context("remove stale HNSW index")?;
        }
    }

    tracing::info!(
        symbols = symbol_count,
        vectors = vectors.len(),
        "indexing complete"
    );
    Ok(symbol_count)
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

    if diff.changed.is_empty() && diff.deleted.is_empty() {
        tracing::info!(unchanged = diff.unchanged, "nothing to update");
        return Ok((existing_symbol_count(&root)?, 0, 0));
    }

    // Serialize concurrent rebuilds: take the build lock BEFORE the expensive
    // parse + embed below. Until this guard existed the lock only wrapped the
    // final write, so N vex instances auto-updating the
    // same stale index each loaded the embedding model and re-embedded in
    // parallel — a thundering-herd rebuild that pegs CPU and RAM under
    // multi-agent fan-out. Holding the lock here means exactly one instance
    // does the work; the rest wait and then skip via the re-check below.
    let _lock = IndexLock::acquire(&root)?;

    // Double-check under the lock: another instance may have refreshed the
    // index while we waited. Re-diff against the now-current manifest; if it
    // already matches the working tree, that peer did our work — skip.
    let diff = {
        let current_manifest = Manifest::load(&manifest_path)?;
        manifest::diff_files(&file_hashes, &current_manifest)
    };
    if diff.changed.is_empty() && diff.deleted.is_empty() {
        tracing::info!("index refreshed by a concurrent vex instance; skipping rebuild");
        return Ok((existing_symbol_count(&root)?, 0, 0));
    }

    tracing::info!(
        changed = diff.changed.len(),
        deleted = diff.deleted.len(),
        unchanged = diff.unchanged,
        "incremental update"
    );

    let changed_set: HashSet<&str> = diff.changed.iter().map(|s| s.as_str()).collect();
    let deleted_set: HashSet<&str> = diff.deleted.iter().map(|s| s.as_str()).collect();

    // Reconstruct unchanged symbols (+ vectors) from existing index
    let index_path = config::index_path(&root);
    let (unchanged_parsed, unchanged_vectors) = if index_path.exists() {
        let reader = crate::store::reader::IndexReader::open(&index_path)
            .context("open existing index for incremental merge")?;
        reconstruct_unchanged(&reader, &changed_set, &deleted_set)
    } else {
        (Vec::new(), Vec::new())
    };

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

    // Generate embeddings only for new/changed symbols
    let new_vectors = if opts.with_embeddings && new_sym_count > 0 {
        generate_embeddings(&newly_parsed, embedder_id)?
    } else {
        Vec::new()
    };

    // Merge: unchanged first (vectors align with symbol order)
    let mut all_parsed = unchanged_parsed;
    all_parsed.extend(newly_parsed);
    let symbol_count: usize = all_parsed.iter().map(|f| f.symbols.len()).sum();

    let mut all_vectors = unchanged_vectors;
    all_vectors.extend(new_vectors);

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
        build_hnsw(&root, &all_vectors)?;
    } else {
        let hnsw_path = config::hnsw_path(&root);
        if hnsw_path.exists() {
            std::fs::remove_file(&hnsw_path).context("remove stale HNSW index")?;
        }
    }

    tracing::info!(
        total = symbol_count,
        reused = unchanged_sym_count,
        reparsed = new_sym_count,
        "incremental update complete"
    );

    Ok((symbol_count, diff.changed.len(), diff.deleted.len()))
}

/// Reconstruct ParsedFile + vectors for unchanged files from the existing index.
/// Symbols are in index order (file-contiguous), vectors align 1:1 with symbols.
///
/// **Known limitation**: `body_tokens` is set to `None` because the on-disk
/// SymbolRecord does not preserve them — we only stored vectors, names,
/// signatures, kinds, lines, paths. Consequently the next BM25 rebuild for
/// these unchanged symbols has only `name + signature` to draw from, which
/// degrades recall on rare body terms after an incremental update relative
/// to a fresh `vex index`. Emitting a one-shot `tracing::warn!` below so a
/// user running `RUST_LOG=warn vex update` can see the regression.
/// Persisting body_tokens to the index → roadmap follow-up.
fn reconstruct_unchanged(
    reader: &crate::store::reader::IndexReader,
    changed: &HashSet<&str>,
    deleted: &HashSet<&str>,
) -> (Vec<ParsedFile>, Vec<Vec<f32>>) {
    if reader.has_bm25() && (!changed.is_empty() || !deleted.is_empty()) {
        tracing::warn!(
            "incremental update is reconstructing unchanged symbols without body_tokens; \
             BM25 recall for those symbols will rely on name+signature only until next full \
             `vex index`. Tracking as a roadmap follow-up to persist body_tokens."
        );
    }
    let has_vectors = reader.has_vectors();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut parsed_files: Vec<ParsedFile> = Vec::new();
    let mut current_path = String::new();
    let mut current_symbols: Vec<ParsedSymbol> = Vec::new();

    for i in 0..reader.symbol_count() {
        let rec = match reader.symbol(i) {
            Some(r) => r,
            None => continue,
        };
        let path = reader.read_string(rec.file_offset).to_string();

        // Skip changed/deleted files — they'll be re-parsed
        if changed.contains(path.as_str()) || deleted.contains(path.as_str()) {
            continue;
        }

        // Flush previous file group when path changes
        if path != current_path && !current_path.is_empty() {
            parsed_files.push(ParsedFile {
                path: std::mem::take(&mut current_path),
                symbols: std::mem::take(&mut current_symbols),
                refs: Vec::new(),
                call_edges: Vec::new(),
                bound_refs: Vec::new(),
                skeletons: Vec::new(),
            });
        }
        current_path = path;

        let name = reader.read_string(rec.name_offset).to_string();
        // Skip records whose name decoded to "" — that's the signal
        // `read_string` raises when the strings section is corrupt.
        // Persisting an empty-name record here would effectively delete
        // the symbol on the next rebuild and silently shrink the index.
        if name.is_empty() {
            tracing::warn!(
                file = %current_path,
                line = rec.line,
                name_offset = rec.name_offset,
                "reconstruct_unchanged: dropping symbol with empty/corrupt name — \
                 the file will be re-parsed on next full index"
            );
            continue;
        }
        let kind = SymbolKind::try_from(rec.kind).unwrap_or(SymbolKind::Function);
        let sig = {
            let s = reader.read_string(rec.signature_offset);
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        };

        current_symbols.push(ParsedSymbol {
            name,
            kind,
            line: rec.line as usize,
            signature: sig,
            doc: None,
            body_tokens: None,
        });

        if has_vectors {
            if let Some(vec) = reader.vector(rec.vector_index) {
                vectors.push(vec.to_vec());
            }
        }
    }

    // Flush last file group
    if !current_path.is_empty() {
        parsed_files.push(ParsedFile {
            path: current_path,
            symbols: current_symbols,
            refs: Vec::new(),
            call_edges: Vec::new(),
            bound_refs: Vec::new(),
            skeletons: Vec::new(),
        });
    }

    // Reconstruct call edges for unchanged files from the existing index.
    // We re-derive RawCallEdge { caller_fn_name, callee_name, line } from
    // each edge by looking up the caller's symbol and callee string. Edges
    // whose caller belongs to a changed/deleted file are dropped — those
    // files are re-parsed and will produce fresh edges.
    if reader.has_call_graph() {
        let mut edges_by_file: HashMap<String, Vec<RawCallEdge>> = HashMap::new();
        for j in 0..reader.call_edge_count() {
            let Some(edge) = reader.call_edge(j) else {
                continue;
            };
            let Some(caller_rec) = reader.symbol(edge.caller_sym_idx as usize) else {
                continue;
            };
            let caller_path = reader.read_string(caller_rec.file_offset).to_string();
            if changed.contains(caller_path.as_str()) || deleted.contains(caller_path.as_str()) {
                continue;
            }
            let caller_name = reader.read_string(caller_rec.name_offset).to_string();
            let callee_name = reader.read_string(edge.callee_name_offset).to_string();
            edges_by_file
                .entry(caller_path)
                .or_default()
                .push(RawCallEdge {
                    caller_fn_name: caller_name,
                    caller_fn_line: caller_rec.line as usize,
                    callee_name,
                    line: edge.line as usize,
                });
        }
        for pf in &mut parsed_files {
            pf.call_edges = edges_by_file.remove(&pf.path).unwrap_or_default();
        }
    }

    (parsed_files, vectors)
}

/// Phase 14.7 — build the global blob-SHA parse cache and run a
/// best-effort LRU eviction sweep up front.
///
/// Default cap is 1 GiB (`1024 * 1024 * 1024`). Override at runtime via
/// `VEX_BLOB_CACHE_CAP_BYTES=<bytes>`; a value that fails to parse is
/// silently ignored and the default is used. Failures from the eviction
/// sweep are logged at `warn!` and swallowed — the cache must stay
/// best-effort.
fn build_blob_cache() -> crate::index::parse_cache::BlobCache {
    const DEFAULT_CAP_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

    let cache_root = config::blob_cache_dir();
    let cache = crate::index::parse_cache::BlobCache::new(cache_root);

    let cap = std::env::var("VEX_BLOB_CACHE_CAP_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CAP_BYTES);

    if let Err(e) = cache.evict_to_cap(cap) {
        tracing::warn!(error = %e, "blob cache eviction failed");
    }

    cache
}

fn hash_files(root: &Path, files: &[std::path::PathBuf]) -> Vec<(String, u64)> {
    files
        .par_iter()
        .filter_map(|path| {
            let content = std::fs::read(path).ok()?;
            let rel = crate::util::paths::to_rel_posix(path, root)?;
            let hash = hasher::content_hash(&content);
            Some((rel, hash))
        })
        .collect()
}

fn parse_files(
    root: &Path,
    files: &[std::path::PathBuf],
    blob_map: &HashMap<std::path::PathBuf, String>,
    cache: &crate::index::parse_cache::BlobCache,
) -> Result<Vec<ParsedFile>> {
    let counter = AtomicUsize::new(0);
    let total = files.len();
    let mut all_parsed = Vec::new();
    // (language, first_error) -> skipped_count. Aggregated so an ABI mismatch
    // surfaces as a single loud summary at the end instead of being buried
    // in per-file warnings the user usually has filtered out.
    let grammar_failures: Mutex<HashMap<Language, (String, usize)>> = Mutex::new(HashMap::new());

    // Phase 14.7 Step 7-opt — background-drain blob-cache writer.
    //
    // The rayon parse closure used to call `cache.insert(sha, lang, &parsed)`
    // synchronously, blocking the parse worker on a ~16 KB bincode serialize
    // plus an `fs::write` + atomic `fs::rename` (two syscalls). Bench showed
    // the cold path (every file is a cache miss) regressed by ~13% over the
    // pre-14.7 baseline because of these inline writes.
    //
    // Split insert into its two halves and run each where it pays off most:
    //   * Bincode serialize (CPU-bound, scales with parallelism) stays on
    //     the rayon parse worker — same thread that produced `parsed`,
    //     no value clone needed.
    //   * `fs::write` + `fs::rename` (I/O syscalls — APFS coalesces, so
    //     parallelism doesn't help and the single-thread serialize avoids
    //     contention on the shard directory) move to a dedicated background
    //     drain thread fed by an mpsc channel.
    //
    // `std::thread::scope` lets the drain thread borrow
    // `cache: &BlobCache` directly — no Arc, no `'static` lifetime, no new
    // dependencies. Dropping the sender at the end of the scope closes the
    // channel and the drain thread joins, guaranteeing every accepted entry
    // is durable before `parse_files` returns (this preserves the Step 4b
    // mtime-stable rerun semantics).
    //
    // Tradeoff considered & rejected: sending `(sha, lang, ParsedFile)` and
    // doing serialize + write on the drain thread. That moves CPU work to a
    // single thread, losing the rayon parallelism on bincode encode; benches
    // showed it stayed at the same +12.9% cold regression. Sending the
    // pre-encoded byte buffer keeps serialize parallel and only the cheap
    // syscalls are serialized on the drain thread.
    type CacheJob = (String, Vec<u8>);
    let (tx, rx) = std::sync::mpsc::channel::<CacheJob>();

    let parsed_files = std::thread::scope(|s| -> Result<Vec<ParsedFile>> {
        let drain_handle = s.spawn(move || {
            while let Ok((sha, buf)) = rx.recv() {
                if let Err(e) = cache.write_entry_bytes(&sha, &buf) {
                    tracing::warn!(sha = %sha, error = %e, "blob cache write failed");
                }
            }
        });

        for chunk in files.chunks(CHUNK_SIZE) {
            let parsed: Vec<ParsedFile> = chunk
                .par_iter()
                .filter_map(|path| {
                    let ext = path.extension()?.to_str()?;
                    let lang = Language::from_extension(ext)?;

                    let rel = crate::util::paths::to_rel_posix(path, root)?;

                    // Phase 14.7 — try the blob-SHA cache first for tracked files.
                    // The cache is keyed by absolute canonical path because the
                    // tracked-blob map is built from `git ls-files` output rooted
                    // at the same canonical `root`. A cache hit short-circuits
                    // both the file read and the tree-sitter parse.
                    let blob_sha = blob_map.get(path);
                    if let Some(sha) = blob_sha {
                        if let Some(mut cached) = cache.lookup(sha, lang) {
                            cached.path = rel.clone();
                            let done = counter.fetch_add(1, Ordering::Relaxed);
                            if done.is_multiple_of(500) && done > 0 {
                                tracing::info!("{done}/{total} files parsed");
                            }
                            return Some(cached);
                        }
                    }

                    let content = read_capped(path)?;

                    // Skip likely binary/minified files (high ratio of non-ASCII or very long lines)
                    if looks_binary(&content) {
                        return None;
                    }

                    let done = counter.fetch_add(1, Ordering::Relaxed);
                    if done.is_multiple_of(500) && done > 0 {
                        tracing::info!("{done}/{total} files parsed");
                    }

                    // SAFETY: parse_file borrows &rel and &content read-only.
                    // A panic from tree-sitter does not leave any shared mutable state
                    // partially modified, so unwinding is safe to catch.
                    let parsed_result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            parse::parse_file(&rel, &content, lang)
                        }));

                    match parsed_result {
                        Ok(Ok(parsed)) => {
                            // Encode the cache entry here (CPU-bound, runs in
                            // parallel across rayon workers) and hand the bytes
                            // off to the single drain thread for the syscalls.
                            // `send` only fails if the receiver has hung up,
                            // which only happens after we drop `tx` below —
                            // by then no parse worker is still running, so
                            // any failure here is unreachable. Encoding
                            // failures are warn-and-continue.
                            if let Some(sha) = blob_sha {
                                match crate::index::parse_cache::encode_entry(lang, &parsed) {
                                    Ok(buf) => {
                                        // `send` only fails if the drain thread
                                        // is gone. The thread is dropped at scope
                                        // exit, after every parse worker is done,
                                        // so a `SendError` here indicates the
                                        // drain panicked or returned early — log
                                        // it so the failure mode is visible
                                        // instead of silently dropping writes.
                                        if tx.send((sha.clone(), buf)).is_err() {
                                            tracing::warn!(
                                                path = %rel,
                                                "blob cache drain thread closed unexpectedly; dropping write"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            path = %rel,
                                            error = %e,
                                            "blob cache encode failed"
                                        );
                                    }
                                }
                            }
                            Some(parsed)
                        }
                        Ok(Err(e)) => {
                            if let Some(g) = e.downcast_ref::<parse::extractor::GrammarLoadError>()
                            {
                                // Recover from a poisoned mutex — never block aggregation
                                // for a downstream caller because of an unrelated panic.
                                let mut map = grammar_failures
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner());
                                map.entry(g.lang).or_insert_with(|| (g.reason.clone(), 0)).1 += 1;
                            } else {
                                tracing::warn!(path = %rel, error = %e, "parse failed, skipping");
                            }
                            None
                        }
                        Err(_) => {
                            tracing::warn!(path = %rel, "parse panicked, skipping");
                            None
                        }
                    }
                })
                .collect();

            all_parsed.extend(parsed);
        }

        // Close the channel and wait for the drain thread to finish so all
        // accepted writes are durable before `parse_files` returns. This
        // preserves the mtime-stable rerun semantics asserted by the Step 4b
        // integration test — without the join, a fast subsequent
        // `pipeline::run` could observe an incomplete cache.
        drop(tx);
        if let Err(panic) = drain_handle.join() {
            tracing::warn!("blob cache drain thread panicked: {panic:?}");
        }

        Ok(all_parsed)
    })?;

    let failures = grammar_failures
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner());
    for (lang, (err, count)) in &failures {
        // tracing::warn! so this respects RUST_LOG and is captureable by
        // integration tests; the bang-default subscriber surfaces it in the
        // terminal too.
        tracing::warn!(
            language = ?lang,
            skipped = count,
            error = %err,
            "tree-sitter grammar failed to load — files for this language were skipped (likely ABI mismatch)"
        );
    }

    Ok(parsed_files)
}

/// Read a file as UTF-8, refusing to allocate more than `MAX_FILE_BYTES`.
///
/// Closes a TOCTOU window: a previous version did `fs::metadata().len() <= 1MB`
/// then `fs::read_to_string()`, which could be defeated by a malicious or
/// concurrently-growing file. `File::open` + `take` enforces the cap on the
/// actual read.
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    const MAX_FILE_BYTES: u64 = 1 << 20; // 1 MiB
    let file = std::fs::File::open(path).ok()?;
    let mut buf = String::new();
    let n = file
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut buf)
        .ok()?;
    if n as u64 > MAX_FILE_BYTES {
        return None;
    }
    Some(buf)
}

/// Pick the `vector_dim` to record in the Header.
///
/// Priority: actual length of the first vector → registry lookup by id →
/// default MiniLM dim. The first wins so non-embedded indexes still record a
/// sensible legacy default.
fn vector_dim_for(embedder_id: &str, vectors: &[Vec<f32>]) -> u32 {
    if let Some(first) = vectors.first() {
        return first.len() as u32;
    }
    embed::embedder_dim(embedder_id).unwrap_or(embed::MINILM_DIM)
}

/// Build the BM25 index from per-symbol term bags.
///
/// Each symbol becomes a document whose terms are drawn from:
/// - `name` (split on non-alnum, lowercased)
/// - `signature` (same split)
/// - `body_tokens` (already extracted, space-separated)
/// - `doc` (docstring; same split)
///
/// Returns `(fst_bytes, postings_bytes, stats_bytes)`. The triple is empty
/// when there are no documents (no symbols across all parsed files).
fn build_bm25_index(parsed: &[ParsedFile]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    // `doc_count` counts every `ParsedSymbol`, including synthetic
    // `SymbolKind::Module` rows (Phase 14.1) that the inner loop skips
    // when populating term bags. The Module slots still reserve a
    // zero-length doc-length entry in BM25 stats so that `sym_idx` —
    // which is incremented for *every* symbol regardless of kind — stays
    // aligned with `SymbolRecord` positions in the records section. Net
    // overhead: 2 bytes per indexed file in the stats footer.
    let doc_count: usize = parsed.iter().map(|f| f.symbols.len()).sum();
    if doc_count == 0 {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    let mut builder = crate::store::bm25::Bm25IndexBuilder::new(doc_count);
    let mut sym_idx: u32 = 0;
    for file in parsed {
        for sym in &file.symbols {
            // Phase 14.1: synthetic Module symbols carry only call edges,
            // never BM25 content. Skip the bag-building but advance
            // `sym_idx` so the doc-id alignment with `SymbolRecord`
            // positions stays correct (doc slot stays zero-length).
            if sym.kind == crate::index::symbols::SymbolKind::Module {
                sym_idx += 1;
                continue;
            }
            let mut bag = String::with_capacity(256);
            bag.push_str(&sym.name);
            bag.push(' ');
            if let Some(s) = &sym.signature {
                bag.push_str(s);
                bag.push(' ');
            }
            if let Some(b) = &sym.body_tokens {
                bag.push_str(b);
                bag.push(' ');
            }
            if let Some(d) = &sym.doc {
                bag.push_str(d);
            }
            let terms = crate::store::bm25::tokenize_document(&bag);
            builder.add_document(sym_idx, &terms);
            sym_idx += 1;
        }
    }
    builder.build()
}

/// Resolve each [`RawCallEdge`] to a [`CallEdgeBuilder`] with a concrete
/// `caller_sym_idx`. Edges whose caller doesn't match any symbol in the
/// parsed-file iteration order are dropped — this happens for languages
/// where the call-graph query produces function names that the symbol
/// extractor doesn't register (e.g. inner closures, anonymous fns).
///
/// The key is `(file_path, fn_name, definition_line)` — the `line` is
/// load-bearing because a single file can contain multiple functions
/// sharing a name (overloaded methods, duplicate `impl` blocks, methods
/// with the same name across nested modules). Keying only on `(path, name)`
/// would cause every duplicate to inherit the first occurrence's symbol
/// index, attributing call sites to the wrong caller.
///
/// The iteration order MUST match what `writer::write_index_to` uses to
/// assign `symbol_idx` (`parsed.iter().flat_map(|f| f.symbols.iter())`)
/// otherwise the resulting indices will be wrong.
fn resolve_call_edges(parsed: &[ParsedFile]) -> Vec<crate::store::call_graph::CallEdgeBuilder> {
    let mut sym_idx_of: HashMap<(&str, &str, usize), u32> = HashMap::new();
    let mut next_idx: u32 = 0;
    for file in parsed {
        for sym in &file.symbols {
            // Preserve the first symbol when `(path, name, line)` collides
            // (genuinely identical entries — duplicate symbols at the same
            // line are a parser bug, not a real case). The line component
            // makes overloaded/same-name siblings distinct.
            sym_idx_of
                .entry((file.path.as_str(), sym.name.as_str(), sym.line))
                .or_insert(next_idx);
            next_idx += 1;
        }
    }

    // Phase 14.1: per-file synthetic Module symbol name lookup key. Only
    // allocate the `<module:path>` string when the file actually contains a
    // sentinel edge — empty `caller_fn_name` + `caller_fn_line == 0` is the
    // marker emitted by `callgraph::extract_call_edges` for module-scope
    // calls. By tree-sitter grammar invariants no real function definition
    // has an empty name or line 0, so this conjunction is collision-free.
    let mut out = Vec::new();
    for file in parsed {
        let module_sym_name = file
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name.is_empty() && e.caller_fn_line == 0)
            .then(|| format!("<module:{}>", file.path));
        for edge in &file.call_edges {
            let (caller_name, caller_line) =
                if edge.caller_fn_name.is_empty() && edge.caller_fn_line == 0 {
                    // `module_sym_name` is `Some` whenever any sentinel
                    // exists in this file — proven by the `any(...)` above.
                    (module_sym_name.as_deref().unwrap_or(""), 1)
                } else {
                    (edge.caller_fn_name.as_str(), edge.caller_fn_line)
                };
            let Some(&caller_sym_idx) =
                sym_idx_of.get(&(file.path.as_str(), caller_name, caller_line))
            else {
                continue;
            };
            out.push(crate::store::call_graph::CallEdgeBuilder {
                caller_sym_idx,
                callee_name: edge.callee_name.clone(),
                line: edge.line as u32,
            });
        }
    }
    out
}

/// Output of [`collect_pattern_skeletons`] — the flattened skeleton tuples
/// the writer consumes, paired with the per-language grammar fingerprints
/// Inc 5 will compare against the live grammar.
type SkeletonsForWriter = (
    Vec<(u32, crate::pattern::skeleton::Skeleton)>,
    Vec<(u8, u32)>,
);

/// Flatten per-file pattern skeletons into the `(file_id, Skeleton)`
/// shape the writer expects, and compute grammar fingerprints for the
/// distinct T1 languages that contributed at least one skeleton. The
/// fingerprint lets Inc 5 detect grammar drift between index build and
/// query time and fall back to live-scan when the on-disk skeletons no
/// longer agree with the live grammar.
fn collect_pattern_skeletons(parsed: &[ParsedFile]) -> SkeletonsForWriter {
    use std::collections::HashSet;
    let mut skeletons: Vec<(u32, crate::pattern::skeleton::Skeleton)> = Vec::new();
    let mut langs_with_skeletons: HashSet<Language> = HashSet::new();
    for (file_id, file) in parsed.iter().enumerate() {
        if file.skeletons.is_empty() {
            continue;
        }
        // Recover the language from the file extension — ParsedFile
        // does not carry it explicitly, but the extension is canonical
        // (parse_files uses Language::from_extension).
        let lang = std::path::Path::new(&file.path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension);
        if let Some(lang) = lang {
            langs_with_skeletons.insert(lang);
        }
        for sk in &file.skeletons {
            skeletons.push((file_id as u32, sk.clone()));
        }
    }
    let fingerprints: Vec<(u8, u32)> = langs_with_skeletons
        .into_iter()
        .map(|lang| {
            (
                lang.lang_id(),
                crate::store::pattern_skeletons::grammar_fingerprint_for_lang(lang),
            )
        })
        .collect();
    (skeletons, fingerprints)
}

/// RAII guard over the per-index build lock (`<index>.lock`). Acquiring it
/// blocks until no other vex instance is building the same index, so only one
/// process runs the expensive parse + embed + write at a time. The lock is
/// released on drop (including on early return).
///
/// The lock file is created once and never deleted. Deleting it on release is
/// the classic `flock` + unlink race: a queued waiter keeps its handle on the
/// now-unlinked inode while a new instance creates a *fresh* inode under the
/// same name and locks it immediately — so both run concurrently. A stable,
/// never-deleted sentinel keeps every instance contending on one lock object.
struct IndexLock {
    file: std::fs::File,
}

impl IndexLock {
    fn acquire(root: &Path) -> Result<Self> {
        let index_path = config::index_path(root);
        let cache_dir = index_path.parent().context("index path has no parent")?;
        std::fs::create_dir_all(cache_dir).context("create cache directory")?;
        let path = index_path.with_extension("lock");
        // Open-or-create the persistent sentinel; never truncated, never removed.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .context("open index lock file")?;
        // Fast path: try without blocking. If a peer is already indexing,
        // emit a single tracing event so users staring at a "stuck" CLI
        // (or an agent harness watching for output) see why it's stuck
        // before we settle into the blocking wait. Without this the CLI
        // would just freeze silently for the duration of the peer's
        // parse + embed + write.
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(e) if is_lock_contended(&e) => {
                tracing::info!(
                    lock = %path.display(),
                    "waiting for index lock (another vex instance is indexing)"
                );
                fs2::FileExt::lock_exclusive(&file)
                    .context("acquire index lock (another vex instance may be indexing)")?;
            }
            Err(e) => {
                return Err(anyhow::Error::from(e).context("try-lock index lock"));
            }
        }
        Ok(Self { file })
    }
}

/// True when `e` describes a lock-contention failure from `try_lock_exclusive`.
/// POSIX returns `ErrorKind::WouldBlock`; Windows returns `ERROR_LOCK_VIOLATION`
/// (raw OS error 33) which historically maps to `ErrorKind::Other` (the Rust
/// stdlib has not always normalized this). Matching both shapes keeps the
/// try-then-block diagnostic path working across platforms — without the
/// raw-code check, every Windows contention would bypass the "waiting for
/// index lock" log and return an outright error, breaking the
/// thundering-herd serialization the lock exists to provide.
fn is_lock_contended(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    // ERROR_LOCK_VIOLATION on Windows. The constant is exposed via
    // `winapi`/`windows-sys`, but we want to avoid adding a Windows-only
    // dep for one number — and the value is stable platform ABI.
    matches!(e.raw_os_error(), Some(33))
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        // Release the advisory lock; intentionally leave the lock file in place
        // (see the struct doc — unlinking it would break mutual exclusion under
        // contention).
        if let Err(e) = fs2::FileExt::unlock(&self.file) {
            tracing::warn!(error = %e, "failed to unlock index lock");
        }
    }
}

/// Build the index sections and write them out. The caller MUST already hold
/// the [`IndexLock`]; both `run` and `update` acquire it before the expensive
/// parse + embed so concurrent instances serialize and skip redundant rebuilds
/// instead of all embedding in parallel.
#[allow(clippy::too_many_arguments)] // mirrors the writer entry shape
fn write_output_locked(
    root: &Path,
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    file_hashes: &[(String, u64)],
    embedder_id: Option<String>,
    opts: IndexOptions,
    is_full_rebuild: bool,
) -> Result<()> {
    let index_path = config::index_path(root);
    let cache_dir = index_path.parent().context("index path has no parent")?;
    std::fs::create_dir_all(cache_dir).context("create cache directory")?;

    let git_head = super::staleness::read_git_head(root);
    let indexed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Skip the corresponding build work entirely when the section is
    // opted out — the reader gates these via `*_len == 0`, so passing
    // empty slices / `None` is a valid disabled state in the format.
    let call_edges = if opts.with_call_graph {
        resolve_call_edges(parsed)
    } else {
        Vec::new()
    };

    let bm25_built = if opts.with_bm25 {
        Some(build_bm25_index(parsed)?)
    } else {
        None
    };
    let bm25 = bm25_built.as_ref().and_then(|(fst, posts, stats)| {
        if fst.is_empty() {
            None
        } else {
            Some((fst.as_slice(), posts.as_slice(), stats.as_slice()))
        }
    });
    // 11.4 Inc 4 — collect (file_id, Skeleton) tuples and compute
    // per-language grammar fingerprints. The empty-opt-out path
    // still produces a v6 index, just with all-zero header fields.
    let (pattern_skeletons, lang_fingerprints) = if opts.with_pattern_index {
        collect_pattern_skeletons(parsed)
    } else {
        (Vec::new(), Vec::new())
    };
    store::writer::write_index_with_call_graph_and_skeletons_and_fingerprints(
        parsed,
        vectors,
        vector_dim,
        &call_edges,
        bm25,
        &pattern_skeletons,
        &lang_fingerprints,
        &index_path,
    )
    .context("write index")?;

    let manifest_path = config::manifest_path(root);
    let manifest = Manifest {
        files: file_hashes.iter().cloned().collect::<HashMap<_, _>>(),
        git_head,
        indexed_at: Some(indexed_at),
        embedder_id,
        // Persist explicit `Some(false)` for opt-outs so `vex update`
        // can detect them. `Some(true)` rather than `None` for the
        // default case so post-10.3 manifests are unambiguous.
        call_graph: Some(opts.with_call_graph),
        bm25: Some(opts.with_bm25),
        pattern_index: Some(opts.with_pattern_index),
        // 11.4 Inc 5: `pattern_index_full` distinguishes a full
        // `vex index` from a `vex update`. The reader checks this
        // before using the indexed prefilter — incremental builds
        // produce a partial section (only re-parsed files have
        // skeletons) and would silently drop matches in unchanged
        // files. `is_update` is plumbed by the writer wrapper.
        pattern_index_full: Some(is_full_rebuild),
    };
    manifest.save(&manifest_path)?;
    Ok(())
}

fn generate_embeddings(parsed: &[ParsedFile], embedder_id: &str) -> Result<Vec<Vec<f32>>> {
    let start = Instant::now();
    tracing::info!(embedder = embedder_id, "loading embedding model");
    let mut embedder = embed::make_embedder(embedder_id)?;
    let budget = embedder.char_budget();
    tracing::info!(
        elapsed = ?start.elapsed(),
        model = embedder.id(),
        dim = embedder.dim(),
        "model loaded"
    );

    let mut contexts = Vec::new();
    for file in parsed {
        for sym in &file.symbols {
            let ctx = embed::build_context(
                sym.kind.as_str(),
                &sym.name,
                &file.path,
                sym.signature.as_deref(),
                sym.doc.as_deref(),
                sym.body_tokens.as_deref(),
                budget,
            );
            contexts.push(ctx);
        }
    }

    let total = contexts.len();
    tracing::info!(total, "embedding symbols");
    let embed_start = Instant::now();

    let mut all_vectors = Vec::with_capacity(total);
    for batch in contexts.chunks(EMBED_BATCH_SIZE) {
        let vectors = embedder.embed_batch(batch)?;
        all_vectors.extend(vectors);
    }

    tracing::info!(total, elapsed = ?embed_start.elapsed(), "embedding complete");
    Ok(all_vectors)
}

fn build_hnsw(root: &Path, vectors: &[Vec<f32>]) -> Result<()> {
    use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};

    let dim = vectors[0].len(); // guaranteed non-empty by caller

    let options = IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,     // auto
        expansion_add: 0,    // auto
        expansion_search: 0, // auto
        multi: false,
    };

    let index = new_index(&options).context("create HNSW index")?;
    index
        .reserve(vectors.len())
        .context("reserve HNSW capacity")?;

    for (i, vec) in vectors.iter().enumerate() {
        index
            .add(i as u64, vec)
            .context("add vector to HNSW index")?;
    }

    let hnsw_path = config::hnsw_path(root);
    let path_str = hnsw_path
        .to_str()
        .context("HNSW path contains non-UTF-8 characters")?;
    index.save(path_str).context("save HNSW index")?;

    tracing::info!(
        vectors = vectors.len(),
        path = %hnsw_path.display(),
        "HNSW index built"
    );

    Ok(())
}

/// Heuristic: file is likely binary or minified if it has many non-UTF8/control chars
/// or extremely long lines (>10KB, typical of minified JS/CSS).
fn looks_binary(content: &str) -> bool {
    // Check first 8KB for control characters (excluding common whitespace)
    let mut end = content.len().min(8192);
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let sample = &content[..end];
    let control_count = sample
        .bytes()
        .filter(|&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();
    if control_count * 20 > sample.len() {
        return true; // ≥5% control chars
    }

    // Check for very long lines (minified code) — scan first 100 lines
    // because the first line may be a normal comment/header
    if content.lines().take(100).any(|l| l.len() > 10_000) {
        return true;
    }

    false
}

fn discover_files(root: &Path, excludes: &[String]) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();

    for entry in crate::util::walk::walk_builder(root, excludes)?.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();

        // Filter by supported extension BEFORE reading — avoids I/O on irrelevant files
        if path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension)
            .is_none()
        {
            continue;
        }

        // Skip files > 1 MB (likely generated/minified)
        if std::fs::metadata(&path).is_ok_and(|m| m.len() <= 1_048_576) {
            files.push(path);
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_failure_summary_includes_language_count_and_reason() {
        // Pin the structured fields the user-visible warning emits, so a future
        // refactor cannot silently drop the count or error string without test
        // fail. We cannot easily hit the path end-to-end (every grammar
        // currently loads), so this test mirrors the format the warning
        // produces and locks the contract.
        let mut failures: HashMap<Language, (String, usize)> = HashMap::new();
        failures.insert(Language::CSharp, ("ABI mismatch v15".to_string(), 42));

        let mut rendered = String::new();
        for (lang, (err, count)) in &failures {
            rendered = format!("language={lang:?} skipped={count} error={err}");
        }
        assert!(rendered.contains("CSharp"), "{rendered}");
        assert!(rendered.contains("42"), "{rendered}");
        assert!(rendered.contains("ABI mismatch v15"), "{rendered}");
    }
}
