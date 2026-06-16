//! Parsing orchestrator for the indexing pipeline.
//!
//! `discover_files` walks the project, honoring excludes; `hash_files`
//! computes the content fingerprints fed into the manifest diff;
//! `parse_files` is the fan-out workhorse that drives `crate::parse`
//! across the discovered set with rayon, sharing a single blob-SHA
//! cache (`build_blob_cache`) across all files. `reconstruct_unchanged`
//! is the incremental-update fast path — it rebuilds `ParsedFile`s for
//! the untouched portion of the previous index without re-parsing.
//!
//! Isolated from `mod.rs` so the orchestration (lock handling, manifest
//! diff, skip-path decisions) stays separable from the parse pipeline.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rayon::prelude::*;

use crate::index::hasher;
use crate::index::symbols::{ParsedFile, ParsedSymbol, RawCallEdge, SymbolKind};
use crate::parse;
use crate::parse::language::Language;
use crate::parse::scope::RefKind;
use crate::util::config;

use super::CHUNK_SIZE;

/// Reconstructed ref-edge fed from `reconstruct_unchanged` into the
/// writer's second-pass resolution. Phase 11.1.9 (Q4-A).
///
/// `target_name` / `target_path` are `Arc<str>` interned across edges so
/// a 50M-edge re-emission doesn't allocate ~8 GB of redundant String
/// copies (architect-H1 / rust-reviewer-#2 must-fix). At typical repo
/// shapes (10k distinct target paths, 5k distinct target names) the
/// interners stay sub-megabyte.
#[derive(Debug, Clone)]
pub(crate) struct ReconstructedRef {
    /// `file_id` of the unchanged source file in the OLD index's file
    /// table. Resolved to a path in the writer via the `old_file_paths`
    /// slice and then mapped to the new index's `file_ids`.
    pub from_file_id: u32,
    pub target_name: Arc<str>,
    /// OLD-index path of the target's defining file — disambiguates
    /// `name_to_global` candidates when `target_name` has multiple
    /// definitions across the project.
    pub target_path: Arc<str>,
    pub line: u32,
    pub col: u32,
    pub kind: RefKind,
}

/// Bundled return from `reconstruct_unchanged` — architect-M1 must-fix.
/// Replaces the previous 3-tuple so a future Q4-B addition lands as a
/// named field, not another tuple-arity churn.
pub(super) struct ReconstructionResult {
    pub parsed_files: Vec<ParsedFile>,
    pub vectors: Vec<Vec<f32>>,
    pub reconstructed_refs: Vec<ReconstructedRef>,
    /// Old-index file_paths table, kept so the writer can resolve
    /// `ReconstructedRef.from_file_id` → path → new file_id.
    pub old_file_paths: Vec<String>,
}

