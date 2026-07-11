//! Binary index file format specification.
//!
//! Layout (v8 — current):
//! ```text
//! [Header]                  fixed (168 B) - magic, version, counts, section offsets
//! [CallGraphHeader]         fixed (128 B) - call graph section offsets (v4+)
//! [V5SectionHeader]         fixed (48  B) - ref edges section offsets (v5+)
//! [PatternSkeletonHeader]   fixed (168 B) - pattern skeleton section offsets + fingerprints (v6+)
//! [UnresolvedRefsHeader]    fixed (48  B) - unresolved-by-name ref section offsets (v7+)
//! [HierarchyHeader]         fixed (48  B) - typed hierarchy edge section offsets (v8+)
//! [Symbols Section]         variable      - fixed-size symbol records
//! [Vectors Section]         variable      - dense f32 arrays (vector_dim each)
//! [Strings Section]         variable      - deduplicated string pool
//! [Refs FST]                variable      - fst::Map bytes (ref name → posting offset)
//! [Refs Postings]           variable      - posting lists (count, [(file_id, line)])
//! [File Table]              variable      - u32 string offsets, one per file_id
//! [Symbol FST]              variable      - fst::Map bytes (name/token → sym posting offset)
//! [Symbol Postings]         variable      - posting lists (count, [symbol_idx])
//! [Call Edges]              variable      - fixed-size CallEdge records (v4+)
//! [Callers FST]             variable      - fst::Map (callee name → edge-idx posting offset)
//! [Callers Postings]        variable      - posting lists (count, [edge_idx])
//! [Callees FST]             variable      - fst::Map (caller_sym_idx_str → posting offset)
//! [Callees Postings]        variable      - posting lists (count, [edge_idx])
//! [Reference Edges]         variable      - fixed-size RefEdge records (v5+)
//! [Reference Edges FST]     variable      - fst::Map (v5+)
//! [Reference Edges Posts]   variable      - posting lists (v5+)
//! [Skeleton Records]        variable      - fixed-size SkeletonRecord array (v6+)
//! [Kind Path Arena]         variable      - kind-name path entries (v6+)
//! [Ident Pool]              variable      - length-prefixed UTF-8 identifier strings (v6+)
//! [File Index]              variable      - per-file skeleton lookup (v6+)
//! [Unresolved Edges]        variable      - fixed-size UnresolvedRef records (v7+)
//! [Unresolved Edges FST]    variable      - fst::Map (lowercased name → posting offset) (v7+)
//! [Unresolved Edges Posts]  variable      - posting lists (count, [edge_idx]) (v7+)
//! [Hierarchy Edges]         variable      - fixed-size HierarchyEdge array, sorted by to_sym_idx (v8+)
//! [Hierarchy Index]         variable      - sorted HierarchyPostingEntry array, binary-searched (v8+)
//! [Hierarchy Postings]      variable      - posting lists (count, [edge_idx]) (v8+)
//! ```
//!
//! Layout v3 (legacy, still readable): same as v4 minus `CallGraphHeader`
//! (the `Symbols Section` starts directly at `Header::SIZE`) and minus all
//! call-graph sections. The `Header` struct is byte-identical between v3
//! and v4 — version dispatch happens at the reader.

pub const MAGIC: &[u8; 4] = b"VEXI";
pub const VERSION: u32 = 8;
/// Oldest format version this build can still open for read.
/// v3 and v4 indexes continue to read without the v5-only sections —
/// `vex usages --strict` will refuse, everything else still works.
pub const MIN_SUPPORTED_VERSION: u32 = 3;
pub const VECTOR_DIM: u32 = 384;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub magic: [u8; 4],
    pub version: u32,
    pub symbol_count: u64,
    pub vector_dim: u32,
    pub _padding: u32,
    pub symbols_offset: u64,
    pub vectors_offset: u64,
    pub strings_offset: u64,
    pub inverted_offset: u64,
    pub hnsw_offset: u64,
    // Refs FST
    pub fst_offset: u64,
    pub fst_len: u64,
    pub postings_offset: u64,
    pub postings_len: u64,
    // File table
    pub file_table_offset: u64,
    pub file_table_count: u32,
    pub _padding2: u32,
    // Symbol FST (v3)
    pub sym_fst_offset: u64,
    pub sym_fst_len: u64,
    pub sym_postings_offset: u64,
    pub sym_postings_len: u64,
}

