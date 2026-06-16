use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};

use super::call_graph::{build_callees_fst, build_callers_fst, CallEdgeBuilder};
use super::format::{
    CallEdge, CallGraphHeader, Header, PatternSkeletonHeader, SymbolRecord, V5SectionHeader, MAGIC,
    VECTOR_DIM, VERSION,
};
use super::include_resolver;
use super::pattern_skeletons::build_pattern_skeleton_section;
use super::ref_edges::{build_ref_edges_section, RefEdgeBuilder};
use super::{refs_fst, symbol_fst};
use crate::parse::scope::BindTarget;
// RefKind ↔ u8 encoding lives at the scope module (`impl From<RefKind>
// for u8` + `impl TryFrom<u8> for RefKind`) so reconstruction and the
// reader can roundtrip without duplicating the bit layout.

use crate::index::symbols::ParsedFile;

/// Resolve `name` to a global SymbolRecord position, optionally
/// disambiguating by preferred file path. Phase 11.1.9 (Q4-A) — shared
/// by the `BindTarget::Imported` arm (preferred_path = None) and the
/// reconstruction second pass (preferred_path = Some(old target path)).
///
/// When `preferred_path` is `Some` and any candidate's new file path
/// matches, return that candidate. Otherwise fall back to the first
/// candidate — matches the historical `Imported` arm's
/// first-match-wins semantics.
fn resolve_by_name_and_path(
    name: &str,
    preferred_path: Option<&str>,
    name_to_global: &HashMap<&str, Vec<u32>>,
    sym_to_file_id: &[u32],
    file_paths_new: &[String],
) -> Option<u32> {
    let candidates = name_to_global.get(name)?;
    match preferred_path {
        // No path hint (the `Imported` arm) — historical first-match semantics.
        None => candidates.first().copied(),
        Some(pref) => {
            // Single candidate is unambiguous even if path doesn't
            // match (typical "file was renamed, symbol kept"); take it.
            if candidates.len() == 1 {
                return candidates.first().copied();
            }
            // Multi-candidate: require the path tie-break match.
            // Without it we'd silently mis-attribute when two unrelated
            // files define the same name (the load-bearing case
            // distinguishing A2 from the rejected A1 approach).
            for &cand in candidates {
                let cand_path = sym_to_file_id
                    .get(cand as usize)
                    .and_then(|fid| file_paths_new.get(*fid as usize));
                if cand_path.map(|p| p.as_str()) == Some(pref) {
                    return Some(cand);
                }
            }
            None
        }
    }
}
use crate::pattern::skeleton::Skeleton;

/// Vector dimension to record in the Header when no vectors are written.
/// Stays at the legacy MiniLM-L6-v2 value so v3 readers that ignore the
/// field continue to see what they expect.
const DEFAULT_VECTOR_DIM: u32 = VECTOR_DIM;

/// String pool that deduplicates strings and returns offsets.
struct StringPool {
    data: Vec<u8>,
    lookup: HashMap<String, u32>,
}

impl StringPool {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            lookup: HashMap::new(),
        }
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&offset) = self.lookup.get(s) {
            return offset;
        }
        let offset = self.data.len() as u32;
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0); // null terminator
        self.lookup.insert(s.to_string(), offset);
        offset
    }
}

/// Write parsed files into the binary index format (no vectors, no refs FST).
#[allow(dead_code)] // used by integration tests
pub fn write_index(parsed: &[ParsedFile], output: &Path) -> Result<()> {
    write_index_full(parsed, &[], DEFAULT_VECTOR_DIM, output)
}

/// Pre-built BM25 section bytes — `(fst, postings, stats)`. Passed in
/// already-serialised because building the BM25 index requires per-symbol
/// term bags that live in the pipeline, not the writer.
pub type Bm25Sections<'a> = (&'a [u8], &'a [u8], &'a [u8]);

/// Write parsed files + embedding vectors + refs FST into the binary index
/// format (no call graph, no BM25, no pattern skeletons). For indexes with
/// those sections use [`write_index_with_call_graph`].
pub fn write_index_full(
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    output: &Path,
) -> Result<()> {
    write_index_with_call_graph(parsed, vectors, vector_dim, &[], None, output)
}

/// Write parsed files + embedding vectors + refs FST + v4 sections
/// (call graph + optional BM25) into the binary index format. Uses atomic
/// write: writes to a temp file first, then renames on success.
///
/// Pattern skeletons are written as an empty v6 section (zeroed header).
/// To include real skeletons use [`write_index_with_call_graph_and_skeletons`].
pub fn write_index_with_call_graph(
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    call_edges: &[CallEdgeBuilder],
    bm25: Option<Bm25Sections<'_>>,
    output: &Path,
) -> Result<()> {
    write_index_with_call_graph_and_skeletons_and_fingerprints(
        parsed,
        vectors,
        vector_dim,
        call_edges,
        bm25,
        &[],
        &[],
        &[], // reconstructed_refs — full rebuild path
        &[], // old_file_paths
        output,
    )
}