/// Reconstruct ParsedFile + vectors for unchanged files from the existing index.
/// Symbols are in index order (file-contiguous), vectors align 1:1 with symbols.
///
/// `body_tokens_sidecar` carries per-sym_idx body_tokens loaded from
/// `index.bodytokens` (v1.15.0 B1.2):
///   - `Some(&slice)` — sidecar loaded successfully; per-symbol lookup
///     returns the stored body_tokens. A zero-length slice here is a
///     legitimate empty-index state, NOT a failure signal.
///   - `None` — sidecar absent / malformed / pre-v1.15 index; every
///     reconstructed symbol falls back to `body_tokens: None` and the
///     BM25 / `compute_hashes_for` rebuild on this update is body-less
///     for the unchanged slice.
///
/// Encoding "load succeeded but empty" vs "load failed" as `Option` (not
/// "empty slice means failed") keeps the BM25-regression warning from
/// firing on legitimate empty-corpus updates and prevents a truncated
/// sidecar from silently suppressing the warning for a partially-loaded
/// state.
pub(super) fn reconstruct_unchanged(
    reader: &crate::store::reader::IndexReader,
    changed: &HashSet<&str>,
    deleted: &HashSet<&str>,
    body_tokens_sidecar: Option<&[Option<String>]>,
) -> ReconstructionResult {
    if reader.has_bm25()
        && (!changed.is_empty() || !deleted.is_empty())
        && body_tokens_sidecar.is_none()
    {
        tracing::warn!(
            "incremental update is reconstructing unchanged symbols without body_tokens; \
             BM25 recall for those symbols will rely on name+signature only until next full \
             `vex index`. Persisting body_tokens lands in v1.15.0 B1.2 — re-run \
             `vex index` once to enable."
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
                cpp_includes: Vec::new(),
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

        let body_tokens = body_tokens_sidecar
            .and_then(|s| s.get(i))
            .cloned()
            .flatten();

        current_symbols.push(ParsedSymbol {
            name,
            kind,
            line: rec.line as usize,
            signature: sig,
            doc: None,
            body_tokens,
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
            cpp_includes: Vec::new(),
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

    // Phase 11.1.9 (Q4-A): reconstruct cross-file ref-edges for unchanged
    // files. Without this, every `vex update` silently drops every
    // bound_ref from unchanged files — `vex usages --strict` degrades to
    // an almost-empty result set after the first incremental update.
    // Mirror of the call_edges block above (uses old-index strings, lets
    // the writer's second pass re-resolve to NEW symbol indices).
    let old_file_paths = reader.file_paths();
    let mut reconstructed_refs: Vec<ReconstructedRef> = Vec::new();
    if reader.has_ref_edges() {
        // Path/name interners — dedupe Arc<str> across edges sharing
        // target_path or target_name. At typical repo shape (10k paths,
        // 5k names) these stay sub-megabyte.
        let mut path_intern: HashMap<String, Arc<str>> = HashMap::new();
        let mut name_intern: HashMap<String, Arc<str>> = HashMap::new();
        let edge_count = reader.ref_edge_count();
        for j in 0..edge_count {
            let Some(edge) = reader.ref_edge(j) else {
                tracing::warn!(
                    edge_idx = j,
                    "ref_edges section corruption: edge decode failed"
                );
                continue;
            };
            let from_file_id = edge.from_file_id;
            let Some(from_path) = old_file_paths.get(from_file_id as usize) else {
                tracing::warn!(
                    from_file_id,
                    "ref_edges corruption: from_file_id past file_paths"
                );
                continue;
            };
            // Expected drop — file in changed/deleted set will produce
            // fresh edges through the parse path this turn.
            if changed.contains(from_path.as_str()) || deleted.contains(from_path.as_str()) {
                continue;
            }
            let Some(target_rec) = reader.symbol(edge.to_sym_idx as usize) else {
                tracing::warn!(
                    to_sym_idx = edge.to_sym_idx,
                    "ref_edges corruption: to_sym_idx past symbol_count"
                );
                continue;
            };
            let target_name = reader.read_string(target_rec.name_offset);
            // Mirror parse_files.rs ~line 102 guard — strings-section
            // corruption surfaces as empty name; drop rather than
            // propagate a poisoned record.
            if target_name.is_empty() {
                tracing::warn!(
                    to_sym_idx = edge.to_sym_idx,
                    "ref_edges: target symbol has empty name (strings corrupt)"
                );
                continue;
            }
            let target_path = reader.read_string(target_rec.file_offset);
            let kind_byte = (edge.col_and_kind >> 24) as u8;
            let kind = RefKind::try_from(kind_byte).unwrap_or_else(|_| {
                tracing::warn!(kind_byte, "ref_edges: unknown RefKind, defaulting to Value");
                RefKind::Value
            });
            let col = edge.col_and_kind & 0x00FF_FFFF;

            let target_name_arc = if let Some(a) = name_intern.get(target_name) {
                a.clone()
            } else {
                let a: Arc<str> = Arc::from(target_name);
                name_intern.insert(target_name.to_string(), a.clone());
                a
            };
            let target_path_arc = if let Some(a) = path_intern.get(target_path) {
                a.clone()
            } else {
                let a: Arc<str> = Arc::from(target_path);
                path_intern.insert(target_path.to_string(), a.clone());
                a
            };

            reconstructed_refs.push(ReconstructedRef {
                from_file_id,
                target_name: target_name_arc,
                target_path: target_path_arc,
                line: edge.line,
                col,
                kind,
            });
        }
    }

    ReconstructionResult {
        parsed_files,
        vectors,
        reconstructed_refs,
        old_file_paths,
    }
}

/// Phase 14.7 — build the global blob-SHA parse cache and run a
/// best-effort LRU eviction sweep up front.
///
/// Default cap is 1 GiB (`1024 * 1024 * 1024`). Override at runtime via
/// `VEX_BLOB_CACHE_CAP_BYTES=<bytes>`; a value that fails to parse is
/// silently ignored and the default is used. Failures from the eviction
/// sweep are logged at `warn!` and swallowed — the cache must stay
/// best-effort.
pub(super) fn build_blob_cache() -> crate::index::parse_cache::BlobCache {
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

pub(super) fn hash_files(root: &Path, files: &[std::path::PathBuf]) -> Vec<(String, u64)> {
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

pub(super) fn parse_files(
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

                    // AssertUnwindSafe: parse_file borrows &rel and &content
                    // read-only. A panic from tree-sitter does not leave any
                    // shared mutable state partially modified, so unwinding is
                    // safe to catch (note: not a `// SAFETY:` invariant — no
                    // `unsafe` involved, just a UnwindSafe assertion).
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

pub(super) fn discover_files(root: &Path, excludes: &[String]) -> Result<Vec<std::path::PathBuf>> {
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
