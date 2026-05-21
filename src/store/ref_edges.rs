//! `reference_edges` section construction and zero-copy reader (11.1.3b).
//!
//! At write time the pipeline collects [`RefEdgeBuilder`] records from
//! every parsed file's `bound_refs`, sorts them by `(to_sym_idx,
//! from_file_id, line)`, serialises them into the on-disk record array,
//! and builds an FST keyed on the stringified `to_sym_idx`. The FST
//! value is the byte offset into a posting-list blob, where each entry
//! is `[u32 count][u32 edge_idx; count]` — the indices point back into
//! the on-disk `RefEdge` array.
//!
//! This mirrors the shape of `call_graph.rs::build_callees_fst` so the
//! reader code can lean on the same posting-list invariants.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use super::format::RefEdge;

/// Input record for [`build_ref_edges_section`]. The writer assembles
/// these from every `ParsedFile.bound_refs`, resolving file-local
/// `ModuleSymbol` targets into global symbol indices before passing
/// them here. `kind` is the [`crate::parse::scope::RefKind`]
/// discriminant as a `u8`.
#[derive(Debug, Clone)]
pub struct RefEdgeBuilder {
    pub to_sym_idx: u32,
    pub from_file_id: u32,
    pub line: u32,
    pub col: u32,
    pub kind: u8,
}

/// Build the `reference_edges` section bytes: `(edges, fst, postings)`.
///
/// Edges are sorted by `(to_sym_idx, from_file_id, line)` so the FST
/// posting lists are dense ranges in the on-disk records — good for
/// cache locality when `vex usages --strict` walks every edge for a
/// given symbol.
pub fn build_ref_edges_section(edges: &[RefEdgeBuilder]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    if edges.is_empty() {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }

    let mut sorted: Vec<&RefEdgeBuilder> = edges.iter().collect();
    sorted.sort_by_key(|e| (e.to_sym_idx, e.from_file_id, e.line, e.col));

    let mut edge_bytes: Vec<u8> = Vec::with_capacity(sorted.len() * RefEdge::SIZE);
    let mut grouped: BTreeMap<String, Vec<u32>> = BTreeMap::new();

    for (idx, e) in sorted.iter().enumerate() {
        // 24-bit column ceiling — unreachable in real source files (no
        // line is 16 M columns wide) but a future caller could pass an
        // arbitrary `u32`. Catch it loudly in tests rather than silently
        // truncating to garbage in production.
        debug_assert!(
            e.col <= 0x00FF_FFFF,
            "column {} exceeds the 24-bit RefEdge encoding",
            e.col
        );
        let col_and_kind = (u32::from(e.kind) << 24) | (e.col & 0x00FF_FFFF);
        let rec = RefEdge {
            to_sym_idx: e.to_sym_idx,
            from_file_id: e.from_file_id,
            line: e.line,
            col_and_kind,
        };
        // SAFETY: RefEdge is #[repr(C)] with fixed 16-byte layout.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&rec as *const RefEdge as *const u8, RefEdge::SIZE)
        };
        edge_bytes.extend_from_slice(bytes);

        grouped
            .entry(encode_to_sym_key(e.to_sym_idx))
            .or_default()
            .push(idx as u32);
    }

    let mut posting_data: Vec<u8> = Vec::new();
    let mut fst_builder = fst::MapBuilder::memory();
    for (key, edge_indices) in grouped.iter() {
        let offset = posting_data.len() as u64;
        let count = edge_indices.len() as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for &idx in edge_indices.iter() {
            posting_data.extend_from_slice(&idx.to_le_bytes());
        }
        fst_builder
            .insert(key.as_bytes(), offset)
            .context("fst insert (ref edges)")?;
    }
    let fst_bytes = fst_builder.into_inner().context("finalize ref-edges fst")?;
    Ok((edge_bytes, fst_bytes, posting_data))
}