impl Header {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    pub fn has_refs(&self) -> bool {
        self.fst_len > 0
    }

    pub fn has_symbol_fst(&self) -> bool {
        self.version >= 3 && self.sym_fst_len > 0
    }

    /// Whether this index format carries a [`CallGraphHeader`] immediately
    /// after the base header. v3 indexes do not.
    pub fn has_call_graph_header(&self) -> bool {
        self.version >= 4
    }

    /// Whether this index format carries a [`V5SectionHeader`] immediately
    /// after the [`CallGraphHeader`]. v3/v4 indexes do not — `vex usages
    /// --strict` falls back to the legacy refs FST on those.
    pub fn has_v5_section_header(&self) -> bool {
        self.version >= 5
    }

    /// Whether this index format carries a [`PatternSkeletonHeader`]
    /// immediately after the [`V5SectionHeader`]. v3/v4/v5 indexes do not.
    pub fn has_pattern_skeleton_header(&self) -> bool {
        self.version >= 6
    }

    /// Whether this index format carries an [`UnresolvedRefsHeader`]
    /// immediately after the [`PatternSkeletonHeader`]. v3..v6 indexes do
    /// not — cross-repo strict-usages fallback (multi-repo Phase 6) is
    /// unavailable on those until `vex index` rebuilds at v7.
    pub fn has_unresolved_refs_header(&self) -> bool {
        self.version >= 7
    }

    /// Whether this index format carries a [`HierarchyHeader`]
    /// immediately after the [`UnresolvedRefsHeader`]. v3..v7 indexes do
    /// not — the typed hierarchy edge section (`extends`/`implements`)
    /// is unavailable on those until `vex index` rebuilds at v8.
    pub fn has_hierarchy_header(&self) -> bool {
        self.version >= 8
    }
}

/// Section offsets and lengths for the v4-only sections (call graph + BM25).
/// Located in the file at exactly `Header::SIZE` when `header.version >= 4`.
/// Absent from v3 files — those have `Symbols` starting directly at
/// `Header::SIZE`.
///
/// Name kept as `CallGraphHeader` for back-compat with 9.3 callers; will be
/// renamed to a generic `V4SectionHeader` in a follow-up when more v4
/// sections land.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CallGraphHeader {
    // Phase 9.3 — call graph sections.
    pub call_edges_offset: u64,
    pub call_edges_len: u64,
    pub callers_fst_offset: u64,
    pub callers_fst_len: u64,
    pub callers_postings_offset: u64,
    pub callers_postings_len: u64,
    pub callees_fst_offset: u64,
    pub callees_fst_len: u64,
    pub callees_postings_offset: u64,
    pub callees_postings_len: u64,

    // Phase 9.4 — BM25 channel sections.
    pub bm25_fst_offset: u64,
    pub bm25_fst_len: u64,
    pub bm25_postings_offset: u64,
    pub bm25_postings_len: u64,
    pub bm25_stats_offset: u64,
    pub bm25_stats_len: u64,
}

impl CallGraphHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// Section offsets and lengths for the v5-only sections (type-aware
/// reference edges from the scope binder, 11.1.x). Located in the file
/// at exactly `Header::SIZE + CallGraphHeader::SIZE` when
/// `header.version >= 5`. Absent from v3/v4 files.
///
/// In 11.1.3a the section payload itself is not yet written — every
/// field below stays zero so a v5 index is bit-identical to a v4 index
/// from the symbols section onward, just with an extra zeroed header
/// block. 11.1.3b populates the section with real `RefEdge` records.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct V5SectionHeader {
    /// Raw fixed-size `RefEdge` records (16 bytes each). The number of
    /// records is `ref_edges_len / RefEdge::SIZE`.
    pub ref_edges_offset: u64,
    pub ref_edges_len: u64,
    /// FST keyed on the stringified `to_sym_idx` (decimal). Values are
    /// u64 offsets into `ref_edges_postings`.
    pub ref_edges_fst_offset: u64,
    pub ref_edges_fst_len: u64,
    /// Posting lists: for each `to_sym_idx` key, a `[u32 count][u32
    /// edge_idx; count]` block that indexes into the `RefEdge` records.
    pub ref_edges_postings_offset: u64,
    pub ref_edges_postings_len: u64,
}

