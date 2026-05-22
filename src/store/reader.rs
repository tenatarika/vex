use std::path::Path;

use anyhow::{bail, Context, Result};
use memmap2::Mmap;

use super::format::{
    CallEdge, CallGraphHeader, Header, PatternSkeletonHeader, SymbolRecord, V5SectionHeader,
};

/// Memory-mapped index reader. Zero-copy access to symbols and strings.
pub struct IndexReader {
    mmap: Mmap,
}

impl IndexReader {
    /// Open and mmap the index file.
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("open index file at {}", path.display()))?;
        // SAFETY: the file is opened read-only. The mmap is not modified after creation.
        // The Mmap is owned by IndexReader and lives as long as all references to it.
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("mmap index file at {}", path.display()))?;

        let reader = Self { mmap };
        // All validation failures point at the same file path so the user
        // can act on the message (delete the file, re-run `vex index`)
        // without having to dig through stderr for the cache location.
        let p = path.display();

        if reader.mmap.len() < Header::SIZE {
            bail!(
                "index file at {p} is too small ({} bytes, need at least {}). Re-run `vex index` to rebuild.",
                reader.mmap.len(),
                Header::SIZE
            );
        }

        let header = reader.header();
        if &header.magic != super::format::MAGIC {
            bail!("index file at {p} is corrupted (bad magic). Re-run `vex index` to rebuild.");
        }
        // Accept any version in [MIN_SUPPORTED_VERSION ..= VERSION], plus
        // legacy v2 (pre-FST). `has_symbol_fst()` / `has_call_graph_header()`
        // gate the optional sections that may be missing in older versions.
        let v = header.version;
        let supported =
            v == 2 || (super::format::MIN_SUPPORTED_VERSION..=super::format::VERSION).contains(&v);
        if !supported {
            bail!(
                "index version mismatch at {p} (found v{}, this build supports v2 or v{}..v{}). Re-run `vex index` to rebuild.",
                v,
                super::format::MIN_SUPPORTED_VERSION,
                super::format::VERSION
            );
        }

        // Validate that claimed sections fit within the file
        let mmap_len = reader.mmap.len() as u64;
        let sym_end = header.symbols_offset.saturating_add(
            header
                .symbol_count
                .saturating_mul(SymbolRecord::SIZE as u64),
        );
        if sym_end > mmap_len {
            bail!(
                "index file at {p} is truncated (claims {} symbols but file too small). Re-run `vex index` to rebuild.",
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
            bail!("index file at {p} is corrupted (section offsets exceed file size). Re-run `vex index` to rebuild.");
        }

        // v4: validate CallGraphHeader fits AND its sections fit. Reuse the
        // same accessor we expose externally so the validation logic
        // matches the read path.
        if header.has_call_graph_header() {
            if (Header::SIZE + CallGraphHeader::SIZE) > reader.mmap.len() {
                bail!("v4 index at {p} is truncated (no room for CallGraphHeader). Re-run `vex index` to rebuild.");
            }
            // v5: V5SectionHeader sits directly after CallGraphHeader; if
            // the file claims v5 the bytes must fit.
            if header.has_v5_section_header()
                && (Header::SIZE + CallGraphHeader::SIZE + V5SectionHeader::SIZE)
                    > reader.mmap.len()
            {
                bail!("v5 index at {p} is truncated (no room for V5SectionHeader). Re-run `vex index` to rebuild.");
            }
            // v6: PatternSkeletonHeader sits directly after V5SectionHeader.
            if header.has_pattern_skeleton_header()
                && (Header::SIZE
                    + CallGraphHeader::SIZE
                    + V5SectionHeader::SIZE
                    + PatternSkeletonHeader::SIZE)
                    > reader.mmap.len()
            {
                bail!("v6 index at {p} is truncated (no room for PatternSkeletonHeader). Re-run `vex index` to rebuild.");
            }
            if let Some(psh) = reader.pattern_skeleton_header() {
                let mmap_len = reader.mmap.len() as u64;
                let skel_end = psh.skeletons_offset.saturating_add(psh.skeletons_len);
                let kp_end = psh.kind_path_offset.saturating_add(psh.kind_path_len);
                let ip_end = psh.ident_pool_offset.saturating_add(psh.ident_pool_len);
                let fi_end = psh.file_index_offset.saturating_add(psh.file_index_len);
                if skel_end > mmap_len
                    || kp_end > mmap_len
                    || ip_end > mmap_len
                    || fi_end > mmap_len
                {
                    bail!("v6 index at {p} is corrupted (pattern_skeleton section offsets exceed file size). Re-run `vex index` to rebuild.");
                }
            }
            if let Some(v5) = reader.v5_section_header() {
                let edges_end = v5.ref_edges_offset.saturating_add(v5.ref_edges_len);
                let fst_end = v5.ref_edges_fst_offset.saturating_add(v5.ref_edges_fst_len);
                let post_end = v5
                    .ref_edges_postings_offset
                    .saturating_add(v5.ref_edges_postings_len);
                if edges_end > mmap_len || fst_end > mmap_len || post_end > mmap_len {
                    bail!("v5 index at {p} is corrupted (reference_edges section offsets exceed file size). Re-run `vex index` to rebuild.");
                }
            }
            if let Some(cg) = reader.call_graph_header() {
                let edges_end = cg.call_edges_offset.saturating_add(cg.call_edges_len);
                let cers_fst_end = cg.callers_fst_offset.saturating_add(cg.callers_fst_len);
                let cers_post_end = cg
                    .callers_postings_offset
                    .saturating_add(cg.callers_postings_len);
                let cees_fst_end = cg.callees_fst_offset.saturating_add(cg.callees_fst_len);
                let cees_post_end = cg
                    .callees_postings_offset
                    .saturating_add(cg.callees_postings_len);
                let bm25_fst_end = cg.bm25_fst_offset.saturating_add(cg.bm25_fst_len);
                let bm25_post_end = cg.bm25_postings_offset.saturating_add(cg.bm25_postings_len);
                let bm25_stats_end = cg.bm25_stats_offset.saturating_add(cg.bm25_stats_len);
                if edges_end > mmap_len
                    || cers_fst_end > mmap_len
                    || cers_post_end > mmap_len
                    || cees_fst_end > mmap_len
                    || cees_post_end > mmap_len
                    || bm25_fst_end > mmap_len
                    || bm25_post_end > mmap_len
                    || bm25_stats_end > mmap_len
                {
                    bail!("v4 index at {p} is corrupted (call-graph or bm25 section offsets exceed file size). Re-run `vex index` to rebuild.");
                }
            }
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

    /// Read the v4 [`CallGraphHeader`] when present. Returns `None` for v3
    /// indexes or when the bytes after the base header don't fit a
    /// `CallGraphHeader` (corrupt file).
    pub fn call_graph_header(&self) -> Option<&CallGraphHeader> {
        if !self.header().has_call_graph_header() {
            return None;
        }
        let offset = Header::SIZE;
        let end = offset.checked_add(CallGraphHeader::SIZE)?;
        if end > self.mmap.len() {
            return None;
        }
        let ptr = unsafe { self.mmap.as_ptr().add(offset) };
        if ptr.align_offset(std::mem::align_of::<CallGraphHeader>()) != 0 {
            return None;
        }
        // SAFETY: bounds + alignment checked. CallGraphHeader is #[repr(C)].
        Some(unsafe { &*(ptr as *const CallGraphHeader) })
    }

    /// Whether the index carries call-graph data we can query directly.
    /// False for v3 indexes and for v4 indexes that recorded zero edges.
    pub fn has_call_graph(&self) -> bool {
        self.call_graph_header()
            .is_some_and(|h| h.call_edges_len > 0)
    }

    /// Read the v5 [`V5SectionHeader`] when present. Returns `None` for
    /// v3/v4 indexes or when the bytes after the `CallGraphHeader` don't
    /// fit a `V5SectionHeader`.
    pub fn v5_section_header(&self) -> Option<&V5SectionHeader> {
        if !self.header().has_v5_section_header() {
            return None;
        }
        let offset = Header::SIZE.checked_add(CallGraphHeader::SIZE)?;
        let end = offset.checked_add(V5SectionHeader::SIZE)?;
        if end > self.mmap.len() {
            return None;
        }
        let ptr = unsafe { self.mmap.as_ptr().add(offset) };
        if ptr.align_offset(std::mem::align_of::<V5SectionHeader>()) != 0 {
            return None;
        }
        // SAFETY: bounds + alignment checked. V5SectionHeader is #[repr(C)].
        Some(unsafe { &*(ptr as *const V5SectionHeader) })
    }

    /// Read the v6 [`PatternSkeletonHeader`] when present. Returns `None`
    /// for v3/v4/v5 indexes or when the bytes don't fit.
    pub fn pattern_skeleton_header(&self) -> Option<&PatternSkeletonHeader> {
        if !self.header().has_pattern_skeleton_header() {
            return None;
        }
        let offset = Header::SIZE
            .checked_add(CallGraphHeader::SIZE)?
            .checked_add(V5SectionHeader::SIZE)?;
        let end = offset.checked_add(PatternSkeletonHeader::SIZE)?;
        if end > self.mmap.len() {
            return None;
        }
        let ptr = unsafe { self.mmap.as_ptr().add(offset) };
        if ptr.align_offset(std::mem::align_of::<PatternSkeletonHeader>()) != 0 {
            return None;
        }
        // SAFETY: bounds + alignment checked. PatternSkeletonHeader is #[repr(C)].
        Some(unsafe { &*(ptr as *const PatternSkeletonHeader) })
    }

    /// Construct a [`PatternSkeletonReader`] for the v6 skeleton section.
    /// Returns `None` for v3/v4/v5 indexes (no section present).
    /// Returns `Some` even when the section is empty (zero-length records).
    pub fn pattern_skeleton_reader(
        &self,
    ) -> Option<super::pattern_skeletons::PatternSkeletonReader<'_>> {
        let psh = self.pattern_skeleton_header()?;
        let mmap = &self.mmap[..];
        let skel = slice_or_empty(
            mmap,
            psh.skeletons_offset as usize,
            psh.skeletons_len as usize,
        )?;
        let kp = slice_or_empty(
            mmap,
            psh.kind_path_offset as usize,
            psh.kind_path_len as usize,
        )?;
        let ip = slice_or_empty(
            mmap,
            psh.ident_pool_offset as usize,
            psh.ident_pool_len as usize,
        )?;
        let fi = slice_or_empty(
            mmap,
            psh.file_index_offset as usize,
            psh.file_index_len as usize,
        )?;
        super::pattern_skeletons::PatternSkeletonReader::new(
            skel,
            kp,
            ip,
            fi,
            psh.grammar_fingerprints,
        )
        .ok()
    }

    /// Whether the index carries scope-resolved reference edges. False
    /// for v3/v4 indexes (no v5 section header) and v5 indexes that
    /// were built before 11.1.3b wired the binder into the writer.
    #[allow(dead_code)] // becomes the dispatch knob in 11.1.3d
    pub fn has_ref_edges(&self) -> bool {
        self.v5_section_header()
            .is_some_and(|h| h.ref_edges_len > 0)
    }

    #[allow(dead_code)] // used by `find_ref_edges_by_symbol` once 11.1.3d wires the CLI
    fn ref_edges_section_bytes(&self) -> Option<(&[u8], &[u8], &[u8])> {
        let v5 = self.v5_section_header()?;
        let mmap = &self.mmap[..];
        let edges = slice_or_empty(
            mmap,
            v5.ref_edges_offset as usize,
            v5.ref_edges_len as usize,
        )?;
        let fst = slice_or_empty(
            mmap,
            v5.ref_edges_fst_offset as usize,
            v5.ref_edges_fst_len as usize,
        )?;
        let post = slice_or_empty(
            mmap,
            v5.ref_edges_postings_offset as usize,
            v5.ref_edges_postings_len as usize,
        )?;
        Some((edges, fst, post))
    }

    /// Look up every persisted reference edge whose `to_sym_idx`
    /// matches `sym_idx`. Returns an empty `Vec` when the index has no
    /// ref-edges section, when the FST is missing the key, or when the
    /// section bytes don't validate.
    #[allow(dead_code)] // wired into the CLI dispatch in 11.1.3d
    pub fn find_ref_edges_by_symbol(&self, sym_idx: u32) -> Vec<super::format::RefEdge> {
        if !self.has_ref_edges() {
            return Vec::new();
        }
        let Some((edges, fst, post)) = self.ref_edges_section_bytes() else {
            return Vec::new();
        };
        let Ok(reader) = super::ref_edges::RefEdgeReader::new(fst, post, edges) else {
            return Vec::new();
        };
        reader.find_by_symbol_idx(sym_idx)
    }

    /// Number of call edges recorded in this index, 0 when absent.
    pub fn call_edge_count(&self) -> usize {
        self.call_graph_header()
            .map(|h| (h.call_edges_len as usize) / CallEdge::SIZE)
            .unwrap_or(0)
    }

    /// Get a call-edge record by index. Returns `None` when out of bounds
    /// or when this index has no call graph.
    pub fn call_edge(&self, idx: usize) -> Option<&CallEdge> {
        let h = self.call_graph_header()?;
        if idx >= self.call_edge_count() {
            return None;
        }
        let offset = (h.call_edges_offset as usize).checked_add(idx * CallEdge::SIZE)?;
        let end = offset.checked_add(CallEdge::SIZE)?;
        if end > self.mmap.len() {
            return None;
        }
        let ptr = unsafe { self.mmap.as_ptr().add(offset) };
        if ptr.align_offset(std::mem::align_of::<CallEdge>()) != 0 {
            return None;
        }
        // SAFETY: bounds + alignment checked. CallEdge is #[repr(C)].
        Some(unsafe { &*(ptr as *const CallEdge) })
    }

    /// Raw bytes of the callers FST section (or empty when absent).
    pub fn callers_fst_bytes(&self) -> &[u8] {
        let Some(h) = self.call_graph_header() else {
            return &[];
        };
        let start = h.callers_fst_offset as usize;
        let end = start + h.callers_fst_len as usize;
        if end > self.mmap.len() {
            return &[];
        }
        &self.mmap[start..end]
    }

    /// Raw bytes of the callers posting list section.
    pub fn callers_posting_bytes(&self) -> &[u8] {
        let Some(h) = self.call_graph_header() else {
            return &[];
        };
        let start = h.callers_postings_offset as usize;
        let end = start + h.callers_postings_len as usize;
        if end > self.mmap.len() {
            return &[];
        }
        &self.mmap[start..end]
    }

    /// Raw bytes of the callees FST section.
    pub fn callees_fst_bytes(&self) -> &[u8] {
        let Some(h) = self.call_graph_header() else {
            return &[];
        };
        let start = h.callees_fst_offset as usize;
        let end = start + h.callees_fst_len as usize;
        if end > self.mmap.len() {
            return &[];
        }
        &self.mmap[start..end]
    }

    /// Raw bytes of the callees posting list section.
    pub fn callees_posting_bytes(&self) -> &[u8] {
        let Some(h) = self.call_graph_header() else {
            return &[];
        };
        let start = h.callees_postings_offset as usize;
        let end = start + h.callees_postings_len as usize;
        if end > self.mmap.len() {
            return &[];
        }
        &self.mmap[start..end]
    }

    /// Whether the index carries BM25 channel data (Phase 9.4).
    /// False for v3 indexes and for v4 indexes built without BM25.
    ///
    /// The gate is `bm25_stats_len >= 8` (minimum valid stats: a `doc_count`
    /// u32 plus an `avg_doc_len` f32 header) so any caller seeing
    /// `has_bm25() == true` can safely construct `Bm25Reader`.
    pub fn has_bm25(&self) -> bool {
        self.call_graph_header()
            .is_some_and(|h| h.bm25_stats_len >= 8)
    }

    /// Raw bytes of the BM25 FST section (empty when BM25 is absent).
    pub fn bm25_fst_bytes(&self) -> &[u8] {
        let Some(h) = self.call_graph_header() else {
            return &[];
        };
        let start = h.bm25_fst_offset as usize;
        let end = start + h.bm25_fst_len as usize;
        if end > self.mmap.len() {
            return &[];
        }
        &self.mmap[start..end]
    }

    /// Raw bytes of the BM25 postings section.
    pub fn bm25_posting_bytes(&self) -> &[u8] {
        let Some(h) = self.call_graph_header() else {
            return &[];
        };
        let start = h.bm25_postings_offset as usize;
        let end = start + h.bm25_postings_len as usize;
        if end > self.mmap.len() {
            return &[];
        }
        &self.mmap[start..end]
    }

    /// Raw bytes of the BM25 stats section.
    pub fn bm25_stats_bytes(&self) -> &[u8] {
        let Some(h) = self.call_graph_header() else {
            return &[];
        };
        let start = h.bm25_stats_offset as usize;
        let end = start + h.bm25_stats_len as usize;
        if end > self.mmap.len() {
            return &[];
        }
        &self.mmap[start..end]
    }
}

/// Bounded slice access used by the v5 ref-edges read path. Returns
/// `Some(&[])` when `len == 0` (legitimate empty subsection) and
/// `None` when `offset + len` would walk past the end of the mmap
/// (corrupt index — caller treats this as "no ref edges available").
#[allow(dead_code)] // used by `ref_edges_section_bytes` once 11.1.3d wires the CLI
fn slice_or_empty(mmap: &[u8], offset: usize, len: usize) -> Option<&[u8]> {
    if len == 0 {
        return Some(&[]);
    }
    let end = offset.checked_add(len)?;
    if end > mmap.len() {
        return None;
    }
    Some(&mmap[offset..end])
}
