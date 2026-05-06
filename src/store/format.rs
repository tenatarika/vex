//! Binary index file format specification.
//!
//! Layout:
//! ```text
//! [Header]           64 bytes   - magic, version, counts, section offsets
//! [Symbols Section]  variable   - fixed-size symbol records
//! [Vectors Section]  variable   - dense f32 arrays (384-dim each)
//! [Strings Section]  variable   - deduplicated string pool
//! [Inverted Index]   variable   - name tokens -> symbol offsets
//! ```

pub const MAGIC: &[u8; 4] = b"VEXI";
pub const VERSION: u32 = 1;
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
}

impl Header {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    pub fn validate(&self) -> bool {
        self.magic == *MAGIC && self.version == VERSION
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
