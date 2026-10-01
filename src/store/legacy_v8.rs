//! Oracle module for the v8 on-disk encodings that the v9 CSR migration
//! (`docs/V9-FORMAT.md`) deleted from production code in P2.
//!
//! P0 (`docs/V9-FORMAT.md` §9/§13 R20) preserved a **faithful copy** of
//! the decimal-string-key encoding the v8 writer used —
//! `build_u32_keyed_fst`, `encode_caller_key` / `encode_caller_key_into`,
//! and `build_ref_edges_fst` — so it keeps working as an oracle now that
//! P2 has deleted those production functions (`call_graph.rs`'s callees
//! FST path and `ref_edges.rs`'s FST path). Production code is untouched
//! by this module; its own on-disk output is unchanged.
//!
//! Used by:
//! - the `csr` module's proptests, to assert that a CSR group equals the
//!   v8 FST posting list element for element;
//! - the P0 golden tests in `tests/legacy_v8_golden_test.rs`, via
//!   [`build_sample_v8_index`] — a programmatic v8-index builder (no
//!   checked-in binary fixture, R20).
//!
//! **P2 deviation from the "drive the writer directly" plan (R20):**
//! `write_index_with_call_graph_and_skeletons_and_fingerprints` now
//! always emits v9 (it IS the production writer; `VERSION == 9`).
//! Rather than thread a hidden `target_version` parameter through that
//! ~1200-line function (and duplicate its offset-chain arithmetic a
//! second time for a test-only code path), [`build_sample_v8_index`]
//! drives the real v9 pipeline and then calls
//! [`downgrade_v9_file_to_v8`], which rewrites the resulting file in
//! place as a byte-for-byte-equivalent v8 file: every section except the
//! two CSR-eligible ones (callees, `ref_edges`) is copied verbatim
//! (their content and internal, pool-relative offsets never depend on
//! the format version); the callees and `ref_edges` FSTs are rebuilt
//! from the v9 file's own `CallEdge` / `RefEdge` records using this
//! module's oracle builders. This is still "programmatic, no checked-in
//! binary" (R20) and is a smaller, more isolated surface than widening
//! the production writer's signature for a test-only shape.
//!
//! `#[doc(hidden)]`: test/bench-only surface, not part of the public API.
//!
//! This module *is* compiled into the release binary too — `main.rs`
//! duplicates `src/` via its own `mod store;` rather than depending on
//! the `vex` library crate, so every item here exists in both
//! compilation units. It is simply never *called* from `main()` or from
//! any production code path in either one; only `#[cfg(test)]` code and
//! `tests/legacy_v8_golden_test.rs` call it. That makes it dead code by
//! rustc's reachability analysis, which is exactly the same situation as
//! the `__fuzz_*` shims (e.g. `store::rename_chains::__fuzz_rename_chains_bytes`):
//! always compiled, `doc(hidden)`, kept alive only for tests/fuzzing.
//! The module-wide allow suppresses the resulting warning, mirroring
//! `parse/scope/mod.rs` and `index/rename_chains/mod.rs`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Zero-padded 10-digit decimal key encoder. Byte-identical to
/// `call_graph::encode_caller_key` (callees) and to
/// `ref_edges::encode_to_sym_key` (ref_edges), which used the same
/// format — kept as a single copy here since the oracle needs both.
pub fn encode_caller_key(n: u32) -> String {
    format!("{n:010}")
}

/// `ref_edges::encode_to_sym_key`. The v8 `ref_edges` FST used the exact
/// same 10-digit zero-padded encoding as the callees FST's caller key;
/// kept as a distinct name so oracle call sites read like the functions
/// they stand in for.
pub fn encode_to_sym_key(to_sym_idx: u32) -> String {
    encode_caller_key(to_sym_idx)
}

/// Stack-buffer encoder — exact copy of
/// `call_graph::encode_caller_key_into`. Same byte output as
/// [`encode_caller_key`] without the per-key `String` allocation.
pub fn encode_caller_key_into(buf: &mut [u8; 10], mut n: u32) {
    for slot in buf.iter_mut().rev() {
        *slot = b'0' + (n % 10) as u8;
        n /= 10;
    }
}

