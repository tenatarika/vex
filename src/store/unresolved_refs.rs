//! `unresolved_refs` section construction and zero-copy reader (multi-repo
//! Phase 6).
//!
//! These are the references a member's own Pass-2 (`writer.rs` `name_to_global`
//! loop) could NOT resolve to a local definition — `Imported`/`Unresolved`
//! targets whose name is defined nowhere in *this* index. They are dropped
//! from the resolved `reference_edges` section ([`super::format::RefEdge`]),
//! but persisted here keyed by **name** so a workspace query can re-resolve
//! them against a sibling member that does define the symbol (gtags-style
//! ordered fallback — see `docs/MULTIREPO-PHASE6.md`).
//!
//! Mirror of [`super::ref_edges`], with two differences: the FST is keyed on
//! the **lowercased referenced name** (not a stringified `to_sym_idx`), and
//! the on-disk record ([`super::format::UnresolvedRef`]) carries no
//! `to_sym_idx` (the name lives in the FST key).

use anyhow::{Context, Result};

use super::format::UnresolvedRef;

/// Input record for [`build_unresolved_section`]. The writer assembles these
/// from every `ParsedFile.bound_refs` whose target resolved to no local
/// symbol. `name` is stored raw (any case); the builder lowercases it for the
/// FST key, matching the `symbol_fst` convention. `kind` is the
/// [`crate::parse::scope::RefKind`] discriminant as a `u8`.
#[derive(Debug, Clone)]
pub struct UnresolvedRefBuilder {
    pub name: String,
    pub from_file_id: u32,
    pub line: u32,
    pub col: u32,
    pub kind: u8,
}

/// Build the `unresolved_refs` section bytes: `(edges, fst, postings)`.
///
/// Edges are sorted by `(name_lowercased, from_file_id, line, col)` so that
/// (a) the FST receives keys in ascending lexicographic order (the builder
/// requires this), and (b) each name's posting list is a dense range in the
/// on-disk records — good for cache locality when a workspace query walks
/// every unresolved ref for a given name. The sort also makes the section
/// byte-deterministic regardless of the rayon-dependent `bound_refs` order.
pub fn build_unresolved_section(
    edges: &[UnresolvedRefBuilder],
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    if edges.is_empty() {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }

    // Pair each builder with its lowercased key once, then sort by
    // (key, from_file_id, line, col). Owning the key string here keeps the
    // comparison cheap and avoids re-lowercasing in the grouping loop.
    let mut sorted: Vec<(String, &UnresolvedRefBuilder)> =
        edges.iter().map(|e| (e.name.to_lowercase(), e)).collect();
    sorted.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.from_file_id.cmp(&b.1.from_file_id))
            .then(a.1.line.cmp(&b.1.line))
            .then(a.1.col.cmp(&b.1.col))
    });

    let mut edge_bytes: Vec<u8> = Vec::with_capacity(sorted.len() * UnresolvedRef::SIZE);
    for (_, e) in &sorted {
        // 24-bit column ceiling — unreachable in real source files but a
        // future caller could pass an arbitrary u32. Catch it loudly in
        // tests rather than silently truncating into the kind bits.
        debug_assert!(
            e.col <= 0x00FF_FFFF,
            "column {} exceeds the 24-bit UnresolvedRef encoding",
            e.col
        );
        let col_and_kind = (u32::from(e.kind) << 24) | (e.col & 0x00FF_FFFF);
        let rec = UnresolvedRef {
            from_file_id: e.from_file_id,
            line: e.line,
            col_and_kind,
        };
        // SAFETY: UnresolvedRef is #[repr(C)] with fixed 12-byte layout.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &rec as *const UnresolvedRef as *const u8,
                UnresolvedRef::SIZE,
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
        let key = &sorted[i].0;
        let mut j = i + 1;
        while j < sorted.len() && &sorted[j].0 == key {
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
            .context("fst insert (unresolved refs)")?;
        i = j;
    }

    let fst_bytes = fst_builder
        .into_inner()
        .context("finalize unresolved-refs fst")?;
    Ok((edge_bytes, fst_bytes, posting_data))
}

/// Zero-copy reader. Built from mmap byte slices, performs no allocation per
/// lookup beyond the result `Vec`.
pub struct UnresolvedRefReader<'a> {
    fst_map: fst::Map<&'a [u8]>,
    posting_data: &'a [u8],
    edge_data: &'a [u8],
}

impl<'a> UnresolvedRefReader<'a> {
    pub fn new(fst_bytes: &'a [u8], posting_bytes: &'a [u8], edge_bytes: &'a [u8]) -> Result<Self> {
        let fst_map = fst::Map::new(fst_bytes)
            .map_err(|e| anyhow::anyhow!("fst load (unresolved refs): {e}"))?;
        Ok(Self {
            fst_map,
            posting_data: posting_bytes,
            edge_data: edge_bytes,
        })
    }

