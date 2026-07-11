//! `unresolved_hierarchy` section construction and zero-copy reader (P2,
//! `docs/HIERARCHY-EDGES.md` §3.5).
//!
//! These are hierarchy captures (`extends`/`implements`/`uses`) whose
//! **parent** name resolved to zero global candidates in the writer's
//! post-loop Pass-2 (`class Foo extends SomeStdlibClass` where the parent
//! is outside the corpus). Every mature code-intel format keeps this edge
//! rather than dropping it — a dropped edge silently makes `Foo` look like
//! a root type. Persisted here keyed by the **verbatim** (case-preserved)
//! parent name so a workspace query can re-resolve it against a sibling
//! member that does define the symbol.
//!
//! Structurally mirrors [`super::unresolved_refs`] (name-keyed FST +
//! posting lists — a name key legitimately needs the FST here, unlike the
//! resolved [`super::hierarchy_edges`] section's dense-`u32`-key sorted
//! array), with two deliberate differences (Q3 LOCKED, see
//! `docs/HIERARCHY-EDGES.md` §3.5):
//!
//! 1. **Not a reuse of [`super::unresolved_refs`]/`UnresolvedRef`** — that
//!    record has no field to distinguish a hierarchy edge from a normal
//!    reference, and reusing it would either couple `RefKind` to
//!    `EdgeKind` or require inventing a tag byte with no home in the
//!    current layout.
//! 2. **The FST key is NOT lowercased.** `unresolved_refs` lowercases its
//!    key because it indexes free-text references; here the key is a
//!    type/module name, which is case-sensitive by construction — folding
//!    case would conflate distinct symbols.
//! 3. **Spills unconditionally** — no `is_meaningful_identifier` gate. That
//!    filter rejects pure-lowercase identifiers without `_`
//!    (`compute`/`total`), which would silently drop legitimate
//!    Ruby/Python/PHP lowercase mixin/base names, recreating exactly the
//!    "looks like a root type" bug this section exists to prevent. A name
//!    captured from an `extends`/`implements`/`use` clause is meaningful
//!    by construction.

use anyhow::{Context, Result};

use super::format::UnresolvedHierarchyEdge;
use super::hierarchy_edges::pack_line_and_kind;

/// Input record for [`build_unresolved_hierarchy_section`]. The writer
/// assembles these from every `HierarchyCapture` whose `parent_name`
/// resolved to zero global candidates. `parent_name` is stored verbatim
/// (case-preserved) — the FST key mirrors it exactly, no lowercasing.
/// `kind` is the [`super::format::EdgeKind`] discriminant as a `u8`.
#[derive(Debug, Clone)]
pub struct UnresolvedHierarchyEdgeBuilder {
    pub parent_name: String,
    pub from_sym_idx: u32,
    pub from_file_id: u32,
    pub line: u32,
    pub kind: u8,
}