/// Lookup key encoding for the ref-edges FST. Zero-padded to 10 digits
/// so the FST keys sort numerically — same convention as
/// `call_graph::encode_caller_key`.
pub fn encode_to_sym_key(to_sym_idx: u32) -> String {
    format!("{to_sym_idx:010}")
}

/// Zero-copy reader. Built from mmap byte slices, performs no
/// allocation per lookup beyond the result `Vec`.
#[allow(dead_code)] // wired into the CLI dispatch in 11.1.3d
pub struct RefEdgeReader<'a> {
    fst_map: fst::Map<&'a [u8]>,
    posting_data: &'a [u8],
    edge_data: &'a [u8],
}

#[allow(dead_code)] // wired into the CLI dispatch in 11.1.3d
impl<'a> RefEdgeReader<'a> {
    pub fn new(fst_bytes: &'a [u8], posting_bytes: &'a [u8], edge_bytes: &'a [u8]) -> Result<Self> {
        let fst_map =
            fst::Map::new(fst_bytes).map_err(|e| anyhow::anyhow!("fst load (ref edges): {e}"))?;
        Ok(Self {
            fst_map,
            posting_data: posting_bytes,
            edge_data: edge_bytes,
        })
    }

    /// Return every `RefEdge` whose `to_sym_idx` equals `sym_idx`.
    /// Empty when the symbol has no recorded refs or when the key is
    /// missing from the FST.
    pub fn find_by_symbol_idx(&self, sym_idx: u32) -> Vec<RefEdge> {
        let key = encode_to_sym_key(sym_idx);
        let Some(offset) = self.fst_map.get(key.as_bytes()) else {
            return Vec::new();
        };
        let edge_indices = self.read_posting_list(offset);
        let edge_count = self.edge_data.len() / RefEdge::SIZE;
        let mut out = Vec::with_capacity(edge_indices.len());
        for idx in edge_indices {
            let idx_usize = idx as usize;
            if idx_usize >= edge_count {
                // Corrupt posting list — skip the entry instead of
                // panicking. Caller-visible behaviour is "missing ref",
                // which is the safest degradation.
                continue;
            }
            let off = idx_usize * RefEdge::SIZE;
            // SAFETY: bounds checked above; copy into stack-aligned
            // storage to avoid unaligned reads from mmap.
            let mut rec = std::mem::MaybeUninit::<RefEdge>::uninit();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.edge_data[off..].as_ptr(),
                    rec.as_mut_ptr() as *mut u8,
                    RefEdge::SIZE,
                );
                out.push(rec.assume_init());
            }
        }
        out
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
        let mut out = Vec::with_capacity(count);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A posting list that names an edge_idx pointing past the end of
    /// the records array must NOT panic and must NOT return garbage —
    /// the `idx_usize >= edge_count` guard in `find_by_symbol_idx`
    /// silently skips the OOB entry. This pins the defensive read
    /// path so a refactor that drops the guard fails loudly.
    #[test]
    fn read_skips_out_of_range_edge_idx() {
        // Build a single real edge so `edge_count` == 1.
        let edges = vec![RefEdgeBuilder {
            to_sym_idx: 7,
            from_file_id: 0,
            line: 1,
            col: 1,
            kind: 0,
        }];
        let (edge_bytes, fst_bytes, mut post_bytes) =
            build_ref_edges_section(&edges).expect("build");

        // The posting list for symbol 7 currently contains the single
        // valid edge_idx 0. Overwrite it with 999 — past the one-edge
        // section. We know the layout: [u32 count = 1][u32 idx]; the
        // idx lives at offset 4..8 of the posting blob.
        assert!(post_bytes.len() >= 8);
        post_bytes[4..8].copy_from_slice(&999u32.to_le_bytes());

        let reader = RefEdgeReader::new(&fst_bytes, &post_bytes, &edge_bytes).expect("reader");
        let hits = reader.find_by_symbol_idx(7);
        assert!(
            hits.is_empty(),
            "out-of-range edge_idx must skip silently, got {hits:?}"
        );
    }
}
