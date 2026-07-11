//! `hierarchy_edges` section construction.
//!
//! Typed hierarchy edges (`extends` / `implements` / trait-mixin `uses`) —
//! see `docs/HIERARCHY-EDGES.md` for the full design. This module builds the
//! on-disk `HierarchyEdge[]` / `HierarchyPostingEntry[]` / postings blobs
//! from in-memory builder records. P1 shipped this module format-only
//! (`build_hierarchy_section` always called with an empty slice); P2 wires
//! `writer::resolve_hierarchy_captures` to populate real builders from
//! parsed `HierarchyCapture`s.
//!
//! Mirrors [`super::ref_edges`]'s shape (sort → serialise records → group
//! into posting lists), with one deliberate, LOCKED difference: the index
//! sub-section is a **plain sorted array binary-searched with
//! `partition_point`**, not an `fst::Map`. `to_sym_idx` is already a dense
//! `u32` array index into the Symbols section, so an FST (which buys prefix
//! compression and fuzzy/range queries — neither applicable to a dense
//! integer key) would be strictly worse and would lock a stringified-u32
//! encoding into the format a third time. See `docs/HIERARCHY-EDGES.md`
//! §3.4 for the full rationale (store-agent CRITICAL — locked).

use anyhow::{bail, Result};

use super::format::HierarchyEdge;

/// Input record for [`build_hierarchy_section`]. The (future, P2)
/// extraction pipeline assembles these from `src/hierarchy/queries.rs`
/// captures after Pass-2 name resolution. `kind` is the
/// [`super::format::EdgeKind`] discriminant as a `u8`.
#[derive(Debug, Clone)]
pub struct HierarchyEdgeBuilder {
    pub to_sym_idx: u32,
    pub from_sym_idx: u32,
    pub from_file_id: u32,
    pub line: u32,
    pub kind: u8,
}

/// Pack a 1-based line number and an [`super::format::EdgeKind`]
/// discriminant into the on-disk `line_and_kind` `u32` field.
///
/// Unlike `RefEdge`'s 24-bit *column* (unreachable in real source files —
/// no line is 16 M columns wide), a 24-bit *line* ceiling
/// (`0x00FF_FFFF` == 16,777,215) is closer to reach for
/// generated/adversarial files, and `line` is user-visible (printed and
/// jumped to). So this is a real `Result`-guarded check, **not** a
/// `debug_assert!` that compiles out in release builds — silently
/// truncating to a wrong line is unacceptable here (locked, see
/// `docs/HIERARCHY-EDGES.md` §3.3).
pub fn pack_line_and_kind(line: u32, kind: u8) -> Result<u32> {
    if line > 0x00FF_FFFF {
        bail!(
            "hierarchy edge line {line} exceeds the 24-bit encoding cap (0x00FF_FFFF); \
             cannot pack into HierarchyEdge::line_and_kind"
        );
    }
    Ok((u32::from(kind) << 24) | (line & 0x00FF_FFFF))
}

