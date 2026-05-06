use std::path::Path;

use anyhow::{bail, Context, Result};
use memmap2::Mmap;

use super::format::{Header, SymbolRecord};

/// Memory-mapped index reader. Zero-copy access to symbols and strings.
pub struct IndexReader {
    mmap: Mmap,
}

impl IndexReader {
    /// Open and mmap the index file.
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).context("open index file")?;
        // SAFETY: the file is opened read-only. The mmap is not modified after creation.
        // The Mmap is owned by IndexReader and lives as long as all references to it.
        let mmap = unsafe { Mmap::map(&file) }.context("mmap index file")?;

        let reader = Self { mmap };
        let header = reader.header();
        if !header.validate() {
            bail!(
                "index version mismatch (found v{}, expected v{}). Re-run `vex index` to rebuild.",
                header.version,
                super::format::VERSION
            );
        }

        Ok(reader)
    }

    pub fn header(&self) -> &Header {
        // SAFETY: mmap is page-aligned (>= 4096 bytes, satisfies align_of::<Header>() == 8).
        // Header is #[repr(C)] with fixed layout. The mmap lives as long as &self.
        // File was validated in open() before external callers can use this.
        unsafe { &*(self.mmap.as_ptr() as *const Header) }
    }

    /// Get symbol record by index.
    pub fn symbol(&self, idx: usize) -> Option<&SymbolRecord> {
        let header = self.header();
        if idx >= header.symbol_count as usize {
            return None;
        }
        let offset = header.symbols_offset as usize + idx * SymbolRecord::SIZE;
        // SAFETY: bounds checked above. SymbolRecord is #[repr(C)] with 4-byte alignment.
        // symbols_offset is Header::SIZE (divisible by 4), SymbolRecord::SIZE is divisible by 4.
        // The mmap lives as long as &self.
        Some(unsafe { &*(self.mmap.as_ptr().add(offset) as *const SymbolRecord) })
    }

    /// Read a null-terminated string from the strings section.
    pub fn read_string(&self, offset: u32) -> &str {
        if offset == u32::MAX {
            return "";
        }
        let header = self.header();
        let base = header.strings_offset as usize + offset as usize;
        let data = &self.mmap[base..];
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        std::str::from_utf8(&data[..end]).unwrap_or("")
    }

    /// Read the embedding vector for a symbol by its vector_index.
    /// Returns a slice of f32 with length = vector_dim (384).
    pub fn vector(&self, vector_index: u32) -> Option<&[f32]> {
        if vector_index == u32::MAX {
            return None;
        }
        let header = self.header();
        let dim = header.vector_dim as usize;
        let byte_offset = header.vectors_offset as usize
            + vector_index as usize * dim * std::mem::size_of::<f32>();

        let end = byte_offset + dim * std::mem::size_of::<f32>();
        if end > self.mmap.len() {
            return None;
        }

        // SAFETY: ptr is 4-byte aligned (mmap is page-aligned, Header::SIZE % 4 == 0,
        // SymbolRecord::SIZE % 4 == 0, so vectors_offset is always divisible by 4).
        // Data was written by writer as valid f32 arrays. Bounds checked above.
        // The mmap lives as long as &self; no mutable references exist.
        let ptr = unsafe { self.mmap.as_ptr().add(byte_offset) as *const f32 };
        Some(unsafe { std::slice::from_raw_parts(ptr, dim) })
    }

    /// Whether the index contains embedding vectors.
    /// True when vectors_offset != strings_offset (i.e. there are bytes between them).
    pub fn has_vectors(&self) -> bool {
        let h = self.header();
        h.vectors_offset != h.strings_offset
    }

    /// Whether the index contains refs FST.
    pub fn has_refs(&self) -> bool {
        self.header().has_refs()
    }

    /// Get raw FST bytes slice from mmap.
    pub fn fst_bytes(&self) -> &[u8] {
        let h = self.header();
        let start = h.fst_offset as usize;
        let end = start + h.fst_len as usize;
        if end > self.mmap.len() {
            return &[];
        }
        &self.mmap[start..end]
    }

    /// Get raw posting list bytes slice from mmap.
    pub fn posting_bytes(&self) -> &[u8] {
        let h = self.header();
        let start = h.postings_offset as usize;
        let end = start + h.postings_len as usize;
        if end > self.mmap.len() {
            return &[];
        }
        &self.mmap[start..end]
    }

    /// Create a RefReader for zero-copy FST lookup of refs.
    pub fn ref_reader(&self) -> Option<super::refs_fst::RefReader<'_>> {
        if !self.has_refs() {
            return None;
        }
        super::refs_fst::RefReader::new(self.fst_bytes(), self.posting_bytes()).ok()
    }

    /// Read file_id → path mapping from the file table section.
    /// The file table stores u32 string offsets, one per file_id, written by the writer.
    pub fn file_paths(&self) -> Vec<String> {
        let h = self.header();
        let count = h.file_table_count as usize;
        let base = h.file_table_offset as usize;
        let mut paths = Vec::with_capacity(count);

        for i in 0..count {
            let offset = base + i * 4;
            if offset + 4 > self.mmap.len() {
                break;
            }
            let str_offset =
                u32::from_le_bytes(self.mmap[offset..offset + 4].try_into().unwrap_or([0; 4]));
            paths.push(self.read_string(str_offset).to_string());
        }
        paths
    }

    /// Total number of indexed symbols.
    pub fn symbol_count(&self) -> usize {
        self.header().symbol_count as usize
    }
}