/// Back-compat shim for the previous Inc 3 entry — preserved so internal
/// tests / pre-Inc 4 callers keep working. Inc 4 added `lang_fingerprints`
/// and pipeline callers now use [`write_index_with_call_graph_and_skeletons_and_fingerprints`]
/// directly.
#[allow(dead_code)]
pub fn write_index_with_call_graph_and_skeletons(
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    call_edges: &[CallEdgeBuilder],
    bm25: Option<Bm25Sections<'_>>,
    pattern_skeletons: &[(u32, Skeleton)],
    output: &Path,
) -> Result<()> {
    write_index_with_call_graph_and_skeletons_and_fingerprints(
        parsed,
        vectors,
        vector_dim,
        call_edges,
        bm25,
        pattern_skeletons,
        &[],
        &[], // reconstructed_refs — full rebuild path
        &[], // old_file_paths
        output,
    )
}

/// Write parsed files + embedding vectors + v4 sections + v6 pattern
/// skeletons with per-language grammar fingerprints. Uses atomic write.
///
/// `pattern_skeletons` is `&[(file_id, Skeleton)]`; pass `&[]` for an
/// empty section (still produces a v6 index — version bump is
/// unconditional). `lang_fingerprints` is `&[(lang_id, fingerprint)]`
/// for the distinct T1 languages that contributed skeletons; Inc 5
/// compares these against live grammar to detect drift.
#[allow(clippy::too_many_arguments)] // primary writer entry — keep flat over a builder for now
                                     // pub(crate) (was pub) — Phase 11.1.9 demoted this to crate-internal so the
                                     // `ReconstructedRef` type need not be exposed in the public API surface.
                                     // External callers (tests, benches) go through the back-compat shims
                                     // above which translate to empty reconstructed_refs.