/// Build the `hierarchy_edges` section bytes: `(edges, index, postings)`.
///
/// Edges are sorted by `(to_sym_idx, from_sym_idx, from_file_id, line)` so
/// (a) the index sub-section receives keys in ascending order (required
/// for the reader's binary search), and (b) each parent's posting list is
/// a dense range in the on-disk records — good for cache locality when
/// `vex implementations` walks every edge for a given parent (P3).
///
/// Any edge whose `line` exceeds the 24-bit cap fails the **whole**
/// section build (the error from [`pack_line_and_kind`] propagates via
/// `?`) rather than being silently dropped or truncated — a builder that
/// violates the line cap is a bug upstream, and swallowing it would ship
/// bad data.
pub fn build_hierarchy_section(
    edges: &[HierarchyEdgeBuilder],
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    if edges.is_empty() {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }

    let mut sorted: Vec<&HierarchyEdgeBuilder> = edges.iter().collect();
    sorted.sort_by_key(|e| (e.to_sym_idx, e.from_sym_idx, e.from_file_id, e.line));

    let mut edge_bytes: Vec<u8> = Vec::with_capacity(sorted.len() * HierarchyEdge::SIZE);
    // (to_sym_idx, edge_idx) accumulator — mirrors ref_edges.rs's v1.13 P7
    // shape. Populated in the same sorted order as `edge_bytes`.
    let mut entries: Vec<(u32, u32)> = Vec::with_capacity(sorted.len());

    for (idx, e) in sorted.iter().enumerate() {
        let line_and_kind = pack_line_and_kind(e.line, e.kind)?;
        let rec = HierarchyEdge {
            to_sym_idx: e.to_sym_idx,
            from_sym_idx: e.from_sym_idx,
            from_file_id: e.from_file_id,
            line_and_kind,
        };
        // SAFETY: HierarchyEdge is #[repr(C)] with fixed 16-byte layout.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &rec as *const HierarchyEdge as *const u8,
                HierarchyEdge::SIZE,
            )
        };
        edge_bytes.extend_from_slice(bytes);

        entries.push((e.to_sym_idx, idx as u32));
    }

    // The outer `sort_by_key` above already grouped edges by `to_sym_idx`,
    // and we pushed `(to_sym_idx, idx)` in iteration order, so `entries`
    // is already sorted by key + ascending idx within group.

    let mut posting_data: Vec<u8> = Vec::new();
    let mut index_bytes: Vec<u8> =
        Vec::with_capacity(sorted.len() * super::format::HierarchyPostingEntry::SIZE);

    let mut i = 0;
    while i < entries.len() {
        let key = entries[i].0;
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == key {
            j += 1;
        }
        let offset = posting_data.len() as u32;
        let count = (j - i) as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for slot in &entries[i..j] {
            posting_data.extend_from_slice(&slot.1.to_le_bytes());
        }

        let posting_entry = super::format::HierarchyPostingEntry {
            to_sym_idx: key,
            posting_offset: offset,
        };
        // SAFETY: HierarchyPostingEntry is #[repr(C)] with fixed 8-byte layout.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &posting_entry as *const super::format::HierarchyPostingEntry as *const u8,
                super::format::HierarchyPostingEntry::SIZE,
            )
        };
        index_bytes.extend_from_slice(bytes);

        i = j;
    }

    Ok((edge_bytes, index_bytes, posting_data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(to: u32, from: u32, file: u32, line: u32, kind: u8) -> HierarchyEdgeBuilder {
        HierarchyEdgeBuilder {
            to_sym_idx: to,
            from_sym_idx: from,
            from_file_id: file,
            line,
            kind,
        }
    }

    #[test]
    fn empty_input_yields_empty_sections() {
        let (e, i, p) = build_hierarchy_section(&[]).expect("build");
        assert!(e.is_empty() && i.is_empty() && p.is_empty());
    }

    #[test]
    fn pack_line_and_kind_rejects_over_cap_line() {
        assert!(pack_line_and_kind(0x0100_0000, 0).is_err());
    }

    #[test]
    fn pack_line_and_kind_accepts_max_cap_line() {
        let packed = pack_line_and_kind(0x00FF_FFFF, 2).expect("max line must pack");
        assert_eq!(packed, (2u32 << 24) | 0x00FF_FFFF);
    }

    #[test]
    fn build_fails_when_any_edge_exceeds_line_cap() {
        let edges = vec![b(1, 10, 0, 5, 0), b(1, 11, 0, 0x0100_0000, 1)];
        let result = build_hierarchy_section(&edges);
        assert!(
            result.is_err(),
            "a single over-cap line must fail the whole section build"
        );
    }

    #[test]
    fn byte_shapes_are_multiples_of_record_size() {
        let edges = vec![
            b(1, 10, 0, 5, 0),
            b(1, 11, 0, 6, 1),
            b(2, 12, 1, 7, 2),
            b(3, 13, 1, 8, 0),
            b(3, 14, 2, 9, 1),
        ];
        let (edge_bytes, index_bytes, posting_bytes) =
            build_hierarchy_section(&edges).expect("build");

        assert_eq!(edge_bytes.len() % HierarchyEdge::SIZE, 0);
        assert_eq!(edge_bytes.len() / HierarchyEdge::SIZE, 5);

        assert_eq!(
            index_bytes.len() % super::super::format::HierarchyPostingEntry::SIZE,
            0
        );
        // 3 distinct to_sym_idx values: 1, 2, 3.
        assert_eq!(
            index_bytes.len() / super::super::format::HierarchyPostingEntry::SIZE,
            3
        );

        // Every posting-list length prefix must be readable within bounds.
        assert!(!posting_bytes.is_empty());
    }

    #[test]
    fn index_entries_are_sorted_ascending_by_to_sym_idx() {
        let edges = vec![
            b(5, 1, 0, 1, 0),
            b(1, 2, 0, 2, 0),
            b(3, 3, 0, 3, 0),
            b(1, 4, 0, 4, 1),
        ];
        let (_edges, index_bytes, _posts) = build_hierarchy_section(&edges).expect("build");

        let entry_size = super::super::format::HierarchyPostingEntry::SIZE;
        let count = index_bytes.len() / entry_size;
        let mut keys = Vec::with_capacity(count);
        for i in 0..count {
            let off = i * entry_size;
            let key = u32::from_le_bytes(index_bytes[off..off + 4].try_into().unwrap());
            keys.push(key);
        }
        let mut sorted_keys = keys.clone();
        sorted_keys.sort_unstable();
        assert_eq!(keys, sorted_keys, "index entries must be ascending");
        assert_eq!(keys, vec![1, 3, 5]);
    }

    #[test]
    fn edges_are_grouped_contiguously_by_to_sym_idx() {
        // Sanity: the sorted edge array groups every parent's children
        // into a contiguous run, which is what makes the posting list's
        // edge_idx range dense/cache-friendly.
        let edges = vec![
            b(2, 20, 0, 1, 0),
            b(1, 10, 0, 1, 0),
            b(2, 21, 0, 2, 1),
            b(1, 11, 0, 2, 0),
        ];
        let (edge_bytes, _index, _posts) = build_hierarchy_section(&edges).expect("build");
        let count = edge_bytes.len() / HierarchyEdge::SIZE;
        let mut to_syms = Vec::with_capacity(count);
        for i in 0..count {
            let off = i * HierarchyEdge::SIZE;
            let to_sym = u32::from_le_bytes(edge_bytes[off..off + 4].try_into().unwrap());
            to_syms.push(to_sym);
        }
        assert_eq!(to_syms, vec![1, 1, 2, 2]);
    }

    #[test]
    fn unsorted_input_order_still_yields_sorted_queryable_section() {
        // Builder input is given in an intentionally scrambled order (not
        // grouped by parent, not ascending). The section must still come
        // out sorted by to_sym_idx (§ build_hierarchy_section doc contract)
        // regardless of caller iteration order — P2's extraction pass has
        // no reason to emit edges in parent order.
        let edges = vec![
            b(3, 30, 2, 9, 1),
            b(1, 10, 0, 1, 0),
            b(2, 20, 1, 5, 2),
            b(1, 11, 0, 2, 0),
            b(3, 31, 2, 10, 0),
        ];
        let (edge_bytes, index_bytes, posting_bytes) =
            build_hierarchy_section(&edges).expect("build");

        // Edge array is grouped/sorted ascending by to_sym_idx.
        let count = edge_bytes.len() / HierarchyEdge::SIZE;
        let mut to_syms = Vec::with_capacity(count);
        for i in 0..count {
            let off = i * HierarchyEdge::SIZE;
            to_syms.push(u32::from_le_bytes(
                edge_bytes[off..off + 4].try_into().unwrap(),
            ));
        }
        let mut sorted = to_syms.clone();
        sorted.sort_unstable();
        assert_eq!(to_syms, sorted, "edges must be sorted by to_sym_idx");
        assert_eq!(to_syms, vec![1, 1, 2, 3, 3]);

        // Every posting-list edge_idx must resolve to an edge whose
        // to_sym_idx matches the index entry's key — i.e. the section is
        // genuinely queryable, not just superficially sorted.
        let entry_size = super::super::format::HierarchyPostingEntry::SIZE;
        let entry_count = index_bytes.len() / entry_size;
        for i in 0..entry_count {
            let off = i * entry_size;
            let key = u32::from_le_bytes(index_bytes[off..off + 4].try_into().unwrap());
            let posting_offset =
                u32::from_le_bytes(index_bytes[off + 4..off + 8].try_into().unwrap()) as usize;
            let pcount = u32::from_le_bytes(
                posting_bytes[posting_offset..posting_offset + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            for slot in 0..pcount {
                let idx_off = posting_offset + 4 + slot * 4;
                let edge_idx =
                    u32::from_le_bytes(posting_bytes[idx_off..idx_off + 4].try_into().unwrap())
                        as usize;
                let edge_off = edge_idx * HierarchyEdge::SIZE;
                let edge_to_sym =
                    u32::from_le_bytes(edge_bytes[edge_off..edge_off + 4].try_into().unwrap());
                assert_eq!(
                    edge_to_sym, key,
                    "posting entry must point at a matching edge"
                );
            }
        }
    }

    proptest::proptest! {
        // Builder -> section -> manual-decode roundtrip over arbitrary
        // (unsorted, duplicate-key-heavy) inputs. Mirrors the grep-trigram
        // no-false-negative proptest precedent (src/grep/trigram.rs):
        // every edge fed to the builder must be recoverable by grouping
        // the flattened (edge, index, postings) bytes by to_sym_idx, with
        // no edge gained, lost, or corrupted, and the index/edge arrays
        // must stay sorted ascending by to_sym_idx regardless of input
        // order.
        #[test]
        fn build_hierarchy_section_roundtrip(
            raw in proptest::collection::vec(
                (0u32..20, 0u32..1000, 0u32..10, 0u32..500, 0u8..3),
                0..64,
            )
        ) {
            let edges: Vec<HierarchyEdgeBuilder> = raw
                .iter()
                .map(|&(to, from, file, line, kind)| b(to, from, file, line, kind))
                .collect();

            let (edge_bytes, index_bytes, posting_bytes) =
                build_hierarchy_section(&edges).expect("all lines are within the 24-bit cap");

            // 1. Edge array length matches input length exactly (no drops).
            proptest::prop_assert_eq!(edge_bytes.len(), edges.len() * HierarchyEdge::SIZE);

            // 2. Edge array is sorted ascending by to_sym_idx.
            let n = edge_bytes.len() / HierarchyEdge::SIZE;
            let mut to_syms = Vec::with_capacity(n);
            for i in 0..n {
                let off = i * HierarchyEdge::SIZE;
                to_syms.push(u32::from_le_bytes(
                    edge_bytes[off..off + 4].try_into().unwrap(),
                ));
            }
            let mut sorted_to_syms = to_syms.clone();
            sorted_to_syms.sort_unstable();
            proptest::prop_assert_eq!(&to_syms, &sorted_to_syms);

            // 3. Index array is sorted ascending by to_sym_idx and has one
            //    entry per DISTINCT to_sym_idx.
            let entry_size = super::super::format::HierarchyPostingEntry::SIZE;
            proptest::prop_assert_eq!(index_bytes.len() % entry_size, 0);
            let entry_count = index_bytes.len() / entry_size;
            let mut distinct: Vec<u32> = to_syms.clone();
            distinct.dedup();
            proptest::prop_assert_eq!(entry_count, distinct.len());

            let mut prev_key: Option<u32> = None;
            let mut recovered_edge_count = 0usize;
            for i in 0..entry_count {
                let off = i * entry_size;
                let key = u32::from_le_bytes(index_bytes[off..off + 4].try_into().unwrap());
                if let Some(p) = prev_key {
                    proptest::prop_assert!(key > p, "index keys must be strictly ascending");
                }
                prev_key = Some(key);

                let posting_offset = u32::from_le_bytes(
                    index_bytes[off + 4..off + 8].try_into().unwrap(),
                ) as usize;
                proptest::prop_assert!(posting_offset + 4 <= posting_bytes.len());
                let pcount = u32::from_le_bytes(
                    posting_bytes[posting_offset..posting_offset + 4]
                        .try_into()
                        .unwrap(),
                ) as usize;
                recovered_edge_count += pcount;

                for slot in 0..pcount {
                    let idx_off = posting_offset + 4 + slot * 4;
                    proptest::prop_assert!(idx_off + 4 <= posting_bytes.len());
                    let edge_idx = u32::from_le_bytes(
                        posting_bytes[idx_off..idx_off + 4].try_into().unwrap(),
                    ) as usize;
                    proptest::prop_assert!(edge_idx < n);
                    let edge_off = edge_idx * HierarchyEdge::SIZE;
                    let edge_to_sym = u32::from_le_bytes(
                        edge_bytes[edge_off..edge_off + 4].try_into().unwrap(),
                    );
                    proptest::prop_assert_eq!(edge_to_sym, key);
                }
            }
            // 4. Every edge is reachable through exactly one posting list —
            //    total posting entries == total edges (no gained/lost edges).
            proptest::prop_assert_eq!(recovered_edge_count, n);
        }
    }
}