/// u32-keyed `Vec` → sorted FST — exact copy of
/// `call_graph::build_u32_keyed_fst`. This is the callees-FST oracle:
/// builds the v8 on-disk shape from `(key, edge_idx)` pairs so tests can
/// assert the CSR module's per-key groups equal these posting lists
/// element for element (F4: ascending edge_idx within a group).
pub fn build_u32_keyed_fst(mut entries: Vec<(u32, u32)>) -> Result<(Vec<u8>, Vec<u8>)> {
    entries.sort_unstable();

    let mut posting_data: Vec<u8> = Vec::with_capacity(entries.len() * 4 + entries.len());
    let mut fst_builder = fst::MapBuilder::memory();
    let mut key_buf = [b'0'; 10];

    let mut i = 0;
    while i < entries.len() {
        let key = entries[i].0;
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == key {
            j += 1;
        }
        let offset = posting_data.len() as u64;
        let group = &mut entries[i..j];
        // Dedup identical (key, edge_idx) pairs across the contiguous
        // group — mirrors the production builder exactly.
        let mut write = 0;
        for read in 0..group.len() {
            if write == 0 || group[read].1 != group[write - 1].1 {
                group.swap(read, write);
                write += 1;
            }
        }
        let count = write as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for slot in group.iter().take(write) {
            posting_data.extend_from_slice(&slot.1.to_le_bytes());
        }
        encode_caller_key_into(&mut key_buf, key);
        fst_builder
            .insert(key_buf, offset)
            .context("fst insert (legacy_v8 callees oracle)")?;
        i = j;
    }

    let fst_bytes = fst_builder
        .into_inner()
        .context("finalize legacy_v8 callees-fst oracle")?;
    Ok((fst_bytes, posting_data))
}

/// `ref_edges::build_ref_edges_section`'s FST-building portion — exact
/// copy of the loop that turns already-sorted `(to_sym_idx, edge_idx)`
/// pairs (F5: the writer sorts `RefEdge` records by `to_sym_idx`, so
/// entries arrive pre-grouped, no extra sort needed here either) into
/// `(fst_bytes, posting_bytes)`. No dedup: v8 ref_edges postings are
/// never deduped — each `edge_idx` is the unique position of its record.
pub fn build_ref_edges_fst(entries: &[(u32, u32)]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut posting_data: Vec<u8> = Vec::new();
    let mut fst_builder = fst::MapBuilder::memory();
    let mut key_buf = [b'0'; 10];

    let mut i = 0;
    while i < entries.len() {
        let key = entries[i].0;
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == key {
            j += 1;
        }
        let offset = posting_data.len() as u64;
        let count = (j - i) as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for slot in &entries[i..j] {
            posting_data.extend_from_slice(&slot.1.to_le_bytes());
        }
        encode_caller_key_into(&mut key_buf, key);
        fst_builder
            .insert(key_buf, offset)
            .context("fst insert (legacy_v8 ref-edges oracle)")?;
        i = j;
    }

    let fst_bytes = fst_builder
        .into_inner()
        .context("finalize legacy_v8 ref-edges-fst oracle")?;
    Ok((fst_bytes, posting_data))
}

/// Zero-copy reader over an oracle FST — the same shape and behaviour as
/// `call_graph::CallGraphFstReader`, kept as its own copy so this module
/// has no dependency on code a later phase may delete.
pub struct FstOracleReader<'a> {
    fst_map: fst::Map<&'a [u8]>,
    posting_data: &'a [u8],
}

impl<'a> FstOracleReader<'a> {
    pub fn new(fst_bytes: &'a [u8], posting_bytes: &'a [u8]) -> Result<Self> {
        let fst_map = fst::Map::new(fst_bytes)
            .map_err(|e| anyhow::anyhow!("fst load (legacy_v8 oracle): {e}"))?;
        Ok(Self {
            fst_map,
            posting_data: posting_bytes,
        })
    }

    /// Posting list for a zero-padded 10-digit decimal key. Empty when
    /// the key is absent or the posting bytes are truncated.
    pub fn find_decimal_key(&self, n: u32) -> Vec<u32> {
        let key = encode_caller_key(n);
        match self.fst_map.get(key.as_bytes()) {
            Some(offset) => self.read_posting_list(offset),
            None => Vec::new(),
        }
    }