pub(crate) fn write_index_with_call_graph_and_skeletons_and_fingerprints(
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    call_edges: &[CallEdgeBuilder],
    bm25: Option<Bm25Sections<'_>>,
    pattern_skeletons: &[(u32, Skeleton)],
    lang_fingerprints: &[(u8, u32)],
    // Phase 11.1.9 (Q4-A) — reconstructed cross-file ref-edges from
    // unchanged files. Empty for full `vex index`. Resolved to new
    // symbol indices after the per-file bound_refs loop closes.
    reconstructed_refs: &[crate::index::pipeline::ReconstructedRef],
    // `old_file_paths` is the OLD index's file_paths table — used to
    // map `ReconstructedRef.from_file_id` → path → new file_id via
    // this writer's freshly-built `file_ids`.
    old_file_paths: &[String],
    output: &Path,
) -> Result<()> {
    // Pre-validate every vector before opening the temp file. The header's
    // section offsets are computed from `vectors.len() * vector_dim`, so a
    // single bad vector slipping past would leave every downstream section
    // (strings, FST, postings, file table) pointing at the wrong bytes. We
    // must not write a single byte until all inputs are confirmed.
    for (i, vec) in vectors.iter().enumerate() {
        ensure!(
            vec.len() == vector_dim as usize,
            "vector {i} has wrong dimension: expected {vector_dim}, got {}",
            vec.len()
        );
    }

    let mut tmp_os = output.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp_path = PathBuf::from(tmp_os);

    if let Err(e) = write_index_to(
        &tmp_path,
        parsed,
        vectors,
        vector_dim,
        call_edges,
        bm25,
        pattern_skeletons,
        lang_fingerprints,
        reconstructed_refs,
        old_file_paths,
    ) {
        let _ = std::fs::remove_file(&tmp_path); // best-effort cleanup
        return Err(e);
    }
    std::fs::rename(&tmp_path, output)
        .with_context(|| format!("rename {} → {}", tmp_path.display(), output.display()))?;
    // On POSIX, fsync the parent directory so the rename itself is
    // durable across crashes. Windows has no equivalent operation —
    // ReplaceFile / MoveFileEx already provide the durability guarantee.
    // Best-effort: some filesystems (tmpfs) don't support directory fsync.
    #[cfg(unix)]
    {
        if let Some(parent) = output.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // internal write helper — mirrors the public entry shape
fn write_index_to(
    output: &Path,
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    call_edges: &[CallEdgeBuilder],
    bm25: Option<Bm25Sections<'_>>,
    pattern_skeletons: &[(u32, Skeleton)],
    lang_fingerprints: &[(u8, u32)],
    reconstructed_refs: &[crate::index::pipeline::ReconstructedRef],
    old_file_paths: &[String],
) -> Result<()> {
    let mut strings = StringPool::new();
    let mut records = Vec::new();
    let mut symbol_idx: u32 = 0;

    // Assign file_id sequentially per unique path. Collect ordered file table.
    let mut file_ids: HashMap<String, u32> = HashMap::new();
    let mut file_table: Vec<u32> = Vec::new(); // string offsets ordered by file_id

    for file in parsed {
        let str_offset = strings.intern(&file.path);
        file_ids.entry(file.path.clone()).or_insert_with(|| {
            let id = file_table.len() as u32;
            file_table.push(str_offset);
            id
        });

        for sym in &file.symbols {
            let name_offset = strings.intern(&sym.name);
            let sig_offset = sym
                .signature
                .as_deref()
                .map(|s| strings.intern(s))
                .unwrap_or(u32::MAX);

            let vec_idx = if !vectors.is_empty() && (symbol_idx as usize) < vectors.len() {
                symbol_idx
            } else {
                u32::MAX
            };

            records.push(SymbolRecord {
                name_offset,
                kind: sym.kind as u8,
                _pad: [0; 3],
                file_offset: str_offset,
                line: sym.line as u32,
                signature_offset: sig_offset,
                vector_index: vec_idx,
            });
            symbol_idx += 1;
        }
    }

    // Build FST + posting lists from refs
    let refs_input: Vec<(u32, &[crate::index::symbols::ParsedRef])> = parsed
        .iter()
        .map(|file| {
            let file_id = file_ids
                .get(&file.path)
                .copied()
                .expect("file_id must exist after prior loop");
            (file_id, file.refs.as_slice())
        })
        .collect();

    let (fst_bytes, posting_bytes) = refs_fst::build_refs_fst(&refs_input)?;

    // Build symbol FST: name + CamelCase sub-tokens → symbol indices.
    // Phase 14.1: skip synthetic `SymbolKind::Module` rows so `<module:path>`
    // is never returned by `vex search` — but ADVANCE `idx` for them so
    // every other entry's index keeps aligning with its `SymbolRecord`
    // position in the records section.
    let sym_entries: Vec<(String, u32)> = {
        let mut entries = Vec::new();
        let mut idx: u32 = 0;
        for file in parsed {
            for sym in &file.symbols {
                if sym.kind != crate::index::symbols::SymbolKind::Module {
                    entries.push((sym.name.clone(), idx));
                }
                idx += 1;
            }
        }
        entries
    };
    let (sym_fst_bytes, sym_posting_bytes) = symbol_fst::build_symbol_fst(&sym_entries)?;

    // Build call graph: intern callee names into the string pool, then
    // construct CallEdge records (resolved to string offsets) and the two
    // FSTs over those edges.
    let edge_records: Vec<CallEdge> = call_edges
        .iter()
        .map(|e| CallEdge {
            caller_sym_idx: e.caller_sym_idx,
            callee_name_offset: strings.intern(&e.callee_name),
            line: e.line,
            _pad: 0,
        })
        .collect();
    let (callers_fst_bytes, callers_post_bytes) = build_callers_fst(call_edges)?;
    let (callees_fst_bytes, callees_post_bytes) = build_callees_fst(call_edges)?;

    // v5 Pass 2 — cross-file Imported resolution (11.1.3c). Build a
    // name → global-idx index over every symbol so each
    // `BindTarget::Imported(use_path)` can be rewritten into a real
    // `to_sym_idx` by matching `use_path.segments.last()` against the
    // file's defining symbol. Ambiguous matches (same name in N files)
    // pick the first hit for now — the plan reserves a future
    // `RefKind::Ambiguous` flag, but that needs `--explain` to surface
    // it usefully so it's deferred.
    // v1.14.1 — `name_to_global` values are **SymbolRecord positions**
    // (the same space `BindTarget::ModuleSymbol(base_idx + local)` uses),
    // NOT sym_entries-positions. This is the fix for a long-standing
    // (since Phase 11.1.3c) inconsistency: before, the loop pushed the
    // post-Module-filter enumeration index `i`, so a ref resolved via
    // the `Imported` (or v1.14 `Unresolved` BFS) arm pointed at the
    // wrong `SymbolRecord` whenever any synthetic `<module:path>` row
    // sat before the target in `parsed → file.symbols`. Python /
    // TypeScript / Rust files with module-level statements all emit
    // such rows (Phase 14.1), so the bug fired on real projects, just
    // silently. Pushing `entries[i].1` (which carries the real
    // SymbolRecord position by construction — `sym_entries` was already
    // designed for this) unifies all three Pass-2 arms.
    let name_to_global: HashMap<&str, Vec<u32>> = {
        let mut m: HashMap<&str, Vec<u32>> = HashMap::with_capacity(records.len());
        for (name, global_idx) in &sym_entries {
            m.entry(name.as_str()).or_default().push(*global_idx);
        }
        m
    };

    // v1.14 — parallel `Vec<file_id>` indexed by **SymbolRecord position**.
    // The C++ include-BFS resolver below takes any sym_idx stored in
    // `name_to_global` values and translates it back to the defining
    // file_id. We include Module rows in this walk so the resulting Vec
    // is 1:1 with `records` — same indexing convention as the post-fix
    // `name_to_global` and the existing `ModuleSymbol(base_idx + local)`
    // arm.
    let sym_to_file_id: Vec<u32> = {
        let mut out: Vec<u32> = Vec::with_capacity(records.len());
        for file in parsed {
            let fid = *file_ids.get(&file.path).expect("file_id must exist");
            for _sym in &file.symbols {
                out.push(fid);
            }
        }
        out
    };
    // 1:1 with `records` is the new invariant. A length drift here
    // would silently corrupt every cross-file ref the BFS resolves
    // (`sym_to_file_id.get(...)` returns None on out-of-bounds, BFS
    // skips the candidate, the symbol resolves nowhere).
    debug_assert_eq!(
        sym_to_file_id.len(),
        records.len(),
        "sym_to_file_id and records must stay 1:1 (SymbolRecord position)",
    );

    // v1.14 — include graph for C++ files only. Non-C++ paths are filtered
    // out at the caller (extension via `Language::from_extension`) so the
    // graph stays small and `include_graph.contains_key(file_id)` doubles
    // as the "is this file C++?" gate inside the BFS.
    let include_graph = {
        let basename_index = include_resolver::build_basename_index(&file_ids);
        let cpp_files = parsed
            .iter()
            .filter(|f| is_cpp_path(&f.path))
            .map(|f| (f.path.as_str(), f.cpp_includes.as_slice()));
        include_resolver::build_include_graph(cpp_files, &file_ids, &basename_index)
    };

    let mut ref_edge_builders: Vec<RefEdgeBuilder> = Vec::new();
    {
        let mut base_idx: u32 = 0;
        for file in parsed {
            let file_id = file_ids
                .get(&file.path)
                .copied()
                .expect("file_id must exist");
            for r in &file.bound_refs {
                let to_sym_idx = match &r.target {
                    BindTarget::ModuleSymbol(local) => {
                        // `checked_add` would let an overflowing index
                        // silently disappear; we'd rather notice in
                        // tests and crash than ship a corrupt edge.
                        debug_assert!(
                            base_idx.checked_add(*local).is_some(),
                            "global symbol idx overflow at base_idx={base_idx} + local={local}",
                        );
                        Some(base_idx.wrapping_add(*local))
                    }
                    // Note: `file_paths_new` isn't built until after the
                    // per-file loop closes (it's the inverse of
                    // `file_ids` populated by the symbol-record assembly
                    // above). For the Imported arm we don't need
                    // path-tiebreak — `use_path` only carries a name —
                    // so pass an empty slice + `None` preferred_path,
                    // which short-circuits to first-candidate semantics
                    // in `resolve_by_name_and_path`.
                    BindTarget::Imported(use_path) => use_path.segments.last().and_then(|name| {
                        resolve_by_name_and_path(
                            name.as_str(),
                            None,
                            &name_to_global,
                            &sym_to_file_id,
                            &[],
                        )
                    }),
                    BindTarget::Local(_) => None,
                    // v1.14 — C++ include-BFS fallback. The BFS itself
                    // bails for non-C++ files via the include_graph
                    // membership check, so this branch stays language-
                    // agnostic; the gate lives in include_graph build.
                    //
                    // Index-space contract (locked v1.14.1): every arm
                    // here returns a **SymbolRecord position** — same
                    // space as `ModuleSymbol(base_idx + local)` above.
                    // `name_to_global` values were rewritten to push
                    // `entries[i].1` (the real SymbolRecord index) so
                    // the BFS's hits cleanly index into both records[]
                    // and the `sym_to_file_id` Vec built right above.
                    // Pre-1.14.1 builds pushed `i` (post Module-filter
                    // enumeration index) and silently mis-pointed every
                    // Imported/Unresolved ref whose target sat after
                    // any `<module:path>` synthetic row.
                    BindTarget::Unresolved => {
                        // First try the v1.14 C++ include-BFS path. Returns
                        // None for non-C++ files (their file_id isn't in
                        // `include_graph`) or when the target isn't reachable
                        // through any included header.
                        include_resolver::resolve_via_include_bfs(
                            &r.name,
                            file_id,
                            &name_to_global,
                            &sym_to_file_id,
                            &include_graph,
                        )
                        // v1.14.1 single-candidate fallback. Languages without
                        // an include graph (Python, C#, TypeScript, Rust) emit
                        // `Unresolved` for method calls on instances, namespace
                        // members not pulled in by name, etc. — there's no
                        // structured graph to walk for them today. When the
                        // name has **exactly one** definition project-wide,
                        // the resolution is unambiguous and we link it; with
                        // two or more candidates we bail rather than guess
                        // (the Imported arm's first-match-wins is fine for
                        // explicit `using`/`import`, but applying it to
                        // duck-typed method calls would silently mis-attribute
                        // refs). Disambiguating beyond single-candidate
                        // requires type inference, which is out of scope.
                        .or_else(|| {
                            name_to_global
                                .get(r.name.as_str())
                                .filter(|hits| hits.len() == 1)
                                .and_then(|hits| hits.first().copied())
                        })
                    }
                };
                if let Some(global) = to_sym_idx {
                    ref_edge_builders.push(RefEdgeBuilder {
                        to_sym_idx: global,
                        from_file_id: file_id,
                        line: r.line as u32,
                        col: r.col as u32,
                        kind: u8::from(r.kind),
                    });
                }
            }
            // Same loudness rule as the ModuleSymbol path: if a file's
            // symbol count would push the running base past `u32::MAX`
            // we fail tests rather than silently corrupt subsequent
            // resolutions. Unreachable in any realistic repo (the
            // SymbolRecord array would be ~68 GB before this fires).
            debug_assert!(
                base_idx.checked_add(file.symbols.len() as u32).is_some(),
                "base_idx overflow accumulating {} symbols from {}",
                file.symbols.len(),
                file.path,
            );
            base_idx = base_idx.wrapping_add(file.symbols.len() as u32);
        }
    }

    // Phase 11.1.9 (Q4-A) — second pass: re-resolve cross-file ref-edges
    // that were reconstructed from the OLD index for unchanged files.
    // Runs STRICTLY AFTER the per-file loop closes so `name_to_global`
    // and `sym_to_file_id` are fully populated (architect-C1 must-fix).
    //
    // The resolution is path-tiebreak: when `target_name` has multiple
    // global candidates (popular in Java/TS), we pick the candidate
    // whose new file resolves back to the OLD target_path. Falling back
    // to "first candidate" (the Imported arm's semantics) would silently
    // mis-attribute refs across files that share a name. When the
    // target was renamed/deleted in the changed slice we drop silently
    // (debug! not warn!) — this is the Q4-B seam, documented in
    // LIMITATIONS §4.
    if !reconstructed_refs.is_empty() {
        // Inverse of file_ids — file_id → path — for the path-tiebreak
        // lookup. Built once before the second-pass loop.
        let mut file_paths_new: Vec<String> = vec![String::new(); file_ids.len()];
        for (path, &fid) in &file_ids {
            if let Some(slot) = file_paths_new.get_mut(fid as usize) {
                *slot = path.clone();
            }
        }
        let mut dropped_target_missing: u64 = 0;
        let mut dropped_from_missing: u64 = 0;
        for rr in reconstructed_refs {
            // OLD file_id → OLD path (via the reconstruction's saved
            // old_file_paths slice) → NEW file_id (via file_ids).
            let Some(from_path) = old_file_paths.get(rr.from_file_id as usize) else {
                dropped_from_missing += 1;
                tracing::debug!(
                    from_file_id = rr.from_file_id,
                    "reconstructed ref: from_file_id past old file_paths"
                );
                continue;
            };
            let Some(&new_from_file_id) = file_ids.get(from_path) else {
                dropped_from_missing += 1;
                tracing::debug!(
                    from_path,
                    "reconstructed ref: source file dropped from new index"
                );
                continue;
            };
            let resolved = resolve_by_name_and_path(
                &rr.target_name,
                Some(&rr.target_path),
                &name_to_global,
                &sym_to_file_id,
                &file_paths_new,
            );
            let Some(global) = resolved else {
                // Expected drop — target was renamed/deleted in the
                // changed slice. Q4-B will recover via cascade.
                dropped_target_missing += 1;
                tracing::debug!(
                    target_name = %rr.target_name,
                    target_path = %rr.target_path,
                    "reconstructed ref: target not found in new index — dropped (Q4-B will reconcile)"
                );
                continue;
            };
            ref_edge_builders.push(RefEdgeBuilder {
                to_sym_idx: global,
                from_file_id: new_from_file_id,
                line: rr.line,
                col: rr.col,
                kind: u8::from(rr.kind),
            });
        }
        // Surface aggregate drops so `RUST_LOG=vex=info` triagers a
        // degraded `--strict` result set without re-running. Split by
        // cause: "target missing" is the Q4-B seam (changed file
        // renamed/deleted an exported symbol); "from missing" means
        // the source file itself didn't survive into the new index
        // (rare — file deleted between reconstruction read and writer
        // assembly). Counts are per-update, not cumulative.
        if dropped_target_missing > 0 {
            tracing::info!(
                dropped_target_missing,
                "vex update: {dropped_target_missing} cross-file refs lost their target (renamed/deleted in the changed slice); \
                 run `vex index` to fully reconcile (Q4-B follow-up)"
            );
        }
        if dropped_from_missing > 0 {
            tracing::info!(
                dropped_from_missing,
                "vex update: {dropped_from_missing} reconstructed refs dropped because their source file did not survive into the new index"
            );
        }
    }

    let (ref_edge_bytes, ref_edge_fst_bytes, ref_edge_post_bytes) =
        build_ref_edges_section(&ref_edge_builders)?;

    // Build v6 pattern skeleton section (empty slice → all-zero header fields,
    // non-empty → populated sub-sections). The version bump to v6 is
    // unconditional — presence of the header is what gates Inc 5's prefilter.
    let mut no_intern_fn = |_s: &str| -> u32 { 0 };
    let (skel_section, skel_fingerprints) =
        build_pattern_skeleton_section(pattern_skeletons, &mut no_intern_fn, lang_fingerprints)?;

    // Calculate section offsets — v6 places CallGraphHeader, V5SectionHeader,
    // and PatternSkeletonHeader immediately after the base Header, so Symbols
    // starts at:
    //   Header::SIZE + CallGraphHeader::SIZE + V5SectionHeader::SIZE
    //   + PatternSkeletonHeader::SIZE
    let cg_header_offset = Header::SIZE as u64;
    let v5_header_offset = cg_header_offset + CallGraphHeader::SIZE as u64;
    let pat_header_offset = v5_header_offset + V5SectionHeader::SIZE as u64;
    let symbols_offset = pat_header_offset + PatternSkeletonHeader::SIZE as u64;
    let symbols_size = records.len() * SymbolRecord::SIZE;

    let vectors_offset = symbols_offset + symbols_size as u64;
    let vectors_size = if vectors.is_empty() {
        0
    } else {
        vectors.len() * vector_dim as usize * std::mem::size_of::<f32>()
    };

    let strings_offset = vectors_offset + vectors_size as u64;
    let fst_offset = strings_offset + strings.data.len() as u64;
    let postings_offset = fst_offset + fst_bytes.len() as u64;
    let file_table_offset = postings_offset + posting_bytes.len() as u64;
    let file_table_size = file_table.len() * 4;
    let sym_fst_offset = file_table_offset + file_table_size as u64;
    let sym_postings_offset = sym_fst_offset + sym_fst_bytes.len() as u64;

    // Call graph sections come after the v3 sections. Align to 4 bytes so
    // that CallEdge (align_of == 4) can be cast directly from the mmap bytes.
    let call_edges_unaligned = sym_postings_offset + sym_posting_bytes.len() as u64;
    let call_edges_offset = (call_edges_unaligned + 3) & !3u64; // round up to 4-byte boundary
    let _call_edges_pad = (call_edges_offset - call_edges_unaligned) as usize;
    let call_edges_len = (edge_records.len() * CallEdge::SIZE) as u64;
    let callers_fst_offset = call_edges_offset + call_edges_len;
    let callers_postings_offset = callers_fst_offset + callers_fst_bytes.len() as u64;
    let callees_fst_offset = callers_postings_offset + callers_post_bytes.len() as u64;
    let callees_postings_offset = callees_fst_offset + callees_fst_bytes.len() as u64;

    // BM25 sections come after callees postings. No alignment requirement —
    // they're variable-length byte blobs (FST + posting + stats).
    let (bm25_fst, bm25_posts, bm25_stats): (&[u8], &[u8], &[u8]) = bm25.unwrap_or((&[], &[], &[]));
    let bm25_fst_offset = callees_postings_offset + callees_post_bytes.len() as u64;
    let bm25_postings_offset = bm25_fst_offset + bm25_fst.len() as u64;
    let bm25_stats_offset = bm25_postings_offset + bm25_posts.len() as u64;

    // v5 reference_edges sections. Align the edges array to 4 bytes so
    // RefEdge (align_of == 4) can be cast from the mmap.
    let ref_edges_unaligned = bm25_stats_offset + bm25_stats.len() as u64;
    let ref_edges_offset = (ref_edges_unaligned + 3) & !3u64;
    let ref_edges_pad = (ref_edges_offset - ref_edges_unaligned) as usize;
    let ref_edges_len = ref_edge_bytes.len() as u64;
    let ref_edges_fst_offset = ref_edges_offset + ref_edges_len;
    let ref_edges_postings_offset = ref_edges_fst_offset + ref_edge_fst_bytes.len() as u64;

    // v6 pattern skeleton sub-sections. Align skeleton records to 4 bytes so
    // SkeletonRecord (align_of == 4) can be cast from the mmap.
    let skel_unaligned = ref_edges_postings_offset + ref_edge_post_bytes.len() as u64;
    let skel_records_offset = (skel_unaligned + 3) & !3u64;
    let skel_records_pad = (skel_records_offset - skel_unaligned) as usize;
    let skel_records_len = skel_section.skeleton_records.len() as u64;
    let skel_kind_path_offset = skel_records_offset + skel_records_len;
    let skel_kind_path_len = skel_section.kind_path_arena.len() as u64;
    let skel_ident_pool_offset = skel_kind_path_offset + skel_kind_path_len;
    let skel_ident_pool_len = skel_section.ident_pool.len() as u64;
    let skel_file_index_offset = skel_ident_pool_offset + skel_ident_pool_len;
    let skel_file_index_len = skel_section.file_index.len() as u64;

    let pat_skel_header = PatternSkeletonHeader {
        skeletons_offset: skel_records_offset,
        skeletons_len: skel_records_len,
        kind_path_offset: skel_kind_path_offset,
        kind_path_len: skel_kind_path_len,
        ident_pool_offset: skel_ident_pool_offset,
        ident_pool_len: skel_ident_pool_len,
        file_index_offset: skel_file_index_offset,
        file_index_len: skel_file_index_len,
        grammar_fingerprints: skel_fingerprints,
    };

    let call_graph_header = CallGraphHeader {
        call_edges_offset,
        call_edges_len,
        callers_fst_offset,
        callers_fst_len: callers_fst_bytes.len() as u64,
        callers_postings_offset,
        callers_postings_len: callers_post_bytes.len() as u64,
        callees_fst_offset,
        callees_fst_len: callees_fst_bytes.len() as u64,
        callees_postings_offset,
        callees_postings_len: callees_post_bytes.len() as u64,
        bm25_fst_offset,
        bm25_fst_len: bm25_fst.len() as u64,
        bm25_postings_offset,
        bm25_postings_len: bm25_posts.len() as u64,
        bm25_stats_offset,
        bm25_stats_len: bm25_stats.len() as u64,
    };

    let header = Header {
        magic: *MAGIC,
        version: VERSION,
        symbol_count: records.len() as u64,
        vector_dim,
        _padding: 0,
        symbols_offset,
        vectors_offset,
        strings_offset,
        inverted_offset: 0,
        hnsw_offset: 0,
        fst_offset,
        fst_len: fst_bytes.len() as u64,
        postings_offset,
        postings_len: posting_bytes.len() as u64,
        file_table_offset,
        file_table_count: file_table.len() as u32,
        _padding2: 0,
        sym_fst_offset,
        sym_fst_len: sym_fst_bytes.len() as u64,
        sym_postings_offset,
        sym_postings_len: sym_posting_bytes.len() as u64,
    };

    let file = std::fs::File::create(output)?;
    let mut w = BufWriter::new(file);

    // SAFETY: Header is #[repr(C)] with fixed layout, no padding issues on same arch
    let header_bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(&header as *const Header as *const u8, Header::SIZE) };
    w.write_all(header_bytes)?;

    // v4: CallGraphHeader immediately after the base header.
    // SAFETY: CallGraphHeader is #[repr(C)] with fixed layout.
    let cg_header_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            &call_graph_header as *const CallGraphHeader as *const u8,
            CallGraphHeader::SIZE,
        )
    };
    w.write_all(cg_header_bytes)?;

    // v5: V5SectionHeader immediately after the CallGraphHeader.
    // Populated with real offsets when bound_refs produced edges; all
    // zero when there were no ModuleSymbol-resolved refs.
    let v5_header = V5SectionHeader {
        ref_edges_offset,
        ref_edges_len,
        ref_edges_fst_offset,
        ref_edges_fst_len: ref_edge_fst_bytes.len() as u64,
        ref_edges_postings_offset,
        ref_edges_postings_len: ref_edge_post_bytes.len() as u64,
    };
    // SAFETY: V5SectionHeader is #[repr(C)] with fixed layout.
    let v5_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            &v5_header as *const V5SectionHeader as *const u8,
            V5SectionHeader::SIZE,
        )
    };
    w.write_all(v5_bytes)?;

    // v6: PatternSkeletonHeader immediately after V5SectionHeader.
    // When skeletons is empty all offset/len fields are set to their actual
    // (zero-length) positions and the sub-section writes below are no-ops.
    // SAFETY: PatternSkeletonHeader is #[repr(C)] with fixed layout.
    let pat_skel_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            &pat_skel_header as *const PatternSkeletonHeader as *const u8,
            PatternSkeletonHeader::SIZE,
        )
    };
    w.write_all(pat_skel_bytes)?;

    for rec in &records {
        // SAFETY: SymbolRecord is #[repr(C)] with fixed layout
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(rec as *const SymbolRecord as *const u8, SymbolRecord::SIZE)
        };
        w.write_all(bytes)?;
    }

    for vec in vectors.iter() {
        // Length was pre-validated in `write_index_full` before this fn ran.
        debug_assert_eq!(vec.len(), vector_dim as usize);
        // SAFETY: vec is a valid &[f32] with known length
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                vec.as_ptr() as *const u8,
                vec.len() * std::mem::size_of::<f32>(),
            )
        };
        w.write_all(bytes)?;
    }

    w.write_all(&strings.data)?;
    w.write_all(&fst_bytes)?;
    w.write_all(&posting_bytes)?;

    // Write file table
    for &str_offset in &file_table {
        w.write_all(&str_offset.to_le_bytes())?;
    }

    // Write symbol FST + postings
    w.write_all(&sym_fst_bytes)?;
    w.write_all(&sym_posting_bytes)?;

    // Pad to 4-byte alignment before call-graph sections so that CallEdge
    // records (align_of == 4) can be safely cast from the mmap pointer.
    if _call_edges_pad > 0 {
        w.write_all(&[0u8; 3][.._call_edges_pad])?;
    }

    // v4 call-graph sections: edge records + 2 FSTs + 2 posting lists.
    for rec in &edge_records {
        // SAFETY: CallEdge is #[repr(C)] with fixed layout.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(rec as *const CallEdge as *const u8, CallEdge::SIZE)
        };
        w.write_all(bytes)?;
    }
    w.write_all(&callers_fst_bytes)?;
    w.write_all(&callers_post_bytes)?;
    w.write_all(&callees_fst_bytes)?;
    w.write_all(&callees_post_bytes)?;

    // v4 BM25 sections (may be empty slices, which is the right behaviour
    // when bm25 == None — writes nothing, header records 0-length).
    w.write_all(bm25_fst)?;
    w.write_all(bm25_posts)?;
    w.write_all(bm25_stats)?;

    // v5 reference_edges sections, 4-byte aligned (see ref_edges_offset).
    if ref_edges_pad > 0 {
        w.write_all(&[0u8; 3][..ref_edges_pad])?;
    }
    w.write_all(&ref_edge_bytes)?;
    w.write_all(&ref_edge_fst_bytes)?;
    w.write_all(&ref_edge_post_bytes)?;

    // v6 pattern skeleton sub-sections, 4-byte aligned before the records.
    if skel_records_pad > 0 {
        w.write_all(&[0u8; 3][..skel_records_pad])?;
    }
    w.write_all(&skel_section.skeleton_records)?;
    w.write_all(&skel_section.kind_path_arena)?;
    w.write_all(&skel_section.ident_pool)?;
    w.write_all(&skel_section.file_index)?;

    // Flush the BufWriter, then recover the inner File so we can fsync it
    // before the caller atomic-renames. Without sync_all() between flush
    // and rename, a crash/power-loss between rename and writeback can
    // leave the destination pointing at garbage — readers then mmap
    // arbitrary bytes as symbol records / offsets.
    let file = w
        .into_inner()
        .map_err(|e| anyhow::anyhow!("flush index temp file: {}", e.error()))?;
    file.sync_all()
        .context("fsync index temp file before rename")?;
    drop(file);

    let ref_count: usize = parsed.iter().map(|f| f.refs.len()).sum();
    tracing::info!(
        symbols = records.len(),
        refs = ref_count,
        files = file_table.len(),
        fst_bytes = fst_bytes.len(),
        edges = edge_records.len(),
        "index written to {:?}",
        output
    );

    Ok(())
}

/// v1.14 — predicate for the C++ include-graph filter. Uses the same
/// extension → `Language` map the parser uses (`parse::language`), so adding
/// a new C++ extension there propagates here automatically. `.c` is
/// intentionally NOT included — the parser doesn't index plain C files
/// either.
fn is_cpp_path(path: &str) -> bool {
    path.rsplit('.')
        .next()
        .and_then(crate::parse::language::Language::from_extension)
        == Some(crate::parse::language::Language::Cpp)
}