impl V5SectionHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// Section offsets and lengths for the v6-only sections (pattern
/// skeletons from the structural-pattern prefilter, 11.4). Located in
/// the file at exactly
/// `Header::SIZE + CallGraphHeader::SIZE + V5SectionHeader::SIZE` when
/// `header.version >= 6`. Absent from v3/v4/v5 files — those readers
/// skip to `Symbols` at `Header::SIZE + CallGraphHeader::SIZE + V5SectionHeader::SIZE`
/// (unchanged) while v6 readers now add `PatternSkeletonHeader::SIZE` on top.
///
/// Sub-sections:
/// - **skeletons**: flat array of fixed-size [`SkeletonRecord`] structs.
/// - **kind_path**: kind-name entries; each entry is
///   `[u16 depth][u32 string_pool_offset; depth]` pointing into the
///   *Strings* section already present in every index.
/// - **ident_pool**: length-prefixed UTF-8 identifier strings.
///   Each entry is `[u32 byte_len][UTF-8 bytes; byte_len]`.
/// - **file_index**: per-file lookup sorted by `file_id`.
///   Format: `[u32 count][{u32 file_id, u32 first_skeleton_idx}; count]`.
///   Binary-search on `file_id` gives `first_skeleton_idx` into the
///   skeleton records array; consecutive records until the next
///   `file_id` boundary belong to the same file.
/// - **grammar_fingerprints**: 32 `u32` slots (one per `lang_id`).
///   Slot 0 = unused. Non-zero means the slot was written.
///   Computed as `xxh3_64(...)` truncated to `u32` — see
///   [`crate::parse::language::Language::lang_id`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PatternSkeletonHeader {
    pub skeletons_offset: u64,
    pub skeletons_len: u64,
    pub kind_path_offset: u64,
    pub kind_path_len: u64,
    pub ident_pool_offset: u64,
    pub ident_pool_len: u64,
    pub file_index_offset: u64,
    pub file_index_len: u64,
    /// Grammar fingerprints indexed by `lang_id()`. Capacity: 32 slots.
    /// Each `u32` is `xxh3_64(...) as u32`, or 0 when not fingerprinted.
    pub grammar_fingerprints: [u32; 32],
}

impl PatternSkeletonHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// Section offsets and lengths for the v7-only sections (unresolved-by-name
/// reference edges, multi-repo Phase 6). Located in the file at exactly
/// `Header::SIZE + CallGraphHeader::SIZE + V5SectionHeader::SIZE +
/// PatternSkeletonHeader::SIZE` when `header.version >= 7`. Absent from
/// v3..v6 files.
///
/// These are the references a member's own Pass-2 (`writer.rs` `name_to_global`
/// loop) left **unresolved** — `Imported`/`Unresolved` targets whose name has
/// no definition in *this* index. They are dropped from the resolved
/// [`V5SectionHeader`] `RefEdge` section but persisted here keyed by **name**
/// so a workspace query can re-resolve them against a sibling member that does
/// define the symbol (gtags-style ordered fallback). Exactly 6 `u64` fields
/// (SIZE == 48), same shape as [`V5SectionHeader`] — DO NOT add fields without
/// updating the `symbols_offset` chain in `writer.rs`. Since v8 a
/// [`HierarchyHeader`] sits immediately downstream of this one in the
/// chain — this is no longer the last header before `symbols_offset`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UnresolvedRefsHeader {
    /// Raw fixed-size [`UnresolvedRef`] records (12 bytes each). Record
    /// count is `unresolved_edges_len / UnresolvedRef::SIZE`.
    pub unresolved_edges_offset: u64,
    pub unresolved_edges_len: u64,
    /// FST keyed on the **lowercased** referenced name. Values are u64
    /// offsets into `unresolved_postings`.
    pub unresolved_fst_offset: u64,
    pub unresolved_fst_len: u64,
    /// Posting lists: for each name key, a `[u32 count][u32 edge_idx;
    /// count]` block indexing into the `UnresolvedRef` records.
    pub unresolved_postings_offset: u64,
    pub unresolved_postings_len: u64,
}