/// Build the `unresolved_hierarchy` section bytes: `(edges, fst, postings)`.
///
/// Edges are sorted by `(parent_name, from_sym_idx, from_file_id, line)` so
/// (a) the FST receives keys in ascending lexicographic order (the builder
/// requires this), and (b) each name's posting list is a dense range in the
/// on-disk records. The sort also makes the section byte-deterministic
/// regardless of the caller's input order.
///
/// Any edge whose `line` exceeds the 24-bit cap fails the **whole** section
/// build (propagated via `?` from the shared [`pack_line_and_kind`] guard),
/// matching [`super::hierarchy_edges::build_hierarchy_section`]'s contract
/// — a builder that violates the line cap is a bug upstream.
pub fn build_unresolved_hierarchy_section(
    edges: &[UnresolvedHierarchyEdgeBuilder],
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    if edges.is_empty() {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }

    let mut sorted: Vec<&UnresolvedHierarchyEdgeBuilder> = edges.iter().collect();
    sorted.sort_by(|a, b| {
        a.parent_name
            .cmp(&b.parent_name)
            .then(a.from_sym_idx.cmp(&b.from_sym_idx))
            .then(a.from_file_id.cmp(&b.from_file_id))
            .then(a.line.cmp(&b.line))
    });

    let mut edge_bytes: Vec<u8> = Vec::with_capacity(sorted.len() * UnresolvedHierarchyEdge::SIZE);
    for e in &sorted {
        let line_and_kind = pack_line_and_kind(e.line, e.kind)?;
        let rec = UnresolvedHierarchyEdge {
            from_sym_idx: e.from_sym_idx,
            from_file_id: e.from_file_id,
            line_and_kind,
        };
        // SAFETY: UnresolvedHierarchyEdge is #[repr(C)] with fixed 12-byte layout.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &rec as *const UnresolvedHierarchyEdge as *const u8,
                UnresolvedHierarchyEdge::SIZE,
            )
        };
        edge_bytes.extend_from_slice(bytes);
    }

    // Group consecutive equal keys (the sort above already clustered them)
    // into posting lists. Each list is `[u32 count][u32 edge_idx; count]`.
    let mut posting_data: Vec<u8> = Vec::new();
    let mut fst_builder = fst::MapBuilder::memory();
    let mut i = 0;
    while i < sorted.len() {
        let key = &sorted[i].parent_name;
        let mut j = i + 1;
        while j < sorted.len() && &sorted[j].parent_name == key {
            j += 1;
        }
        let offset = posting_data.len() as u64;
        let count = (j - i) as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for idx in i..j {
            posting_data.extend_from_slice(&(idx as u32).to_le_bytes());
        }
        fst_builder
            .insert(key.as_bytes(), offset)
            .context("fst insert (unresolved hierarchy)")?;
        i = j;
    }

    let fst_bytes = fst_builder
        .into_inner()
        .context("finalize unresolved-hierarchy fst")?;
    Ok((edge_bytes, fst_bytes, posting_data))
}

/// Zero-copy reader. Built from mmap byte slices, performs no allocation per
/// lookup beyond the result `Vec`.
pub struct UnresolvedHierarchyReader<'a> {
    fst_map: fst::Map<&'a [u8]>,
    posting_data: &'a [u8],
    edge_data: &'a [u8],
}

impl<'a> UnresolvedHierarchyReader<'a> {
    pub fn new(fst_bytes: &'a [u8], posting_bytes: &'a [u8], edge_bytes: &'a [u8]) -> Result<Self> {
        let fst_map = fst::Map::new(fst_bytes)
            .map_err(|e| anyhow::anyhow!("fst load (unresolved hierarchy): {e}"))?;
        Ok(Self {
            fst_map,
            posting_data: posting_bytes,
            edge_data: edge_bytes,
        })
    }

    /// Return every [`UnresolvedHierarchyEdge`] recorded for the verbatim
    /// parent `name` (case-sensitive — unlike `unresolved_refs`, this
    /// section does NOT lowercase its key). Empty when the name has no
    /// unresolved hierarchy edges or is missing from the FST.
    pub fn find_by_name(&self, name: &str) -> Vec<UnresolvedHierarchyEdge> {
        let Some(offset) = self.fst_map.get(name.as_bytes()) else {
            return Vec::new();
        };
        let edge_indices = self.read_posting_list(offset);
        let mut out = Vec::with_capacity(edge_indices.len());
        for idx in edge_indices {
            if let Some(rec) = self.edge_at(idx) {
                out.push(rec);
            }
        }
        out
    }

    /// Iterate every `(parent_name, edge)` pair in the section, in FST-key
    /// order (verbatim case, mirrors [`Self::find_by_name`]'s key). Used by
    /// the `vex update` carry-forward (P2a) — `reconstruct_unchanged` reads
    /// the old index's unresolved hierarchy edges through here and rebuilds
    /// `HierarchyCapture`s for unchanged files so the writer's post-loop
    /// Pass-2 can re-resolve them against the NEW index. Mirrors
    /// [`super::unresolved_refs::UnresolvedRefReader::iter_all`].
    pub fn iter_all(&self) -> Vec<(String, UnresolvedHierarchyEdge)> {
        use fst::Streamer;
        let mut out = Vec::new();
        let mut stream = self.fst_map.stream();
        while let Some((key, offset)) = stream.next() {
            let name = String::from_utf8_lossy(key).into_owned();
            for idx in self.read_posting_list(offset) {
                if let Some(rec) = self.edge_at(idx) {
                    out.push((name.clone(), rec));
                }
            }
        }
        out
    }

