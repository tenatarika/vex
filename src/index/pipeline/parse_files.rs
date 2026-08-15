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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rayon::prelude::*;

use crate::index::hasher;
use crate::index::symbols::{HierarchyCapture, ParsedFile, ParsedSymbol, RawCallEdge, SymbolKind};
use crate::index::types::{ReconstructedRef, ReconstructedUnresolvedRef};
use crate::parse;
use crate::parse::language::Language;
use crate::parse::scope::RefKind;
use crate::util::config;

use super::CHUNK_SIZE;

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
    /// Unresolved-by-name refs carried forward from unchanged files
    /// (multi-repo Phase 6). Empty when the old index predates v7.
    pub reconstructed_unresolved_refs: Vec<ReconstructedUnresolvedRef>,
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
                // Reconstructed from a prior index — no bytes read, so no
                // fresh bloom. The sidecar writer carries the old record
                // forward for this path (see output.rs trigram block).
                trigram_bloom: None,
                // Placeholder — overwritten below (P2a carry-forward
                // block) for files that had hierarchy edges in the OLD
                // index. Left empty here because that block runs after
                // every `ParsedFile` has been assembled and needs the
                // final `path` to look up the OLD file_id.
                hierarchy_captures: Vec::new(),
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
            // Reconstructed — see the flush block above.
            trigram_bloom: None,
            // Placeholder — see the per-flush comment above.
            hierarchy_captures: Vec::new(),
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

    // Multi-repo Phase 6: carry forward unresolved-by-name refs for
    // unchanged files. Without this, the first `vex update` would drop
    // every unchanged file's unresolved refs (they live in the new v7
    // section, NOT the resolved RefEdge section the loop above reads),
    // silently breaking cross-repo strict usages. Simpler than the
    // resolved carry-forward: the name IS the key — no re-resolution, no
    // path-tiebreak — so we just re-emit with the OLD from_file_id, which
    // the writer maps to the new file_id exactly like `reconstructed_refs`.
    let mut reconstructed_unresolved_refs: Vec<ReconstructedUnresolvedRef> = Vec::new();
    if reader.has_unresolved_refs() {
        let mut name_intern: HashMap<String, Arc<str>> = HashMap::new();
        for (name, edge) in reader.unresolved_refs_all() {
            let Some(from_path) = old_file_paths.get(edge.from_file_id as usize) else {
                continue;
            };
            // Changed/deleted files re-emit fresh unresolved refs through
            // the parse path this turn — skip their stale carry-forward.
            if changed.contains(from_path.as_str()) || deleted.contains(from_path.as_str()) {
                continue;
            }
            if name.is_empty() {
                continue;
            }
            let name_arc = if let Some(a) = name_intern.get(&name) {
                a.clone()
            } else {
                let a: Arc<str> = Arc::from(name.as_str());
                name_intern.insert(name, a.clone());
                a
            };
            let kind = RefKind::try_from((edge.col_and_kind >> 24) as u8).unwrap_or(RefKind::Value);
            reconstructed_unresolved_refs.push(ReconstructedUnresolvedRef {
                from_file_id: edge.from_file_id,
                name: name_arc,
                line: edge.line,
                col: edge.col_and_kind & 0x00FF_FFFF,
                kind,
            });
        }
    }

    // P2a (`docs/HIERARCHY-EDGES.md` §8, architect CRITICAL-1 — mandatory):
    // carry forward hierarchy captures for unchanged files.
    //
    // Unlike `reconstructed_refs`/`reconstructed_unresolved_refs` (which
    // feed a SEPARATE writer-side re-emission pass keyed on OLD
    // file_id → OLD path → NEW file_id), this uses the
    // "reconstruct-captures-and-re-resolve" mechanism: rebuild
    // `HierarchyCapture { child_name, parent_name, kind, line }` tuples
    // (the exact shape `capture_hierarchy_edges` would have produced on a
    // fresh parse) and stash them directly on `ParsedFile.hierarchy_captures`
    // for the unchanged file. The EXISTING `resolve_hierarchy_captures`
    // post-loop pass in `store::writer` then re-resolves them against the
    // NEW index uniformly — no separate remap/re-resolve pass needed here,
    // because captures are NAME-based (not index-based) and file_id is
    // assigned fresh during writer assembly.
    //
    // Why re-resolve instead of copying the OLD resolved `to_sym_idx`
    // verbatim: a parent that moved to a different (changed) file, was
    // renamed, or was deleted entirely is only handled correctly by feeding
    // the name through Pass-2 again — a stale sym_idx carried forward
    // verbatim could point at the wrong (or a since-reused) symbol slot.
    //
    // Both the resolved hierarchy_edges section (keyed by `to_sym_idx`,
    // i.e. by PARENT) and the unresolved_hierarchy section (keyed by
    // parent NAME) are read via their `_all()` enumeration accessors and
    // bucketed by `from_file_id` up front — a single pass over each
    // section, not one `hierarchy_edges_all()` call per unchanged file.
    if reader.has_hierarchy_edges() || reader.has_unresolved_hierarchy_edges() {
        let mut resolved_by_file: HashMap<u32, Vec<HierarchyCapture>> = HashMap::new();

        for edge in reader.hierarchy_edges_all() {
            let Some(child_rec) = reader.symbol(edge.from_sym_idx as usize) else {
                tracing::warn!(
                    from_sym_idx = edge.from_sym_idx,
                    "hierarchy_edges corruption: from_sym_idx past symbol_count"
                );
                continue;
            };
            let child_name = reader.read_string(child_rec.name_offset);
            if child_name.is_empty() {
                continue;
            }
            let Some(parent_rec) = reader.symbol(edge.to_sym_idx as usize) else {
                tracing::warn!(
                    to_sym_idx = edge.to_sym_idx,
                    "hierarchy_edges corruption: to_sym_idx past symbol_count"
                );
                continue;
            };
            let parent_name = reader.read_string(parent_rec.name_offset);
            if parent_name.is_empty() {
                continue;
            }
            resolved_by_file
                .entry(edge.from_file_id)
                .or_default()
                .push(HierarchyCapture {
                    child_name: child_name.to_string(),
                    parent_name: parent_name.to_string(),
                    kind: edge.edge_kind_bits(),
                    line: edge.line(),
                });
        }

        let mut unresolved_by_file: HashMap<u32, Vec<HierarchyCapture>> = HashMap::new();
        for (parent_name, edge) in reader.unresolved_hierarchy_all() {
            if parent_name.is_empty() {
                continue;
            }
            let Some(child_rec) = reader.symbol(edge.from_sym_idx as usize) else {
                tracing::warn!(
                    from_sym_idx = edge.from_sym_idx,
                    "unresolved_hierarchy corruption: from_sym_idx past symbol_count"
                );
                continue;
            };
            let child_name = reader.read_string(child_rec.name_offset);
            if child_name.is_empty() {
                continue;
            }
            unresolved_by_file
                .entry(edge.from_file_id)
                .or_default()
                .push(HierarchyCapture {
                    child_name: child_name.to_string(),
                    parent_name,
                    kind: edge.edge_kind_bits(),
                    line: edge.line(),
                });
        }

        if !resolved_by_file.is_empty() || !unresolved_by_file.is_empty() {
            // OLD path -> OLD file_id, built once (not per-file / per-edge)
            // so the per-file lookup below is O(1) instead of an O(files)
            // linear scan over `old_file_paths` for every unchanged file.
            let old_file_id_by_path: HashMap<&str, u32> = old_file_paths
                .iter()
                .enumerate()
                .map(|(fid, path)| (path.as_str(), fid as u32))
                .collect();

            for pf in &mut parsed_files {
                // changed/deleted files never reach `parsed_files` in this
                // function (filtered at the top of the main loop), so no
                // extra changed/deleted guard is needed here — every entry
                // in `parsed_files` is by construction an unchanged file.
                let Some(&file_id) = old_file_id_by_path.get(pf.path.as_str()) else {
                    continue;
                };
                let mut caps = resolved_by_file.remove(&file_id).unwrap_or_default();
                if let Some(mut extra) = unresolved_by_file.remove(&file_id) {
                    caps.append(&mut extra);
                }
                pf.hierarchy_captures = caps;
            }
        }
    }

    ReconstructionResult {
        parsed_files,
        vectors,
        reconstructed_refs,
        old_file_paths,
        reconstructed_unresolved_refs,
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

/// The previous run's stat fingerprints plus the hashes they correspond to.
///
/// Reuse is only sound when both halves agree, so they travel together rather
/// than as two loose maps a caller could pair up wrongly.
pub(super) struct StatCache<'a> {
    stats: &'a BTreeMap<String, crate::index::incremental_state::FileStat>,
    hashes: &'a HashMap<String, u64>,
    /// `hashed_at` (Unix seconds) of the run that produced `stats` — stamped
    /// immediately before that run's hashing pass.
    ///
    /// A file whose mtime is not strictly older than this is re-hashed. It is
    /// deliberately NOT the manifest's `indexed_at`: that is stamped after
    /// parse and embed, which would declare the whole run's duration
    /// trustworthy and leave every edit landing inside it guarded only by
    /// exact `(len, mtime_ns)` equality.
    hashed_at: u64,
    enabled: bool,
}