impl UnresolvedRefsHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// Section offsets and lengths for the v8-only section (typed hierarchy
/// edges — `extends`/`implements`/`uses`, see `docs/HIERARCHY-EDGES.md`).
/// Located in the file at exactly `Header::SIZE + CallGraphHeader::SIZE +
/// V5SectionHeader::SIZE + PatternSkeletonHeader::SIZE +
/// UnresolvedRefsHeader::SIZE` when `header.version >= 8`. Absent from
/// v3..v7 files.
///
/// This is a fixed-size header **always written** (zeroed when the
/// section is empty — P1 never populates it, since extraction lands in
/// P2). Exactly 6 `u64` fields (SIZE == 48), same shape as
/// [`V5SectionHeader`] / [`UnresolvedRefsHeader`] — DO NOT add fields
/// without updating the `symbols_offset` chain in `writer.rs`.
///
/// Sub-sections:
/// - **edges**: sorted [`HierarchyEdge`][] records, sorted by `to_sym_idx`
///   (the resolved parent). Record count is `edges_len / HierarchyEdge::SIZE`.
/// - **index**: sorted [`HierarchyPostingEntry`][] records, also sorted by
///   `to_sym_idx` — binary-searched (NOT an FST; `to_sym_idx` is already a
///   dense array index, see `docs/HIERARCHY-EDGES.md` §3.4) to find the
///   posting-list offset for a given parent symbol.
/// - **postings**: for each distinct `to_sym_idx`, a `[u32 count][u32
///   edge_idx; count]` block indexing into the `edges` array — the CSR
///   posting lists enumerating every child edge for that parent.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HierarchyHeader {
    pub edges_offset: u64,
    pub edges_len: u64,
    pub index_offset: u64,
    pub index_len: u64,
    pub postings_offset: u64,
    pub postings_len: u64,
}

impl HierarchyHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// One on-disk skeleton record (24 bytes, `#[repr(C)]`).
///
/// Fields are ordered to avoid implicit `#[repr(C)]` padding:
/// four `u32`s, then one `u32`, then `u16`, then two `u8`s.
///
/// `kind_path_offset` is a byte offset into the *kind_path* sub-section;
/// `kind_path_len` is the depth (number of kind-name entries for this
/// record, typically 1 or 2). `ident_offset` is a byte offset into the
/// *ident_pool*; `u32::MAX` means "no identifier". Bit 0 of `flags` is
/// `has_block`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkeletonRecord {
    pub file_id: u32,
    pub start_row: u32,
    pub end_row: u32,
    /// Byte offset into the kind_path arena where this record's path begins.
    pub kind_path_offset: u32,
    /// Byte offset into ident_pool; `u32::MAX` = no identifier.
    pub ident_offset: u32,
    /// Number of kind-name path entries (typically 1–2).
    pub kind_path_len: u16,
    /// bit 0 = has_block.
    pub flags: u8,
    pub _pad: u8,
}

impl SkeletonRecord {
    pub const SIZE: usize = std::mem::size_of::<Self>();
    pub const FLAG_HAS_BLOCK: u8 = 0x01;
}

/// One resolved reference edge. `to_sym_idx` is the global index into
/// the Symbols section; `from_file_id` indexes the file table; `line`
/// is 1-based. `col_and_kind` packs an 8-bit `RefKind` discriminant in
/// the upper byte and a 24-bit column in the lower three bytes —
/// sufficient for any source file under 16 MB of columns per line.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RefEdge {
    pub to_sym_idx: u32,
    pub from_file_id: u32,
    pub line: u32,
    pub col_and_kind: u32,
}

impl RefEdge {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    // ref_kind_bits / column unpack the `col_and_kind` bit-packed field.
    // Production consumers (`cmd_usages.rs`) only need `from_file_id` +
    // `line`; the integration suite exercises the bit layout via these
    // accessors, so deleting them would force tests to inline the same
    // shift constants. `pub` + `#[allow(dead_code)]` keeps the
    // documented layout the single source of truth.
    #[allow(dead_code)] // exercised by integration tests; documents the bit layout
    pub fn ref_kind_bits(&self) -> u8 {
        ((self.col_and_kind >> 24) & 0xFF) as u8
    }

