//! Binary index file format specification.
//!
//! Layout (v4 — current):
//! ```text
//! [Header]             fixed (168 B) - magic, version, counts, section offsets
//! [CallGraphHeader]    fixed (80  B) - call graph section offsets (v4 only)
//! [Symbols Section]    variable      - fixed-size symbol records
//! [Vectors Section]    variable      - dense f32 arrays (vector_dim each)
//! [Strings Section]    variable      - deduplicated string pool
//! [Refs FST]           variable      - fst::Map bytes (ref name → posting offset)
//! [Refs Postings]      variable      - posting lists (count, [(file_id, line)])
//! [File Table]         variable      - u32 string offsets, one per file_id
//! [Symbol FST]         variable      - fst::Map bytes (name/token → sym posting offset)
//! [Symbol Postings]    variable      - posting lists (count, [symbol_idx])
//! [Call Edges]         variable      - fixed-size CallEdge records (v4 only)
//! [Callers FST]        variable      - fst::Map (callee name → edge-idx posting offset)
//! [Callers Postings]   variable      - posting lists (count, [edge_idx])
//! [Callees FST]        variable      - fst::Map (caller_sym_idx_str → posting offset)
//! [Callees Postings]   variable      - posting lists (count, [edge_idx])
//! ```
//!
//! Layout v3 (legacy, still readable): same as v4 minus `CallGraphHeader`
//! (the `Symbols Section` starts directly at `Header::SIZE`) and minus all
//! call-graph sections. The `Header` struct is byte-identical between v3
//! and v4 — version dispatch happens at the reader.

pub const MAGIC: &[u8; 4] = b"VEXI";
pub const VERSION: u32 = 5;
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
