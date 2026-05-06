//! Binary index file format specification.
//!
//! Layout (v2):
//! ```text
//! [Header]            fixed      - magic, version, counts, section offsets
//! [Symbols Section]   variable   - fixed-size symbol records
//! [Vectors Section]   variable   - dense f32 arrays (384-dim each)
//! [Strings Section]   variable   - deduplicated string pool
//! [FST Section]       variable   - fst::Map bytes (ref name → posting offset)
//! [Postings Section]  variable   - posting lists (count, [(file_id, line)])
//! [File Table]        variable   - u32 count + count × u32 string offsets
//! ```

pub const MAGIC: &[u8; 4] = b"VEXI";
pub const VERSION: u32 = 2;
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
    pub fst_offset: u64,
    pub fst_len: u64,
    pub postings_offset: u64,
    pub postings_len: u64,
    pub file_table_offset: u64,
    pub file_table_count: u32,
    pub _padding2: u32,
}

impl Header {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    pub fn validate(&self) -> bool {
        self.magic == *MAGIC && self.version == VERSION
    }

    pub fn has_refs(&self) -> bool {
        self.fst_len > 0
    }
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