    /// Copy the `idx`-th [`UnresolvedHierarchyEdge`] out of the mmap'd
    /// record array. `None` when `idx` is out of range (corrupt posting
    /// list) — the caller treats that as a missing edge, the safest
    /// degradation.
    fn edge_at(&self, idx: u32) -> Option<UnresolvedHierarchyEdge> {
        let idx_usize = idx as usize;
        let edge_count = self.edge_data.len() / UnresolvedHierarchyEdge::SIZE;
        if idx_usize >= edge_count {
            return None;
        }
        let off = idx_usize * UnresolvedHierarchyEdge::SIZE;
        // SAFETY: bounds checked above; copy into stack-aligned storage to
        // avoid unaligned reads from mmap.
        let mut rec = std::mem::MaybeUninit::<UnresolvedHierarchyEdge>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.edge_data[off..].as_ptr(),
                rec.as_mut_ptr() as *mut u8,
                UnresolvedHierarchyEdge::SIZE,
            );
            Some(rec.assume_init())
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
        // Cap the speculative allocation to what the blob can hold (4 bytes
        // per entry) so a crafted `count` can't trigger a huge OOM alloc; the
        // loop below still bounds-checks every read.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn b(
        name: &str,
        from_sym: u32,
        file: u32,
        line: u32,
        kind: u8,
    ) -> UnresolvedHierarchyEdgeBuilder {
        UnresolvedHierarchyEdgeBuilder {
            parent_name: name.to_string(),
            from_sym_idx: from_sym,
            from_file_id: file,
            line,
            kind,
        }
    }

    #[test]
    fn empty_input_yields_empty_sections() {
        let (e, f, p) = build_unresolved_hierarchy_section(&[]).expect("build");
        assert!(e.is_empty() && f.is_empty() && p.is_empty());
    }

    #[test]
    fn roundtrip_finds_edges_by_parent_name() {
        let edges = vec![
            b("Foo", 1, 0, 10, 0),
            b("Bar", 2, 1, 20, 2),
            b("Foo", 3, 2, 30, 0),
        ];
        let (edge_bytes, fst_bytes, post_bytes) =
            build_unresolved_hierarchy_section(&edges).expect("build");
        let reader =
            UnresolvedHierarchyReader::new(&fst_bytes, &post_bytes, &edge_bytes).expect("reader");

        let foos = reader.find_by_name("Foo");
        assert_eq!(foos.len(), 2, "two Foo edges");
        let mut lines: Vec<u32> = foos.iter().map(|e| e.line()).collect();
        lines.sort_unstable();
        assert_eq!(lines, vec![10, 30]);

        let bars = reader.find_by_name("Bar");
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].from_file_id, 1);
    }

    #[test]
    fn lookup_is_case_sensitive() {
        // Unlike unresolved_refs, this section must NOT fold case — a
        // name from an extends/implements/use clause is a real symbol
        // name, not a free-text reference.
        let edges = vec![b("Foo", 1, 0, 10, 0)];
        let (e, f, p) = build_unresolved_hierarchy_section(&edges).expect("build");
        let reader = UnresolvedHierarchyReader::new(&f, &p, &e).expect("reader");
        assert_eq!(reader.find_by_name("Foo").len(), 1, "exact case hits");
        assert!(
            reader.find_by_name("foo").is_empty(),
            "lowercased query must NOT match — case-sensitive key"
        );
        assert!(
            reader.find_by_name("FOO").is_empty(),
            "uppercased query must NOT match — case-sensitive key"
        );
    }

    #[test]
    fn lowercase_parent_name_still_spills() {
        // Simulates Ruby `include mymodule` — a lowercase mixin name.
        // This section must NOT apply is_meaningful_identifier or any
        // other noise filter (LOCKED, §3.5).
        let edges = vec![b("mymodule", 5, 0, 7, 2)];
        let (e, f, p) = build_unresolved_hierarchy_section(&edges).expect("build");
        let reader = UnresolvedHierarchyReader::new(&f, &p, &e).expect("reader");
        let hits = reader.find_by_name("mymodule");
        assert_eq!(
            hits.len(),
            1,
            "lowercase mixin name must spill unconditionally"
        );
        assert_eq!(hits[0].edge_kind_bits(), 2);
    }

    #[test]
    fn missing_name_returns_empty() {
        let edges = vec![b("Foo", 1, 0, 10, 0)];
        let (e, f, p) = build_unresolved_hierarchy_section(&edges).expect("build");
        let reader = UnresolvedHierarchyReader::new(&f, &p, &e).expect("reader");
        assert!(reader.find_by_name("Nope").is_empty());
    }

    #[test]
    fn line_and_kind_pack_and_unpack() {
        let edges = vec![b("X", 9, 3, 42, 1)];
        let (e, f, p) = build_unresolved_hierarchy_section(&edges).expect("build");
        let reader = UnresolvedHierarchyReader::new(&f, &p, &e).expect("reader");
        let hits = reader.find_by_name("X");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line(), 42);
        assert_eq!(hits[0].edge_kind_bits(), 1);
        assert_eq!(hits[0].from_sym_idx, 9);
    }

    #[test]
    fn build_fails_when_any_edge_exceeds_line_cap() {
        let edges = vec![b("A", 1, 0, 5, 0), b("A", 2, 0, 0x0100_0000, 1)];
        let result = build_unresolved_hierarchy_section(&edges);
        assert!(
            result.is_err(),
            "a single over-cap line must fail the whole section build"
        );
    }

    #[test]
    fn read_skips_out_of_range_edge_idx() {
        let edges = vec![b("Foo", 1, 0, 1, 0)];
        let (edge_bytes, fst_bytes, mut post_bytes) =
            build_unresolved_hierarchy_section(&edges).expect("build");
        // Posting layout: [u32 count = 1][u32 idx]; corrupt the idx to 999.
        assert!(post_bytes.len() >= 8);
        post_bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        let reader =
            UnresolvedHierarchyReader::new(&fst_bytes, &post_bytes, &edge_bytes).expect("reader");
        assert!(
            reader.find_by_name("Foo").is_empty(),
            "out-of-range edge_idx must skip silently"
        );
    }

    #[test]
    fn iter_all_enumerates_every_edge_with_its_parent_name() {
        let edges = vec![
            b("Foo", 1, 0, 10, 0),
            b("Bar", 2, 1, 20, 2),
            b("Foo", 3, 2, 30, 0),
        ];
        let (edge_bytes, fst_bytes, post_bytes) =
            build_unresolved_hierarchy_section(&edges).expect("build");
        let reader =
            UnresolvedHierarchyReader::new(&fst_bytes, &post_bytes, &edge_bytes).expect("reader");

        let mut all = reader.iter_all();
        all.sort_by_key(|(name, e)| (name.clone(), e.from_sym_idx));
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, "Bar");
        assert_eq!(all[0].1.from_sym_idx, 2);
        assert_eq!(all[1].0, "Foo");
        assert_eq!(all[1].1.from_sym_idx, 1);
        assert_eq!(all[2].0, "Foo");
        assert_eq!(all[2].1.from_sym_idx, 3);
    }

    #[test]
    fn iter_all_on_empty_section_returns_empty() {
        // Constructing a reader over an empty section isn't meaningful
        // (fst::Map::new requires valid FST bytes); this documents that
        // the reader-level accessor (IndexReader::unresolved_hierarchy_all)
        // short-circuits via has_unresolved_hierarchy_edges() before ever
        // constructing a reader — see store/reader.rs.
        let edges: Vec<UnresolvedHierarchyEdgeBuilder> = vec![];
        let (e, f, p) = build_unresolved_hierarchy_section(&edges).expect("build");
        assert!(e.is_empty() && f.is_empty() && p.is_empty());
    }
}