    /// Return every [`UnresolvedRef`] recorded for `name` (case-insensitive).
    /// Empty when the name has no unresolved refs or is missing from the FST.
    pub fn find_by_name(&self, name: &str) -> Vec<UnresolvedRef> {
        let key = name.to_lowercase();
        let Some(offset) = self.fst_map.get(key.as_bytes()) else {
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

    /// Iterate every `(name, edge)` pair in the section, in FST-key order.
    /// Used by the incremental-update carry-forward — `vex update` reads the
    /// old index's unresolved refs through here and re-emits them for
    /// unchanged files. Allocates the name once per key (cloned per edge).
    pub fn iter_all(&self) -> Vec<(String, UnresolvedRef)> {
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

    /// Copy the `idx`-th [`UnresolvedRef`] out of the mmap'd record array.
    /// `None` when `idx` is out of range (corrupt posting list) — the
    /// caller treats that as a missing ref, the safest degradation.
    fn edge_at(&self, idx: u32) -> Option<UnresolvedRef> {
        let idx_usize = idx as usize;
        let edge_count = self.edge_data.len() / UnresolvedRef::SIZE;
        if idx_usize >= edge_count {
            return None;
        }
        let off = idx_usize * UnresolvedRef::SIZE;
        // SAFETY: bounds checked above; copy into stack-aligned storage to
        // avoid unaligned reads from mmap.
        let mut rec = std::mem::MaybeUninit::<UnresolvedRef>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.edge_data[off..].as_ptr(),
                rec.as_mut_ptr() as *mut u8,
                UnresolvedRef::SIZE,
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

    fn b(name: &str, file: u32, line: u32) -> UnresolvedRefBuilder {
        UnresolvedRefBuilder {
            name: name.to_string(),
            from_file_id: file,
            line,
            col: 1,
            kind: 0,
        }
    }

    #[test]
    fn unresolved_ref_is_twelve_bytes() {
        assert_eq!(UnresolvedRef::SIZE, 12);
        assert_eq!(std::mem::align_of::<UnresolvedRef>(), 4);
    }

    #[test]
    fn unresolved_refs_header_is_forty_eight_bytes() {
        // Pinned: 6 × u64, same shape as V5SectionHeader. Adding fields
        // would silently drift `symbols_offset` in writer.rs.
        assert_eq!(super::super::format::UnresolvedRefsHeader::SIZE, 48);
    }

    #[test]
    fn empty_input_yields_empty_sections() {
        let (e, f, p) = build_unresolved_section(&[]).expect("build");
        assert!(e.is_empty() && f.is_empty() && p.is_empty());
    }

    #[test]
    fn roundtrip_finds_refs_by_name() {
        let edges = vec![b("Foo", 0, 10), b("Bar", 1, 20), b("Foo", 2, 30)];
        let (edge_bytes, fst_bytes, post_bytes) = build_unresolved_section(&edges).expect("build");
        let reader =
            UnresolvedRefReader::new(&fst_bytes, &post_bytes, &edge_bytes).expect("reader");

        let foos = reader.find_by_name("Foo");
        assert_eq!(foos.len(), 2, "two Foo refs");
        let mut lines: Vec<u32> = foos.iter().map(|e| e.line).collect();
        lines.sort_unstable();
        assert_eq!(lines, vec![10, 30]);

        let bars = reader.find_by_name("Bar");
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].from_file_id, 1);
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let edges = vec![b("Foo", 0, 10)];
        let (e, f, p) = build_unresolved_section(&edges).expect("build");
        let reader = UnresolvedRefReader::new(&f, &p, &e).expect("reader");
        assert_eq!(reader.find_by_name("foo").len(), 1, "lowercased query hits");
        assert_eq!(reader.find_by_name("FOO").len(), 1, "uppercased query hits");
    }

    #[test]
    fn case_variant_names_merge_into_one_key() {
        // `Foo` and `foo` referenced in the same index share the lowercased
        // FST key, so both come back from a single lookup.
        let edges = vec![b("Foo", 0, 10), b("foo", 1, 20)];
        let (e, f, p) = build_unresolved_section(&edges).expect("build");
        let reader = UnresolvedRefReader::new(&f, &p, &e).expect("reader");
        assert_eq!(reader.find_by_name("foo").len(), 2);
    }

    #[test]
    fn missing_name_returns_empty() {
        let edges = vec![b("Foo", 0, 10)];
        let (e, f, p) = build_unresolved_section(&edges).expect("build");
        let reader = UnresolvedRefReader::new(&f, &p, &e).expect("reader");
        assert!(reader.find_by_name("Nope").is_empty());
    }

    #[test]
    fn col_and_kind_packs_and_unpacks() {
        let edges = vec![UnresolvedRefBuilder {
            name: "X".into(),
            from_file_id: 3,
            line: 7,
            col: 42,
            kind: 5,
        }];
        let (e, f, p) = build_unresolved_section(&edges).expect("build");
        let reader = UnresolvedRefReader::new(&f, &p, &e).expect("reader");
        let hits = reader.find_by_name("x");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].column(), 42);
        assert_eq!(hits[0].ref_kind_bits(), 5);
    }

    #[test]
    fn read_skips_out_of_range_edge_idx() {
        let edges = vec![b("Foo", 0, 1)];
        let (edge_bytes, fst_bytes, mut post_bytes) =
            build_unresolved_section(&edges).expect("build");
        // Posting layout: [u32 count = 1][u32 idx]; corrupt the idx to 999.
        assert!(post_bytes.len() >= 8);
        post_bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        let reader =
            UnresolvedRefReader::new(&fst_bytes, &post_bytes, &edge_bytes).expect("reader");
        assert!(
            reader.find_by_name("Foo").is_empty(),
            "out-of-range edge_idx must skip silently"
        );
    }
}