    #[allow(dead_code)] // exercised by integration tests; documents the bit layout
    pub fn column(&self) -> u32 {
        self.col_and_kind & 0x00FF_FFFF
    }
}

/// One unresolved-by-name reference edge (v7+, multi-repo Phase 6). Same
/// shape as [`RefEdge`] minus `to_sym_idx` — the referenced name is the FST
/// key in [`UnresolvedRefsHeader`], not stored on the record. `from_file_id`
/// indexes the file table; `line` is 1-based; `col_and_kind` packs an 8-bit
/// `RefKind` discriminant in the upper byte and a 24-bit column in the lower
/// three bytes (identical to `RefEdge`). 12 bytes, `align_of == 4`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UnresolvedRef {
    pub from_file_id: u32,
    pub line: u32,
    pub col_and_kind: u32,
}

impl UnresolvedRef {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    #[allow(dead_code)] // exercised by integration tests; documents the bit layout
    pub fn ref_kind_bits(&self) -> u8 {
        ((self.col_and_kind >> 24) & 0xFF) as u8
    }

    #[allow(dead_code)] // exercised by integration tests; documents the bit layout
    pub fn column(&self) -> u32 {
        self.col_and_kind & 0x00FF_FFFF
    }
}

/// Typed hierarchy edge kind (`extends` / `implements` / trait-mixin
/// `uses`), Kythe-style rather than SCIP-style — see
/// `docs/HIERARCHY-EDGES.md` §3.2. This is the **in-memory decode type
/// only**; the on-disk field is always the raw packed `u32`
/// ([`HierarchyEdge::line_and_kind`]), decoded via [`TryFrom<u8>`],
/// **never `mem::transmute`** — a reserved/unknown byte (3..=255, which
/// will appear once `Overrides`/`Satisfies` land, or from a corrupt file)
/// must decode to "unknown kind", never undefined behaviour.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // P1 format scaffold — no extraction pipeline emits this yet (P2)
pub enum EdgeKind {
    /// Nominal class inheritance (Rust supertrait, Python base,
    /// Java/TS/C#/Kotlin/Swift/C++ `extends`, Ruby `<`).
    Extends = 0,
    /// Nominal interface conformance (Java/C#/Kotlin/TS `implements`).
    Implements = 1,
    /// Trait/mixin composition (PHP `use`, Ruby include/extend/prepend).
    Uses = 2,
    // reserved 3..=254 — Overrides, Satisfies (Go structural) added
    // later, no format bump required (see §3.2 / §9).
}

impl TryFrom<u8> for EdgeKind {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(EdgeKind::Extends),
            1 => Ok(EdgeKind::Implements),
            2 => Ok(EdgeKind::Uses),
            _ => Err(()),
        }
    }
}

/// One typed hierarchy edge (v8+, format-only in P1 — the section is
/// empty until P2 wires extraction). `to_sym_idx` is the resolved
/// **parent** (supertype/interface) symbol index — the CSR grouping key
/// the section is sorted and indexed by. `from_sym_idx` is the resolved
/// **child** (subtype/implementer) symbol index, kept (unlike
/// [`RefEdge`], which only keeps the site) because a hierarchy query's
/// useful output is a symbol name, not just a file:line — see
/// `docs/HIERARCHY-EDGES.md` §3.3 Q5. `from_file_id` indexes the file
/// table for the file where the `extends`/`implements` clause lives.
///
/// `line_and_kind` packs an 8-bit [`EdgeKind`] discriminant in the top
/// byte and a 24-bit 1-based line number in the low three bytes — same
/// bit layout as [`RefEdge::col_and_kind`], decode via
/// [`HierarchyEdge::edge_kind_bits`] / [`HierarchyEdge::line`], and
/// [`EdgeKind::try_from`] for the safe enum decode (never
/// `mem::transmute`). Unlike `RefEdge`'s 24-bit *column* (unreachable in
/// real source), a 24-bit *line* ceiling (16,777,215) is reachable on
/// generated/adversarial files, so the builder
/// (`hierarchy_edges::pack_line_and_kind`) rejects an over-cap line with
/// a real `Result` error rather than silently truncating.
///
/// 16 bytes, `#[repr(C)]`, four `u32` fields (align 4, no padding).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HierarchyEdge {
    pub to_sym_idx: u32,
    pub from_sym_idx: u32,
    pub from_file_id: u32,
    pub line_and_kind: u32,
}