    fn read_posting_list(&self, offset: u64) -> Vec<u32> {
        let offset = offset as usize;
        if offset + 4 > self.posting_data.len() {
            return Vec::new();
        }
        let count = u32::from_le_bytes(
            self.posting_data[offset..offset + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ) as usize;
        let max_entries = self.posting_data.len().saturating_sub(offset + 4) / 4;
        let mut out = Vec::with_capacity(count.min(max_entries));
        let mut pos = offset + 4;
        for _ in 0..count {
            if pos + 4 > self.posting_data.len() {
                break;
            }
            let idx =
                u32::from_le_bytes(self.posting_data[pos..pos + 4].try_into().unwrap_or([0; 4]));
            out.push(idx);
            pos += 4;
        }
        out
    }
}

// ---------------------------------------------------------------------
// R20: programmatic v8-index builder for library-level golden tests.
// ---------------------------------------------------------------------

/// Writes a tiny multi-file Rust project under `project_root` and runs
/// the real indexing pipeline over it, producing a genuine v8 index —
/// no checked-in binary fixture (R20). `project_root` must already
/// exist and, on macOS, should be pre-canonicalized by the caller (the
/// pipeline and `util::config::index_path` must agree on the canonical
/// path — see the cache-path writer/reader-symmetry note).
///
/// Layout, chosen so callees, callers and `ref_edges` (the `--strict`
/// binder-resolved refs) are all non-empty:
/// - `src/a.rs` defines `helper_fn`.
/// - `src/b.rs` has `use crate::a::helper_fn;` and defines `caller_fn`, which
///   calls it — a cross-file, binder-resolved (`Imported`) reference,
///   plus a name-based call-graph edge.
/// - `src/lib.rs` declares `pub mod a; pub mod b;` — the Rust binder
///   needs the module tree declared to resolve `crate::a::helper_fn`
///   cross-file (see `tests/incremental_consistency_ref_edges.rs`).
///
/// Returns the index path (`util::config::index_path(project_root)`).
pub fn build_sample_v8_index(project_root: &Path) -> Result<PathBuf> {
    let index_path = build_sample_v9_index(project_root)?;
    downgrade_v9_file_to_v8(&index_path).context("downgrade freshly-built v9 index to v8")?;
    Ok(index_path)
}

/// Same fixture project as [`build_sample_v8_index`], but returns the
/// genuine v9 index the pipeline produces today — no downgrade. Used by
/// the v9 side of the golden-answer parity tests (`tests/legacy_v8_golden_test.rs`):
/// callees / callers / ref_edges must answer identically on both formats.
pub fn build_sample_v9_index(project_root: &Path) -> Result<PathBuf> {
    let src_dir = project_root.join("src");
    std::fs::create_dir_all(&src_dir).context("create src dir")?;
    std::fs::write(
        src_dir.join("a.rs"),
        "pub fn helper_fn() -> i32 {\n    42\n}\n",
    )
    .context("write a.rs")?;
    std::fs::write(
        src_dir.join("b.rs"),
        "use crate::a::helper_fn;\n\npub fn caller_fn() -> i32 {\n    helper_fn()\n}\n",
    )
    .context("write b.rs")?;
    std::fs::write(src_dir.join("lib.rs"), "pub mod a;\npub mod b;\n").context("write lib.rs")?;

    crate::index::pipeline::run(
        project_root,
        crate::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .context("run indexing pipeline")?;

    Ok(crate::util::config::index_path(project_root))
}

/// Rewrite a v9 index file in place as a byte-for-byte-equivalent v8
/// file (see the module doc for why this exists instead of a writer
/// `target_version` parameter). No `unsafe`: every header field is
/// decoded via the already-vetted [`crate::store::reader::IndexReader`]
/// accessors and re-encoded with plain `to_le_bytes` pushes, matching
/// the `code-conventions.md` rule that only `store/reader.rs` and
/// `store/writer.rs` contain `unsafe`.
pub fn downgrade_v9_file_to_v8(index_path: &Path) -> Result<()> {
    use crate::store::format::{
        CallGraphHeader, Header, HierarchyHeader, PatternSkeletonHeader, UnresolvedHierarchyHeader,
        UnresolvedRefsHeader, V5SectionHeader,
    };
    use crate::store::reader::IndexReader;

    let data = std::fs::read(index_path).context("read v9 index for downgrade")?;
    let reader = IndexReader::open(index_path).context("open v9 index for downgrade")?;
    let header = reader.header();
    anyhow::ensure!(
        header.version == 9,
        "downgrade_v9_file_to_v8: expected a v9 file, found v{}",
        header.version
    );

    let cg = reader
        .call_graph_header()
        .context("v9 file missing CallGraphHeader")?;
    let v5 = reader
        .v5_section_header()
        .context("v9 file missing V5SectionHeader")?;
    let pat = reader
        .pattern_skeleton_header()
        .context("v9 file missing PatternSkeletonHeader")?;
    let unres = reader
        .unresolved_refs_header()
        .context("v9 file missing UnresolvedRefsHeader")?;
    let hier = reader
        .hierarchy_header()
        .context("v9 file missing HierarchyHeader")?;
    let unres_hier = reader
        .unresolved_hierarchy_header()
        .context("v9 file missing UnresolvedHierarchyHeader")?;

    // Every section except callees/ref_edges is copied verbatim — content
    // and pool-relative offsets never depend on the format version.
    let symbols_bytes = byte_slice(
        &data,
        header.symbols_offset,
        header
            .symbol_count
            .saturating_mul(crate::store::format::SymbolRecord::SIZE as u64),
    )?;
    let vectors_bytes = byte_slice(
        &data,
        header.vectors_offset,
        header.strings_offset.saturating_sub(header.vectors_offset),
    )?;
    let strings_bytes = byte_slice(
        &data,
        header.strings_offset,
        header.fst_offset.saturating_sub(header.strings_offset),
    )?;
    let refs_fst_bytes = byte_slice(&data, header.fst_offset, header.fst_len)?;
    let refs_postings_bytes = byte_slice(&data, header.postings_offset, header.postings_len)?;
    let file_table_bytes = byte_slice(
        &data,
        header.file_table_offset,
        (header.file_table_count as u64).saturating_mul(4),
    )?;
    let sym_fst_bytes = byte_slice(&data, header.sym_fst_offset, header.sym_fst_len)?;
    let sym_postings_bytes =
        byte_slice(&data, header.sym_postings_offset, header.sym_postings_len)?;
    let call_edges_bytes = byte_slice(&data, cg.call_edges_offset, cg.call_edges_len)?;
    let callers_fst_bytes = byte_slice(&data, cg.callers_fst_offset, cg.callers_fst_len)?;
    let callers_postings_bytes =
        byte_slice(&data, cg.callers_postings_offset, cg.callers_postings_len)?;
    let bm25_fst_bytes = byte_slice(&data, cg.bm25_fst_offset, cg.bm25_fst_len)?;
    let bm25_postings_bytes = byte_slice(&data, cg.bm25_postings_offset, cg.bm25_postings_len)?;
    let bm25_stats_bytes = byte_slice(&data, cg.bm25_stats_offset, cg.bm25_stats_len)?;
    let ref_edges_record_bytes = byte_slice(&data, v5.ref_edges_offset, v5.ref_edges_len)?;
    let skel_records_bytes = byte_slice(&data, pat.skeletons_offset, pat.skeletons_len)?;
    let skel_kind_path_bytes = byte_slice(&data, pat.kind_path_offset, pat.kind_path_len)?;
    let skel_ident_pool_bytes = byte_slice(&data, pat.ident_pool_offset, pat.ident_pool_len)?;
    let skel_file_index_bytes = byte_slice(&data, pat.file_index_offset, pat.file_index_len)?;
    let unresolved_edge_bytes = byte_slice(
        &data,
        unres.unresolved_edges_offset,
        unres.unresolved_edges_len,
    )?;
    let unresolved_fst_bytes =
        byte_slice(&data, unres.unresolved_fst_offset, unres.unresolved_fst_len)?;
    let unresolved_postings_bytes = byte_slice(
        &data,
        unres.unresolved_postings_offset,
        unres.unresolved_postings_len,
    )?;
    let hierarchy_edge_bytes = byte_slice(&data, hier.edges_offset, hier.edges_len)?;
    let hierarchy_index_bytes = byte_slice(&data, hier.index_offset, hier.index_len)?;
    let hierarchy_postings_bytes = byte_slice(&data, hier.postings_offset, hier.postings_len)?;
    let unresolved_hier_edge_bytes =
        byte_slice(&data, unres_hier.edges_offset, unres_hier.edges_len)?;
    let unresolved_hier_fst_bytes = byte_slice(&data, unres_hier.fst_offset, unres_hier.fst_len)?;
    let unresolved_hier_postings_bytes =
        byte_slice(&data, unres_hier.postings_offset, unres_hier.postings_len)?;

    // Rebuild the two CSR-eligible sections as v8 decimal-FSTs from the
    // v9 file's own (byte-identical) CallEdge / RefEdge records.
    let callees_entries: Vec<(u32, u32)> = (0..reader.call_edge_count())
        .filter_map(|i| reader.call_edge(i).map(|e| (e.caller_sym_idx, i as u32)))
        .collect();
    let (callees_fst_bytes, callees_post_bytes) = build_u32_keyed_fst(callees_entries)?;
    let ref_entries: Vec<(u32, u32)> = (0..reader.ref_edge_count())
        .filter_map(|i| reader.ref_edge(i).map(|e| (e.to_sym_idx, i as u32)))
        .collect();
    let (ref_fst_bytes, ref_post_bytes) = build_ref_edges_fst(&ref_entries)?;

    // v8 layout: no ClusterHeader, so the chain ends at 656 bytes.
    let symbols_offset_v8 = (Header::SIZE
        + CallGraphHeader::SIZE
        + V5SectionHeader::SIZE
        + PatternSkeletonHeader::SIZE
        + UnresolvedRefsHeader::SIZE
        + HierarchyHeader::SIZE
        + UnresolvedHierarchyHeader::SIZE) as u64;
    debug_assert_eq!(symbols_offset_v8, 656, "v8 symbols_offset is pinned at 656");

    let vectors_offset = symbols_offset_v8 + symbols_bytes.len() as u64;
    let strings_offset = vectors_offset + vectors_bytes.len() as u64;
    let fst_offset = strings_offset + strings_bytes.len() as u64;
    let postings_offset = fst_offset + refs_fst_bytes.len() as u64;
    let file_table_offset = postings_offset + refs_postings_bytes.len() as u64;
    let sym_fst_offset = file_table_offset + file_table_bytes.len() as u64;
    let sym_postings_offset = sym_fst_offset + sym_fst_bytes.len() as u64;

    let call_edges_unaligned = sym_postings_offset + sym_postings_bytes.len() as u64;
    let call_edges_offset = (call_edges_unaligned + 3) & !3u64;
    let call_edges_pad = (call_edges_offset - call_edges_unaligned) as usize;
    let call_edges_len = call_edges_bytes.len() as u64;
    let callers_fst_offset = call_edges_offset + call_edges_len;
    let callers_postings_offset = callers_fst_offset + callers_fst_bytes.len() as u64;
    let callees_fst_offset = callers_postings_offset + callers_postings_bytes.len() as u64;
    let callees_postings_offset = callees_fst_offset + callees_fst_bytes.len() as u64;
    let bm25_fst_offset = callees_postings_offset + callees_post_bytes.len() as u64;
    let bm25_postings_offset = bm25_fst_offset + bm25_fst_bytes.len() as u64;
    let bm25_stats_offset = bm25_postings_offset + bm25_postings_bytes.len() as u64;

    let ref_edges_unaligned = bm25_stats_offset + bm25_stats_bytes.len() as u64;
    let ref_edges_offset = (ref_edges_unaligned + 3) & !3u64;
    let ref_edges_pad = (ref_edges_offset - ref_edges_unaligned) as usize;
    let ref_edges_len = ref_edges_record_bytes.len() as u64;
    let ref_edges_fst_offset = ref_edges_offset + ref_edges_len;
    let ref_edges_postings_offset = ref_edges_fst_offset + ref_fst_bytes.len() as u64;

    let skel_unaligned = ref_edges_postings_offset + ref_post_bytes.len() as u64;
    let skel_records_offset = (skel_unaligned + 3) & !3u64;
    let skel_records_pad = (skel_records_offset - skel_unaligned) as usize;
    let skel_records_len = skel_records_bytes.len() as u64;
    let skel_kind_path_offset = skel_records_offset + skel_records_len;
    let skel_kind_path_len = skel_kind_path_bytes.len() as u64;
    let skel_ident_pool_offset = skel_kind_path_offset + skel_kind_path_len;
    let skel_ident_pool_len = skel_ident_pool_bytes.len() as u64;
    let skel_file_index_offset = skel_ident_pool_offset + skel_ident_pool_len;
    let skel_file_index_len = skel_file_index_bytes.len() as u64;

    let unresolved_unaligned = skel_file_index_offset + skel_file_index_len;
    let unresolved_edges_offset = (unresolved_unaligned + 3) & !3u64;
    let unresolved_edges_pad = (unresolved_edges_offset - unresolved_unaligned) as usize;
    let unresolved_edges_len = unresolved_edge_bytes.len() as u64;
    let unresolved_fst_offset = unresolved_edges_offset + unresolved_edges_len;
    let unresolved_postings_offset = unresolved_fst_offset + unresolved_fst_bytes.len() as u64;

    let hierarchy_unaligned = unresolved_postings_offset + unresolved_postings_bytes.len() as u64;
    let hierarchy_edges_offset = (hierarchy_unaligned + 3) & !3u64;
    let hierarchy_edges_pad = (hierarchy_edges_offset - hierarchy_unaligned) as usize;
    let hierarchy_edges_len = hierarchy_edge_bytes.len() as u64;
    let hierarchy_index_offset = hierarchy_edges_offset + hierarchy_edges_len;
    let hierarchy_index_len = hierarchy_index_bytes.len() as u64;
    let hierarchy_postings_offset = hierarchy_index_offset + hierarchy_index_len;

    let unresolved_hier_unaligned =
        hierarchy_postings_offset + hierarchy_postings_bytes.len() as u64;
    let unresolved_hier_edges_offset = (unresolved_hier_unaligned + 3) & !3u64;
    let unresolved_hier_edges_pad =
        (unresolved_hier_edges_offset - unresolved_hier_unaligned) as usize;
    let unresolved_hier_edges_len = unresolved_hier_edge_bytes.len() as u64;
    let unresolved_hier_fst_offset = unresolved_hier_edges_offset + unresolved_hier_edges_len;
    let unresolved_hier_postings_offset =
        unresolved_hier_fst_offset + unresolved_hier_fst_bytes.len() as u64;

    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    push_u8s(&mut out, &header.magic);
    push_u32(&mut out, 8); // version
    push_u64(&mut out, header.symbol_count);
    push_u32(&mut out, header.vector_dim);
    push_u32(&mut out, 0); // _padding
    push_u64(&mut out, symbols_offset_v8);
    push_u64(&mut out, vectors_offset);
    push_u64(&mut out, strings_offset);
    push_u64(&mut out, 0); // inverted_offset
    push_u64(&mut out, 0); // hnsw_offset
    push_u64(&mut out, fst_offset);
    push_u64(&mut out, refs_fst_bytes.len() as u64);
    push_u64(&mut out, postings_offset);
    push_u64(&mut out, refs_postings_bytes.len() as u64);
    push_u64(&mut out, file_table_offset);
    push_u32(&mut out, header.file_table_count);
    push_u32(&mut out, 0); // _padding2
    push_u64(&mut out, sym_fst_offset);
    push_u64(&mut out, sym_fst_bytes.len() as u64);
    push_u64(&mut out, sym_postings_offset);
    push_u64(&mut out, sym_postings_bytes.len() as u64);
    debug_assert_eq!(out.len(), Header::SIZE);

    push_u64(&mut out, call_edges_offset);
    push_u64(&mut out, call_edges_len);
    push_u64(&mut out, callers_fst_offset);
    push_u64(&mut out, callers_fst_bytes.len() as u64);
    push_u64(&mut out, callers_postings_offset);
    push_u64(&mut out, callers_postings_bytes.len() as u64);
    push_u64(&mut out, callees_fst_offset);
    push_u64(&mut out, callees_fst_bytes.len() as u64);
    push_u64(&mut out, callees_postings_offset);
    push_u64(&mut out, callees_post_bytes.len() as u64);
    push_u64(&mut out, bm25_fst_offset);
    push_u64(&mut out, bm25_fst_bytes.len() as u64);
    push_u64(&mut out, bm25_postings_offset);
    push_u64(&mut out, bm25_postings_bytes.len() as u64);
    push_u64(&mut out, bm25_stats_offset);
    push_u64(&mut out, bm25_stats_bytes.len() as u64);
    debug_assert_eq!(out.len(), Header::SIZE + CallGraphHeader::SIZE);

    push_u64(&mut out, ref_edges_offset);
    push_u64(&mut out, ref_edges_len);
    push_u64(&mut out, ref_edges_fst_offset);
    push_u64(&mut out, ref_fst_bytes.len() as u64);
    push_u64(&mut out, ref_edges_postings_offset);
    push_u64(&mut out, ref_post_bytes.len() as u64);
    debug_assert_eq!(
        out.len(),
        Header::SIZE + CallGraphHeader::SIZE + V5SectionHeader::SIZE
    );

    push_u64(&mut out, skel_records_offset);
    push_u64(&mut out, skel_records_len);
    push_u64(&mut out, skel_kind_path_offset);
    push_u64(&mut out, skel_kind_path_len);
    push_u64(&mut out, skel_ident_pool_offset);
    push_u64(&mut out, skel_ident_pool_len);
    push_u64(&mut out, skel_file_index_offset);
    push_u64(&mut out, skel_file_index_len);
    for fp in pat.grammar_fingerprints {
        push_u32(&mut out, fp);
    }
    debug_assert_eq!(
        out.len(),
        Header::SIZE + CallGraphHeader::SIZE + V5SectionHeader::SIZE + PatternSkeletonHeader::SIZE
    );

    push_u64(&mut out, unresolved_edges_offset);
    push_u64(&mut out, unresolved_edges_len);
    push_u64(&mut out, unresolved_fst_offset);
    push_u64(&mut out, unresolved_fst_bytes.len() as u64);
    push_u64(&mut out, unresolved_postings_offset);
    push_u64(&mut out, unresolved_postings_bytes.len() as u64);

    push_u64(&mut out, hierarchy_edges_offset);
    push_u64(&mut out, hierarchy_edges_len);
    push_u64(&mut out, hierarchy_index_offset);
    push_u64(&mut out, hierarchy_index_len);
    push_u64(&mut out, hierarchy_postings_offset);
    push_u64(&mut out, hierarchy_postings_bytes.len() as u64);

    push_u64(&mut out, unresolved_hier_edges_offset);
    push_u64(&mut out, unresolved_hier_edges_len);
    push_u64(&mut out, unresolved_hier_fst_offset);
    push_u64(&mut out, unresolved_hier_fst_bytes.len() as u64);
    push_u64(&mut out, unresolved_hier_postings_offset);
    push_u64(&mut out, unresolved_hier_postings_bytes.len() as u64);
    debug_assert_eq!(out.len(), symbols_offset_v8 as usize);

    out.extend_from_slice(symbols_bytes);
    out.extend_from_slice(vectors_bytes);
    out.extend_from_slice(strings_bytes);
    out.extend_from_slice(refs_fst_bytes);
    out.extend_from_slice(refs_postings_bytes);
    out.extend_from_slice(file_table_bytes);
    out.extend_from_slice(sym_fst_bytes);
    out.extend_from_slice(sym_postings_bytes);
    out.extend(std::iter::repeat_n(0u8, call_edges_pad));
    out.extend_from_slice(call_edges_bytes);
    out.extend_from_slice(callers_fst_bytes);
    out.extend_from_slice(callers_postings_bytes);
    out.extend_from_slice(&callees_fst_bytes);
    out.extend_from_slice(&callees_post_bytes);
    out.extend_from_slice(bm25_fst_bytes);
    out.extend_from_slice(bm25_postings_bytes);
    out.extend_from_slice(bm25_stats_bytes);
    out.extend(std::iter::repeat_n(0u8, ref_edges_pad));
    out.extend_from_slice(ref_edges_record_bytes);
    out.extend_from_slice(&ref_fst_bytes);
    out.extend_from_slice(&ref_post_bytes);
    out.extend(std::iter::repeat_n(0u8, skel_records_pad));
    out.extend_from_slice(skel_records_bytes);
    out.extend_from_slice(skel_kind_path_bytes);
    out.extend_from_slice(skel_ident_pool_bytes);
    out.extend_from_slice(skel_file_index_bytes);
    out.extend(std::iter::repeat_n(0u8, unresolved_edges_pad));
    out.extend_from_slice(unresolved_edge_bytes);
    out.extend_from_slice(unresolved_fst_bytes);
    out.extend_from_slice(unresolved_postings_bytes);
    out.extend(std::iter::repeat_n(0u8, hierarchy_edges_pad));
    out.extend_from_slice(hierarchy_edge_bytes);
    out.extend_from_slice(hierarchy_index_bytes);
    out.extend_from_slice(hierarchy_postings_bytes);
    out.extend(std::iter::repeat_n(0u8, unresolved_hier_edges_pad));
    out.extend_from_slice(unresolved_hier_edge_bytes);
    out.extend_from_slice(unresolved_hier_fst_bytes);
    out.extend_from_slice(unresolved_hier_postings_bytes);

    drop(reader); // close the mmap before overwriting the file
    let mut tmp_os = index_path.as_os_str().to_owned();
    tmp_os.push(".v8downgrade.tmp");
    let tmp_path = PathBuf::from(tmp_os);
    std::fs::write(&tmp_path, &out).context("write downgraded v8 index")?;
    std::fs::rename(&tmp_path, index_path).context("rename downgraded v8 index into place")?;
    Ok(())
}

fn byte_slice(data: &[u8], offset: u64, len: u64) -> Result<&[u8]> {
    let start = usize::try_from(offset).context("offset overflows usize")?;
    let len = usize::try_from(len).context("len overflows usize")?;
    let end = start
        .checked_add(len)
        .context("section end overflows usize")?;
    data.get(start..end).with_context(|| {
        format!(
            "section [{start}..{end}) out of bounds (file is {} bytes)",
            data.len()
        )
    })
}

fn push_u8s(buf: &mut Vec<u8>, v: &[u8]) {
    buf.extend_from_slice(v);
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------
    // P2 note: the production-parity tests that used to live here
    // (`encode_caller_key_matches_production`,
    // `build_u32_keyed_fst_matches_build_callees_fst`,
    // `build_ref_edges_fst_matches_production`, and the two parity
    // proptests) imported `call_graph::build_callees_fst` /
    // `encode_caller_key` / `encode_caller_key_into` and
    // `ref_edges::build_ref_edges_section` — all deleted from
    // production in the v9 CSR migration. There is nothing left to
    // compare the oracle against, so these tests are gone too (per
    // `docs/V9-FORMAT.md` §9's P2 note). The CSR-vs-oracle equivalence
    // proptests in `store::csr` (which compare the oracle against the
    // NEW `csr::build_csr` / `build_csr_offsets_sorted`) survive P2
    // unchanged.
    // -------------------------------------------------------------

    #[test]
    fn oracle_encode_caller_key_is_zero_padded_decimal() {
        assert_eq!(encode_caller_key(42), "0000000042");
        assert_eq!(encode_caller_key(0), "0000000000");
        assert_eq!(encode_caller_key(u32::MAX), "4294967295");
    }

    #[test]
    fn oracle_encode_caller_key_into_matches_encode_caller_key() {
        for n in [0u32, 1, 9, 10, 12345, u32::MAX] {
            let mut buf = [b'0'; 10];
            encode_caller_key_into(&mut buf, n);
            assert_eq!(std::str::from_utf8(&buf).unwrap(), encode_caller_key(n));
        }
    }

    #[test]
    fn oracle_build_u32_keyed_fst_groups_by_key() {
        let entries = vec![(5, 0), (5, 1), (7, 2)];
        let (fst, posts) = build_u32_keyed_fst(entries).unwrap();
        let reader = FstOracleReader::new(&fst, &posts).unwrap();
        assert_eq!(reader.find_decimal_key(5), vec![0, 1]);
        assert_eq!(reader.find_decimal_key(7), vec![2]);
        assert!(reader.find_decimal_key(99).is_empty());
    }

    #[test]
    fn oracle_build_ref_edges_fst_no_dedup() {
        // Unlike build_u32_keyed_fst, this builder never dedups — each
        // edge_idx is the unique position of its record.
        let entries = vec![(2, 0), (2, 1), (4, 2)];
        let (fst, posts) = build_ref_edges_fst(&entries).unwrap();
        let reader = FstOracleReader::new(&fst, &posts).unwrap();
        assert_eq!(reader.find_decimal_key(2), vec![0, 1]);
        assert_eq!(reader.find_decimal_key(4), vec![2]);
    }

    // -------------------------------------------------------------
    // R20: `build_sample_v8_index` drives the real (v9) pipeline, then
    // `downgrade_v9_file_to_v8` rewrites the file as v8 in place. Both
    // go through `util::config`'s process-global cache resolver, so
    // they're exercised at library level in
    // `tests/legacy_v8_golden_test.rs` (its own test binary / process)
    // rather than here — a `#[cfg(test)]` unit test in this file shares
    // a process with every other `--lib` unit test, and a second
    // `set_cache_override` call anywhere in that process is a silent
    // no-op (`OnceLock`), which would make this test's behaviour depend
    // on test execution order. `downgrade_v9_file_to_v8`'s own
    // version-gate (non-v9 input) is covered directly below instead,
    // using a plain `write_index_with_call_graph` fixture with no cache
    // involvement.
    // -------------------------------------------------------------

    #[test]
    fn downgrade_rejects_a_non_v9_file() {
        use crate::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};

        let parsed = vec![ParsedFile {
            path: "a.rs".to_string(),
            symbols: vec![ParsedSymbol {
                name: "foo".to_string(),
                kind: SymbolKind::Function,
                line: 1,
                signature: None,
                doc: None,
                body_tokens: None,
            }],
            refs: Vec::new(),
            call_edges: Vec::new(),
            bound_refs: Vec::new(),
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
            hierarchy_captures: Vec::new(),
        }];
        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("index.vex");
        crate::store::writer::write_index_full(
            &parsed,
            &[],
            crate::store::format::VECTOR_DIM,
            &out,
        )
        .expect("write v9 index");

        // The freshly-written file IS v9 (today's writer) — this proves
        // the gate fires on a genuinely wrong version, not just "any
        // file downgrade_v9_file_to_v8 is handed". Corrupt the on-disk
        // version byte to something else and assert the gate still
        // rejects it with a clear message.
        let mut bytes = std::fs::read(&out).unwrap();
        // Header: magic[4] then version (u32 LE) at byte offset 4.
        bytes[4..8].copy_from_slice(&8u32.to_le_bytes());
        std::fs::write(&out, &bytes).unwrap();

        let err = downgrade_v9_file_to_v8(&out).unwrap_err();
        assert!(
            err.to_string().contains("expected a v9 file"),
            "unexpected error: {err}"
        );
    }
}
