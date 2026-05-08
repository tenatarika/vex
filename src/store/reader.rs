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

        if reader.mmap.len() < Header::SIZE {
            bail!(
                "index file is too small ({} bytes, need at least {}). Re-run `vex index` to rebuild.",
                reader.mmap.len(),
                Header::SIZE
            );
        }

        let header = reader.header();
        if &header.magic != super::format::MAGIC {
            bail!("index file is corrupted (bad magic). Re-run `vex index` to rebuild.");
        }
        // Accept v2 indexes written by vex ≤0.1.x (no symbol FST); has_symbol_fst() gates that section.
        if header.version != super::format::VERSION && header.version != 2 {
            bail!(
                "index version mismatch (found v{}, expected v{}). Re-run `vex index` to rebuild.",
                header.version,
                super::format::VERSION
            );
        }

        // Validate that claimed sections fit within the file
        let mmap_len = reader.mmap.len() as u64;
        let sym_end = header
            .symbols_offset
            .saturating_add(header.symbol_count.saturating_mul(SymbolRecord::SIZE as u64));
        if sym_end > mmap_len {
            bail!(
                "index file is truncated (claims {} symbols but file too small). Re-run `vex index` to rebuild.",
                header.symbol_count
            );
        }

        // Validate that all section offsets are within bounds
        let file_table_end = header
            .file_table_offset
            .saturating_add((header.file_table_count as u64).saturating_mul(4));
        let fst_end = header.fst_offset.saturating_add(header.fst_len);
        let postings_end = header.postings_offset.saturating_add(header.postings_len);
        let sym_fst_end = header.sym_fst_offset.saturating_add(header.sym_fst_len);
        let sym_post_end = header
            .sym_postings_offset
            .saturating_add(header.sym_postings_len);

        if file_table_end > mmap_len
            || fst_end > mmap_len
            || postings_end > mmap_len
            || sym_fst_end > mmap_len
            || sym_post_end > mmap_len
        {
            bail!("index file is corrupted (section offsets exceed file size). Re-run `vex index` to rebuild.");
        }

        Ok(reader)
    }

    pub fn header(&self) -> &Header {
        let ptr = self.mmap.as_ptr();
        debug_assert!(
            ptr.align_offset(std::mem::align_of::<Header>()) == 0,
            "mmap pointer is not aligned to Header (align={})",
            std::mem::align_of::<Header>()
        );
        // SAFETY: mmap is page-aligned (>= 4096 bytes, satisfies align_of::<Header>() == 8).
        // Header is #[repr(C)] with fixed layout. The mmap lives as long as &self.
        // File was validated in open() before external callers can use this.
        unsafe { &*(ptr as *const Header) }
    }

    /// Get symbol record by index.
    pub fn symbol(&self, idx: usize) -> Option<&SymbolRecord> {
        let header = self.header();
        if idx >= header.symbol_count as usize {
            return None;
        }
        let offset = header
            .symbols_offset
            .checked_add((idx * SymbolRecord::SIZE) as u64)? as usize;
        if offset + SymbolRecord::SIZE > self.mmap.len() {
            return None;
        }
        let ptr = unsafe { self.mmap.as_ptr().add(offset) };
        // Alignment check — required for safe pointer cast, not just a debug hint
        if ptr.align_offset(std::mem::align_of::<SymbolRecord>()) != 0 {
            return None;
        }
        // SAFETY: bounds and alignment checked above.
        // SymbolRecord is #[repr(C)] with fixed layout.
        // The mmap lives as long as &self.
        Some(unsafe { &*(ptr as *const SymbolRecord) })
    }

    /// Read a null-terminated string from the strings section.
    pub fn read_string(&self, offset: u32) -> &str {
        if offset == u32::MAX {
            return "";
        }
        let header = self.header();
        let base = match (header.strings_offset as usize).checked_add(offset as usize) {
            Some(b) => b,
            None => return "",
        };
        if base >= self.mmap.len() {
            return "";
        }
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
        let vec_byte_size = dim.checked_mul(std::mem::size_of::<f32>())?;
        let byte_offset = (vector_index as usize)
            .checked_mul(vec_byte_size)?
            .checked_add(header.vectors_offset as usize)?;
        let end = byte_offset.checked_add(vec_byte_size)?;
        if end > self.mmap.len() {
            return None;
        }

        let ptr = unsafe { self.mmap.as_ptr().add(byte_offset) };
        // Alignment check — required for safe pointer cast
        if ptr.align_offset(std::mem::align_of::<f32>()) != 0 {
            return None;
        }
        // SAFETY: bounds and alignment checked above.
        // Data was written by writer as valid f32 arrays.
        // The mmap lives as long as &self; no mutable references exist.
        Some(unsafe { std::slice::from_raw_parts(ptr as *const f32, dim) })
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
        let base = h.file_table_offset as usize;
        // Cap count to what actually fits in the mmap to avoid OOM on crafted headers
        let max_entries = self.mmap.len().saturating_sub(base) / 4;
        let count = (h.file_table_count as usize).min(max_entries);
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

    /// Create a SymbolFstReader for zero-copy symbol lookup.
    pub fn symbol_fst_reader(&self) -> Option<super::symbol_fst::SymbolFstReader<'_>> {
        let h = self.header();
        if !h.has_symbol_fst() {
            return None;
        }
        let fst_start = h.sym_fst_offset as usize;
        let fst_end = fst_start + h.sym_fst_len as usize;
        let post_start = h.sym_postings_offset as usize;
        let post_end = post_start + h.sym_postings_len as usize;

        if fst_end > self.mmap.len() || post_end > self.mmap.len() {
            return None;
        }

        let fst_bytes = &self.mmap[fst_start..fst_end];
        let posting_bytes = &self.mmap[post_start..post_end];
        super::symbol_fst::SymbolFstReader::new(fst_bytes, posting_bytes).ok()
    }

    /// Total number of indexed symbols.
    pub fn symbol_count(&self) -> usize {
        self.header().symbol_count as usize
    }
}
