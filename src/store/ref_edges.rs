//! `reference_edges` section construction (11.1.3b; v9 CSR migration —
//! `docs/V9-FORMAT.md` §2.3, §13 R1/R18).
//!
//! At write time the pipeline collects [`RefEdgeBuilder`] records from
//! every parsed file's `bound_refs`, sorts them by `(to_sym_idx,
//! from_file_id, line)`, and serialises them into the on-disk record
//! array via [`build_ref_edges_records`]. Sortedness by `to_sym_idx` is a
//! **v9 format invariant** (R1/R2): the writer (`writer.rs`) feeds the
//! returned keys into `store::csr::build_csr_offsets_sorted`, which
//! `ensure!`s the ordering in release builds — eliding the `edge_idx`
//! array (§2.3) only works because the on-disk records are guaranteed
//! sorted. The legacy (v4–v8) decimal-FST encoder this module used to
//! build lives on as an oracle copy in `store::legacy_v8`
//! (`build_ref_edges_fst`) for the v8 compatibility tests; production no
//! longer builds or reads that FST.

use super::format::RefEdge;

/// Input record for [`build_ref_edges_records`]. The writer assembles
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

/// Build the `reference_edges` on-disk record array, sorted by
/// `(to_sym_idx, from_file_id, line, col)`. Returns `(edge_bytes,
/// sorted_to_sym_idx_keys)` — `sorted_to_sym_idx_keys[i]` is the
/// `to_sym_idx` of the record at byte offset `i * RefEdge::SIZE` in
/// `edge_bytes`. The writer feeds `sorted_to_sym_idx_keys` into
/// `store::csr::build_csr_offsets_sorted` to build the v9 `offsets`
/// array (§2.3) — this function builds no index of its own.
pub fn build_ref_edges_records(edges: &[RefEdgeBuilder]) -> (Vec<u8>, Vec<u32>) {
    if edges.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut sorted: Vec<&RefEdgeBuilder> = edges.iter().collect();
    sorted.sort_by_key(|e| (e.to_sym_idx, e.from_file_id, e.line, e.col));

    let mut edge_bytes: Vec<u8> = Vec::with_capacity(sorted.len() * RefEdge::SIZE);
    let mut keys: Vec<u32> = Vec::with_capacity(sorted.len());

    for e in &sorted {
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
        keys.push(e.to_sym_idx);
    }

    (edge_bytes, keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_sorted_by_to_sym_idx_then_file_then_line_then_col() {
        let edges = vec![
            RefEdgeBuilder {
                to_sym_idx: 7,
                from_file_id: 0,
                line: 10,
                col: 1,
                kind: 0,
            },
            RefEdgeBuilder {
                to_sym_idx: 2,
                from_file_id: 1,
                line: 5,
                col: 2,
                kind: 1,
            },
            RefEdgeBuilder {
                to_sym_idx: 2,
                from_file_id: 0,
                line: 1,
                col: 0,
                kind: 2,
            },
        ];
        let (edge_bytes, keys) = build_ref_edges_records(&edges);
        assert_eq!(keys, vec![2, 2, 7]);
        assert_eq!(edge_bytes.len(), 3 * RefEdge::SIZE);
        // First record (sorted) is to_sym_idx=2, from_file_id=0 (the
        // smaller from_file_id among the two to_sym_idx=2 entries).
        let first_from_file_id = u32::from_le_bytes(edge_bytes[4..8].try_into().unwrap());
        assert_eq!(first_from_file_id, 0);
    }

    #[test]
    fn empty_input_produces_empty_output() {
        let (edge_bytes, keys) = build_ref_edges_records(&[]);
        assert!(edge_bytes.is_empty());
        assert!(keys.is_empty());
    }
}