impl HierarchyEdge {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    /// Raw `EdgeKind` discriminant byte. Decode via
    /// `EdgeKind::try_from(edge.edge_kind_bits())` — never
    /// `mem::transmute` — since an unrecognised value (reserved for
    /// future kinds, or corrupt input) must degrade to "unknown kind",
    /// not undefined behaviour.
    #[allow(dead_code)] // exercised by integration/reader tests; documents the bit layout
    pub fn edge_kind_bits(&self) -> u8 {
        (self.line_and_kind >> 24) as u8
    }

    /// 1-based line number, unpacked from the low 24 bits.
    #[allow(dead_code)] // exercised by integration/reader tests; documents the bit layout
    pub fn line(&self) -> u32 {
        self.line_and_kind & 0x00FF_FFFF
    }
}

/// One entry in the sorted [`HierarchyHeader`] index sub-section (v8+).
/// `to_sym_idx` is the parent symbol index (the CSR grouping key,
/// matching [`HierarchyEdge::to_sym_idx`]); `posting_offset` is the byte
/// offset into the postings blob where that parent's `[u32 count][u32
/// edge_idx; count]` block begins.
///
/// This is a **plain sorted array binary-searched with
/// `partition_point`**, deliberately NOT an FST — `to_sym_idx` is already
/// a dense array index into the Symbols section, so an FST (which buys
/// prefix compression and fuzzy/range queries neither of which apply to
/// a dense integer key) would be strictly worse. See
/// `docs/HIERARCHY-EDGES.md` §3.4 for the locked rationale.
///
/// 8 bytes, `#[repr(C)]`, two `u32` fields (align 4, no padding).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HierarchyPostingEntry {
    pub to_sym_idx: u32,
    pub posting_offset: u32,
}

impl HierarchyPostingEntry {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// One caller → callee edge. The caller is identified by `caller_sym_idx`
/// (an index into the Symbols section, resolving to the enclosing function
/// definition). The callee is stored as a string offset into the Strings
/// section because vex may call functions defined outside the index
/// (stdlib, dependencies) for which no symbol record exists.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CallEdge {
    pub caller_sym_idx: u32,
    pub callee_name_offset: u32,
    pub line: u32,
    pub _pad: u32,
}

impl CallEdge {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// Fixed-size symbol record stored in the symbols section.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SymbolRecord {
    pub name_offset: u32,
    pub kind: u8,
    pub _pad: [u8; 3],
    pub file_offset: u32,
    pub line: u32,
    pub signature_offset: u32,
    pub vector_index: u32,
}

impl SymbolRecord {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_header_is_forty_eight_bytes() {
        // Pinned: 6 × u64, same shape as V5SectionHeader /
        // UnresolvedRefsHeader. Adding fields would silently drift
        // `symbols_offset` in writer.rs.
        assert_eq!(HierarchyHeader::SIZE, 48);
    }

    #[test]
    fn hierarchy_edge_is_sixteen_bytes() {
        assert_eq!(HierarchyEdge::SIZE, 16);
        assert_eq!(std::mem::align_of::<HierarchyEdge>(), 4);
    }

    #[test]
    fn hierarchy_posting_entry_is_eight_bytes() {
        assert_eq!(HierarchyPostingEntry::SIZE, 8);
        assert_eq!(std::mem::align_of::<HierarchyPostingEntry>(), 4);
    }

    #[test]
    fn edge_kind_try_from_decodes_known_values() {
        assert_eq!(EdgeKind::try_from(0u8), Ok(EdgeKind::Extends));
        assert_eq!(EdgeKind::try_from(1u8), Ok(EdgeKind::Implements));
        assert_eq!(EdgeKind::try_from(2u8), Ok(EdgeKind::Uses));
    }

    #[test]
    fn edge_kind_try_from_rejects_reserved_and_corrupt_values() {
        assert!(EdgeKind::try_from(3u8).is_err());
        assert!(EdgeKind::try_from(255u8).is_err());
    }
}