impl StatCache<'_> {
    /// A cache that never hits — used by the full-index path, which has no
    /// prior run to trust, and by the plain [`hash_files`] entry point.
    pub(super) fn disabled() -> Self {
        static EMPTY_STATS: std::sync::LazyLock<
            BTreeMap<String, crate::index::incremental_state::FileStat>,
        > = std::sync::LazyLock::new(BTreeMap::new);
        static EMPTY_HASHES: std::sync::LazyLock<HashMap<String, u64>> =
            std::sync::LazyLock::new(HashMap::new);
        Self {
            stats: &EMPTY_STATS,
            hashes: &EMPTY_HASHES,
            hashed_at: 0,
            enabled: false,
        }
    }
}

impl<'a> StatCache<'a> {
    pub(super) fn new(
        stats: &'a BTreeMap<String, crate::index::incremental_state::FileStat>,
        hashes: &'a HashMap<String, u64>,
        hashed_at: Option<u64>,
    ) -> Self {
        let hashed_at = hashed_at.unwrap_or(0);
        Self {
            stats,
            hashes,
            hashed_at,
            // An index written before `hashed_at` existed has no cutoff, so the
            // racily-clean guard cannot be applied and reuse would be unsound.
            // Fall back to hashing everything; the next run records one.
            enabled: hashed_at > 0,
        }
    }
}

/// One hashing pass's output: the per-file content hashes, the stat
/// fingerprints to persist alongside them, and the cutoff those fingerprints
/// are valid against.
///
/// The three travel together because they are only meaningful together — a
/// fingerprint set paired with the wrong cutoff, or with hashes from a
/// different pass, is exactly the mistake that would make reuse unsound.
pub(super) struct HashedFiles {
    pub(super) hashes: Vec<(String, u64)>,
    pub(super) stats: BTreeMap<String, crate::index::incremental_state::FileStat>,
    /// Unix seconds, stamped immediately before the pass began.
    pub(super) hashed_at: u64,
}

/// Hash every file, reusing the previous run's hash for files whose `(len,
/// mtime)` is unchanged.
///
/// ## Why this is safe, and where it stops being safe
///
/// Reuse requires three things to hold, not two:
///
/// 1. `len` identical to the recorded fingerprint,
/// 2. `mtime` identical to the nanosecond,
/// 3. that mtime **strictly older** than the previous run's `hashed_at` — the
///    instant stamped just before that run started hashing.
///
/// (3) is git's "racily clean" guard. Without it, a file written in the same
/// clock second as the hashing pass could be modified again, keep a `(len,
/// mtime)` pair the index already recorded, and be skipped forever. With it,
/// anything touched at or after the instant hashing began is re-hashed, so the
/// window closes.
///
/// The cutoff must be the hashing pass's own timestamp, not the manifest's
/// `indexed_at`: the latter is stamped after parse and embed complete, which on
/// a large index is seconds to minutes later, and would silently trust every
/// mtime in that whole interval.
///
/// A second, narrower window is inherent and accepted: `metadata()` and
/// `read()` are separate syscalls, so an edit landing exactly between them
/// records a stat and a hash from different instants. The next run's live
/// `metadata()` will disagree with the recorded stat and re-hash, unless the
/// edit reproduced the same `(len, mtime_ns)` — the same collision the
/// paragraph below covers.
///
/// What remains, and is a deliberate trade every build system makes: a writer
/// that changes a file's bytes while **preserving both its length and its
/// mtime** (`touch -r`, some archive extractors, a filesystem with coarse
/// timestamps) is invisible. `vex index` always hashes everything, so a full
/// rebuild is the escape hatch.
pub(super) fn hash_files_with_stat_cache(
    root: &Path,
    files: &[std::path::PathBuf],
    cache: &StatCache<'_>,
) -> HashedFiles {
    // Stamp BEFORE the pass, so anything modified while it runs lands at or
    // after the cutoff and is re-hashed next time.
    let hashed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let pairs: Vec<(String, u64, crate::index::incremental_state::FileStat)> = files
        .par_iter()
        .filter_map(|path| {
            let rel = crate::util::paths::to_rel_posix(path, root)?;
            let meta = std::fs::metadata(path).ok()?;
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let stat = crate::index::incremental_state::FileStat {
                len: meta.len(),
                mtime_ns,
            };

            if cache.enabled && mtime_ns > 0 {
                let mtime_secs = mtime_ns / 1_000_000_000;
                if mtime_secs < cache.hashed_at {
                    if let (Some(prev), Some(hash)) =
                        (cache.stats.get(&rel), cache.hashes.get(&rel))
                    {
                        if *prev == stat {
                            return Some((rel, *hash, stat));
                        }
                    }
                }
            }

            let content = std::fs::read(path).ok()?;
            let hash = hasher::content_hash(&content);
            Some((rel, hash, stat))
        })
        .collect();

    let mut hashes = Vec::with_capacity(pairs.len());
    let mut stats = BTreeMap::new();
    for (rel, hash, stat) in pairs {
        stats.insert(rel.clone(), stat);
        hashes.push((rel, hash));
    }
    HashedFiles {
        hashes,
        stats,
        hashed_at,
    }
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
                        Ok(Ok(mut parsed)) => {
                            // grep trigram skip-index (STORAGE-RESEARCH §2):
                            // build the per-file presence bloom from the raw
                            // bytes we just read. Attaching it here means it
                            // rides into `encode_entry`'s cache slot (so a
                            // future cache hit restores it for free) AND into
                            // the returned `ParsedFile` (so the sidecar writer
                            // records it for untracked files that never touch
                            // the cache). Over raw bytes, not body_tokens —
                            // comments must be searchable.
                            parsed.trigram_bloom = Some(
                                *crate::grep::trigram::TrigramBloom::from_bytes(content.as_bytes())
                                    .as_bytes(),
                            );

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

#[cfg(test)]
mod hierarchy_carry_forward_tests {
    //! P2a (`docs/HIERARCHY-EDGES.md` §8) unit tests for the
    //! `reconstruct_unchanged` hierarchy-capture carry-forward, isolated
    //! from the full `pipeline::run`/`pipeline::update` harness (that
    //! end-to-end coverage lives in
    //! `tests/incremental_consistency_hierarchy.rs`). These build a
    //! minimal on-disk index directly via `write_index_full` — with real
    //! `HierarchyCapture`s on a `ParsedFile` so the writer's existing
    //! `resolve_hierarchy_captures` populates real hierarchy_edges /
    //! unresolved_hierarchy sections — then call `reconstruct_unchanged`
    //! against that index and assert the rebuilt `ParsedFile.hierarchy_captures`
    //! match what a fresh parse would have produced.
    use super::*;
    use crate::store::format::EdgeKind;
    use crate::store::reader::IndexReader;
    use crate::store::writer::write_index_full;

    fn mk_sym(name: &str, kind: SymbolKind, line: usize) -> ParsedSymbol {
        ParsedSymbol {
            name: name.to_string(),
            kind,
            line,
            signature: None,
            doc: None,
            body_tokens: None,
        }
    }

    fn mk_file(
        path: &str,
        symbols: Vec<ParsedSymbol>,
        captures: Vec<HierarchyCapture>,
    ) -> ParsedFile {
        ParsedFile {
            path: path.to_string(),
            symbols,
            refs: Vec::new(),
            call_edges: Vec::new(),
            bound_refs: Vec::new(),
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
            hierarchy_captures: captures,
        }
    }

    #[test]
    fn reconstruct_unchanged_carries_forward_resolved_hierarchy_capture() {
        // a.rs defines Base; b.rs defines Derived extending Base
        // (resolved edge). Neither file is in `changed`/`deleted`, so
        // reconstruct_unchanged must rebuild the capture for b.rs.
        let file_a = mk_file("a.rs", vec![mk_sym("Base", SymbolKind::Class, 1)], vec![]);
        let file_b = mk_file(
            "b.rs",
            vec![mk_sym("Derived", SymbolKind::Class, 5)],
            vec![HierarchyCapture {
                child_name: "Derived".to_string(),
                parent_name: "Base".to_string(),
                kind: EdgeKind::Extends as u8,
                line: 5,
            }],
        );
        let parsed = vec![file_a, file_b];

        let tmp = tempfile::TempDir::new().unwrap();
        let index_path = tmp.path().join("index.vex");
        write_index_full(&parsed, &[], 384, &index_path).expect("write index");

        let reader = IndexReader::open(&index_path).expect("open index");
        assert!(
            reader.has_hierarchy_edges(),
            "fixture must produce a resolved edge"
        );

        let changed: HashSet<&str> = HashSet::new();
        let deleted: HashSet<&str> = HashSet::new();
        let recon = reconstruct_unchanged(&reader, &changed, &deleted, None);

        let b_file = recon
            .parsed_files
            .iter()
            .find(|f| f.path == "b.rs")
            .expect("b.rs must be reconstructed");
        assert_eq!(
            b_file.hierarchy_captures.len(),
            1,
            "b.rs's hierarchy capture must be carried forward, not dropped"
        );
        let cap = &b_file.hierarchy_captures[0];
        assert_eq!(cap.child_name, "Derived");
        assert_eq!(cap.parent_name, "Base");
        assert_eq!(cap.kind, EdgeKind::Extends as u8);
        assert_eq!(cap.line, 5);
    }

    #[test]
    fn reconstruct_unchanged_carries_forward_unresolved_hierarchy_capture() {
        // b.rs's Derived extends an external/stdlib "Base" with zero
        // local candidates — spills to unresolved_hierarchy. The
        // carry-forward must reconstruct the SAME capture shape so
        // Pass-2 spills it again on the next write (still unresolved,
        // since nothing in this fixture defines "Base" locally).
        let file_b = mk_file(
            "b.rs",
            vec![mk_sym("Derived", SymbolKind::Class, 5)],
            vec![HierarchyCapture {
                child_name: "Derived".to_string(),
                parent_name: "ExternalBase".to_string(),
                kind: EdgeKind::Extends as u8,
                line: 5,
            }],
        );
        let parsed = vec![file_b];

        let tmp = tempfile::TempDir::new().unwrap();
        let index_path = tmp.path().join("index.vex");
        write_index_full(&parsed, &[], 384, &index_path).expect("write index");

        let reader = IndexReader::open(&index_path).expect("open index");
        assert!(
            reader.has_unresolved_hierarchy_edges(),
            "fixture must produce an unresolved spill"
        );
        assert!(!reader.has_hierarchy_edges(), "must NOT be a resolved edge");

        let changed: HashSet<&str> = HashSet::new();
        let deleted: HashSet<&str> = HashSet::new();
        let recon = reconstruct_unchanged(&reader, &changed, &deleted, None);

        let b_file = recon
            .parsed_files
            .iter()
            .find(|f| f.path == "b.rs")
            .expect("b.rs must be reconstructed");
        assert_eq!(
            b_file.hierarchy_captures.len(),
            1,
            "b.rs's unresolved hierarchy capture must be carried forward"
        );
        let cap = &b_file.hierarchy_captures[0];
        assert_eq!(cap.child_name, "Derived");
        assert_eq!(cap.parent_name, "ExternalBase");
        assert_eq!(cap.kind, EdgeKind::Extends as u8);
    }

    #[test]
    fn reconstruct_unchanged_skips_captures_for_changed_files() {
        // b.rs is in the `changed` set — its capture must NOT be carried
        // forward (it's about to be re-parsed with fresh captures).
        let file_a = mk_file("a.rs", vec![mk_sym("Base", SymbolKind::Class, 1)], vec![]);
        let file_b = mk_file(
            "b.rs",
            vec![mk_sym("Derived", SymbolKind::Class, 5)],
            vec![HierarchyCapture {
                child_name: "Derived".to_string(),
                parent_name: "Base".to_string(),
                kind: EdgeKind::Extends as u8,
                line: 5,
            }],
        );
        let parsed = vec![file_a, file_b];

        let tmp = tempfile::TempDir::new().unwrap();
        let index_path = tmp.path().join("index.vex");
        write_index_full(&parsed, &[], 384, &index_path).expect("write index");

        let reader = IndexReader::open(&index_path).expect("open index");

        let mut changed: HashSet<&str> = HashSet::new();
        changed.insert("b.rs");
        let deleted: HashSet<&str> = HashSet::new();
        let recon = reconstruct_unchanged(&reader, &changed, &deleted, None);

        assert!(
            recon.parsed_files.iter().all(|f| f.path != "b.rs"),
            "changed files must not appear in the reconstructed set at all"
        );
        // a.rs (unchanged, no captures of its own) must still reconstruct
        // cleanly with an empty hierarchy_captures — no false-positive
        // carry-forward of b.rs's capture onto the wrong file.
        let a_file = recon
            .parsed_files
            .iter()
            .find(|f| f.path == "a.rs")
            .expect("a.rs must be reconstructed");
        assert!(a_file.hierarchy_captures.is_empty());
    }

    #[test]
    fn reconstruct_unchanged_handles_index_with_no_hierarchy_section() {
        // Simulates a pre-P2 v8 index (or effectively v7 — no hierarchy
        // captures were ever written) via a fixture with zero captures.
        // reconstruct_unchanged must not panic and must produce empty
        // hierarchy_captures for every reconstructed file.
        let file_a = mk_file("a.rs", vec![mk_sym("Foo", SymbolKind::Class, 1)], vec![]);
        let parsed = vec![file_a];

        let tmp = tempfile::TempDir::new().unwrap();
        let index_path = tmp.path().join("index.vex");
        write_index_full(&parsed, &[], 384, &index_path).expect("write index");

        let reader = IndexReader::open(&index_path).expect("open index");
        assert!(!reader.has_hierarchy_edges());
        assert!(!reader.has_unresolved_hierarchy_edges());

        let changed: HashSet<&str> = HashSet::new();
        let deleted: HashSet<&str> = HashSet::new();
        let recon = reconstruct_unchanged(&reader, &changed, &deleted, None);

        assert_eq!(recon.parsed_files.len(), 1);
        assert!(recon.parsed_files[0].hierarchy_captures.is_empty());
    }
}
