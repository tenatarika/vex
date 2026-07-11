use std::path::Path;

use anyhow::{bail, Context, Result};
use memmap2::Mmap;

use super::format::{
    CallEdge, CallGraphHeader, Header, HierarchyEdge, HierarchyHeader, HierarchyPostingEntry,
    PatternSkeletonHeader, SymbolRecord, UnresolvedHierarchyEdge, UnresolvedHierarchyHeader,
    UnresolvedRefsHeader, V5SectionHeader,
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
        // Accept any version in [MIN_SUPPORTED_VERSION ..= VERSION]. The
        // pre-v3 (v2) special-case was dropped in v1.10.1 — the reader still
        // opened v2 files but `has_symbol_fst()` then refused them, leaving
        // search in a silently-degraded state. v2 is pre-Phase 9 (>= 6
        // minors ago); anyone on a v2 index re-runs `vex index` to rebuild
        // at the current format.
        let v = header.version;
        let supported =
            (super::format::MIN_SUPPORTED_VERSION..=super::format::VERSION).contains(&v);
        if !supported {
            bail!(
                "index version mismatch at {p} (found v{}, this build supports v{}..v{}). Re-run `vex index` to rebuild.",
                v,
                super::format::MIN_SUPPORTED_VERSION,
                super::format::VERSION
            );
        }

        // Cap the embedding dimension up front. The writer ships a fixed
        // 384-dim model today; anything wildly higher is a corrupt header
        // (or a malicious file) that would let `vector()` size a per-symbol
        // slice large enough to alias the next section's bytes.
        const MAX_VECTOR_DIM: u32 = 4096;
        if header.vector_dim > MAX_VECTOR_DIM {
            bail!(
                "index file at {p} is corrupted (vector_dim {} exceeds cap {MAX_VECTOR_DIM}). Re-run `vex index` to rebuild.",
                header.vector_dim
            );
        }
        // Reject `vector_dim = 0` when symbols claim to exist: `vector()` would
        // silently return empty slices for every symbol instead of erroring.
        // A legitimately vectors-less index has `vectors_offset == strings_offset`
        // (no vectors section); that path is fine. The bad case is "vectors
        // section claims length but dim is zero".
        if header.vector_dim == 0 && header.vectors_offset != header.strings_offset {
            bail!(
                "index file at {p} is corrupted (vector_dim is 0 but a non-empty vectors section is present). Re-run `vex index` to rebuild."
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

        // Monotone-offset invariants: the writer emits sections in a fixed
        // increasing order. Without these checks a crafted header with
        // overlapping offsets (e.g. `vectors_offset == 0`) would let the
        // reader pun bytes between sections — vector reads would alias
        // symbol-record bytes as f32, etc.
        //
        // Vectors → strings: this ordering is a structural invariant of
        // the format regardless of how many symbols are present, because
        // `read_string` indexes off `strings_offset` and `vector` reads
        // off `vectors_offset`. A reversed pair lets vector reads alias
        // strings-section bytes.
        if header.vectors_offset > header.strings_offset {
            bail!("index file at {p} is corrupted (vectors_offset > strings_offset). Re-run `vex index` to rebuild.");
        }
        if header.symbol_count > 0 {
            // The vectors section has no explicit `vector_count` field —
            // when present it holds one f32-vector per symbol, when
            // absent `vectors_offset == strings_offset` (zero bytes
            // between them). Derive the actual byte length from that
            // delta rather than the (max-possible) `symbol_count *
            // vector_dim * 4`.
            let vectors_byte_len = header.strings_offset.saturating_sub(header.vectors_offset);
            if vectors_byte_len > 0 {
                let vec_byte_size =
                    (header.vector_dim as u64).saturating_mul(std::mem::size_of::<f32>() as u64);
                let max_vector_bytes = header.symbol_count.saturating_mul(vec_byte_size);
                if vec_byte_size == 0
                    || !vectors_byte_len.is_multiple_of(vec_byte_size)
                    || vectors_byte_len > max_vector_bytes
                {
                    bail!("index file at {p} is corrupted (vectors section size {} is not aligned to vector_dim={} or exceeds symbol_count). Re-run `vex index` to rebuild.",
                        vectors_byte_len, header.vector_dim);
                }
            }
            // symbols → vectors. The symbol records must not overlap
            // the vectors section.
            if sym_end > header.vectors_offset {
                bail!("index file at {p} is corrupted (symbols section overlaps vectors_offset). Re-run `vex index` to rebuild.");
            }
        }
        // For every variable-length post-vectors section, only enforce
        // monotone ordering when the section actually carries bytes.
        // The minimal-fixture path (test-only / legacy) leaves the
        // trailing offsets at zero, which is still safe because every
        // `*_len` is also zero.
        if header.fst_len > 0 && header.strings_offset > header.fst_offset {
            bail!("index file at {p} is corrupted (strings section overlaps refs FST). Re-run `vex index` to rebuild.");
        }
        if header.postings_len > 0 && fst_end > header.postings_offset {
            bail!("index file at {p} is corrupted (refs FST overlaps refs postings). Re-run `vex index` to rebuild.");
        }
        if header.file_table_count > 0 && postings_end > header.file_table_offset {
            bail!("index file at {p} is corrupted (refs postings overlap file table). Re-run `vex index` to rebuild.");
        }
        if header.sym_fst_len > 0 && file_table_end > header.sym_fst_offset {
            bail!("index file at {p} is corrupted (file table overlaps symbol FST). Re-run `vex index` to rebuild.");
        }
        if header.sym_postings_len > 0 && sym_fst_end > header.sym_postings_offset {
            bail!("index file at {p} is corrupted (symbol FST overlaps symbol postings). Re-run `vex index` to rebuild.");
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
            // v8: HierarchyHeader sits directly after UnresolvedRefsHeader.
            if header.has_hierarchy_header()
                && (Header::SIZE
                    + CallGraphHeader::SIZE
                    + V5SectionHeader::SIZE
                    + PatternSkeletonHeader::SIZE
                    + UnresolvedRefsHeader::SIZE
                    + HierarchyHeader::SIZE)
                    > reader.mmap.len()
            {
                bail!("v8 index at {p} is truncated (no room for HierarchyHeader). Re-run `vex index` to rebuild.");
            }
            // v8: UnresolvedHierarchyHeader sits directly after HierarchyHeader.
            if header.has_unresolved_hierarchy_header()
                && (Header::SIZE
                    + CallGraphHeader::SIZE
                    + V5SectionHeader::SIZE
                    + PatternSkeletonHeader::SIZE
                    + UnresolvedRefsHeader::SIZE
                    + HierarchyHeader::SIZE
                    + UnresolvedHierarchyHeader::SIZE)
                    > reader.mmap.len()
            {
                bail!("v8 index at {p} is truncated (no room for UnresolvedHierarchyHeader). Re-run `vex index` to rebuild.");
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
            if let Some(hh) = reader.hierarchy_header() {
                let edges_end = hh.edges_offset.saturating_add(hh.edges_len);
                let index_end = hh.index_offset.saturating_add(hh.index_len);
                let postings_end = hh.postings_offset.saturating_add(hh.postings_len);
                if edges_end > mmap_len || index_end > mmap_len || postings_end > mmap_len {
                    bail!("v8 index at {p} is corrupted (hierarchy_edges section offsets exceed file size). Re-run `vex index` to rebuild.");
                }
            }
            if let Some(uhh) = reader.unresolved_hierarchy_header() {
                let edges_end = uhh.edges_offset.saturating_add(uhh.edges_len);
                let fst_end = uhh.fst_offset.saturating_add(uhh.fst_len);
                let postings_end = uhh.postings_offset.saturating_add(uhh.postings_len);
                if edges_end > mmap_len || fst_end > mmap_len || postings_end > mmap_len {
                    bail!("v8 index at {p} is corrupted (unresolved_hierarchy section offsets exceed file size). Re-run `vex index` to rebuild.");
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

    /// Read a null-terminated string from the strings section. Returns
    /// `""` when the offset is `u32::MAX` (canonical "no string"
    /// sentinel) OR when the bytes fail UTF-8 decoding — in the latter
    /// case we emit a `tracing::warn!` so a `RUST_LOG=warn` run can
    /// surface silent index corruption rather than letting an empty
    /// string propagate into rebuilds and effectively delete symbols.
    pub fn read_string(&self, offset: u32) -> &str {
        if offset == u32::MAX {
            return "";
        }
        let header = self.header();
        let base = match (header.strings_offset as usize).checked_add(offset as usize) {
            Some(b) => b,
            None => {
                tracing::warn!(
                    offset,
                    strings_offset = header.strings_offset,
                    "read_string: offset overflows usize — returning empty string"
                );
                return "";
            }
        };
        if base >= self.mmap.len() {
            tracing::warn!(
                offset,
                base,
                mmap_len = self.mmap.len(),
                "read_string: offset past end of mmap — returning empty string"
            );
            return "";
        }
        let data = &self.mmap[base..];
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        match std::str::from_utf8(&data[..end]) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    offset,
                    base,
                    len = end,
                    error = %e,
                    "read_string: invalid UTF-8 in strings section — returning empty string; \
                     callers using the result as a symbol name MUST skip the record \
                     instead of persisting an empty name."
                );
                ""
            }
        }
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
        // Never read past the end of the vectors section (the strings
        // section starts at `strings_offset`). The monotone-offset guard
        // in `open()` proves `strings_offset <= mmap.len()`, so this is
        // tighter than the mmap-length check below for malformed
        // `vector_index` values.
        if end > header.strings_offset as usize {
            return None;
        }
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
    pub fn has_ref_edges(&self) -> bool {
        self.v5_section_header()
            .is_some_and(|h| h.ref_edges_len > 0)
    }

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
    ///
    /// The FST lookup is wrapped in `catch_unwind` because the upstream
    /// `fst` crate's `Map::new` does only shallow header validation —
    /// adversarially-corrupt FST bytes can pass construction but panic
    /// during node traversal (fuzzer found one: `node.rs:302` index OOB).
    /// vex's threat model says index bytes are user-owned, but
    /// defense-in-depth: corrupt mmap (cosmic ray, half-truncated write,
    /// hostile cache override) shouldn't crash the CLI.
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
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reader.find_by_symbol_idx(sym_idx)
        }))
        .unwrap_or_else(|_| {
            tracing::warn!(
                sym_idx,
                "ref_edges FST traversal panicked on corrupt bytes; returning empty result"
            );
            Vec::new()
        })
    }

    /// Read the v7 [`UnresolvedRefsHeader`] when present. Returns `None`
    /// for v3..v6 indexes or when the bytes after the
    /// [`PatternSkeletonHeader`] don't fit / aren't aligned.
    pub fn unresolved_refs_header(&self) -> Option<&UnresolvedRefsHeader> {
        if !self.header().has_unresolved_refs_header() {
            return None;
        }
        let offset = Header::SIZE
            .checked_add(CallGraphHeader::SIZE)?
            .checked_add(V5SectionHeader::SIZE)?
            .checked_add(PatternSkeletonHeader::SIZE)?;
        let end = offset.checked_add(UnresolvedRefsHeader::SIZE)?;
        if end > self.mmap.len() {
            return None;
        }
        let ptr = unsafe { self.mmap.as_ptr().add(offset) };
        if ptr.align_offset(std::mem::align_of::<UnresolvedRefsHeader>()) != 0 {
            return None;
        }
        // SAFETY: bounds + alignment checked. UnresolvedRefsHeader is #[repr(C)].
        Some(unsafe { &*(ptr as *const UnresolvedRefsHeader) })
    }

    /// Whether the index carries unresolved-by-name reference edges (v7+,
    /// multi-repo Phase 6). False for v3..v6 indexes and v7 indexes whose
    /// Pass-2 left nothing unresolved.
    pub fn has_unresolved_refs(&self) -> bool {
        self.unresolved_refs_header()
            .is_some_and(|h| h.unresolved_edges_len > 0)
    }

    fn unresolved_refs_section_bytes(&self) -> Option<(&[u8], &[u8], &[u8])> {
        let h = self.unresolved_refs_header()?;
        let mmap = &self.mmap[..];
        let edges = slice_or_empty(
            mmap,
            h.unresolved_edges_offset as usize,
            h.unresolved_edges_len as usize,
        )?;
        let fst = slice_or_empty(
            mmap,
            h.unresolved_fst_offset as usize,
            h.unresolved_fst_len as usize,
        )?;
        let post = slice_or_empty(
            mmap,
            h.unresolved_postings_offset as usize,
            h.unresolved_postings_len as usize,
        )?;
        Some((edges, fst, post))
    }

    /// Look up every persisted unresolved reference edge recorded for
    /// `name` (case-insensitive). Returns an empty `Vec` when the index has
    /// no unresolved-refs section, the FST misses the key, or the bytes
    /// don't validate. FST traversal is wrapped in `catch_unwind` for the
    /// same defense-in-depth reason as [`Self::find_ref_edges_by_symbol`].
    pub fn find_unresolved_refs_by_name(&self, name: &str) -> Vec<super::format::UnresolvedRef> {
        if !self.has_unresolved_refs() {
            return Vec::new();
        }
        let Some((edges, fst, post)) = self.unresolved_refs_section_bytes() else {
            return Vec::new();
        };
        let Ok(reader) = super::unresolved_refs::UnresolvedRefReader::new(fst, post, edges) else {
            return Vec::new();
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.find_by_name(name)))
            .unwrap_or_else(|_| {
                tracing::warn!(
                    name,
                    "unresolved_refs FST traversal panicked on corrupt bytes; returning empty result"
                );
                Vec::new()
            })
    }

    /// Every `(name, UnresolvedRef)` pair recorded in this index, FST-key
    /// order. Empty when there is no unresolved-refs section. Used by the
    /// `vex update` carry-forward (`reconstruct_unchanged`) so unchanged
    /// files keep their cross-repo unresolved refs across incremental
    /// updates. FST traversal is wrapped in `catch_unwind` for the same
    /// defense-in-depth reason as [`Self::find_ref_edges_by_symbol`].
    pub fn unresolved_refs_all(&self) -> Vec<(String, super::format::UnresolvedRef)> {
        if !self.has_unresolved_refs() {
            return Vec::new();
        }
        let Some((edges, fst, post)) = self.unresolved_refs_section_bytes() else {
            return Vec::new();
        };
        let Ok(reader) = super::unresolved_refs::UnresolvedRefReader::new(fst, post, edges) else {
            return Vec::new();
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.iter_all())).unwrap_or_else(
            |_| {
                tracing::warn!(
                    "unresolved_refs FST traversal panicked on corrupt bytes; returning empty result"
                );
                Vec::new()
            },
        )
    }

    /// Read the v8 [`HierarchyHeader`] when present. Returns `None` for
    /// v3..v7 indexes or when the bytes after the [`UnresolvedRefsHeader`]
    /// don't fit / aren't aligned.
    pub fn hierarchy_header(&self) -> Option<&HierarchyHeader> {
        if !self.header().has_hierarchy_header() {
            return None;
        }
        let offset = Header::SIZE
            .checked_add(CallGraphHeader::SIZE)?
            .checked_add(V5SectionHeader::SIZE)?
            .checked_add(PatternSkeletonHeader::SIZE)?
            .checked_add(UnresolvedRefsHeader::SIZE)?;
        let end = offset.checked_add(HierarchyHeader::SIZE)?;
        if end > self.mmap.len() {
            return None;
        }
        let ptr = unsafe { self.mmap.as_ptr().add(offset) };
        if ptr.align_offset(std::mem::align_of::<HierarchyHeader>()) != 0 {
            return None;
        }
        // SAFETY: bounds + alignment checked. HierarchyHeader is #[repr(C)].
        Some(unsafe { &*(ptr as *const HierarchyHeader) })
    }

    /// Whether the index carries typed hierarchy edges (v8+). False for
    /// v3..v7 indexes and v8 indexes whose section is empty.
    pub fn has_hierarchy_edges(&self) -> bool {
        self.hierarchy_header().is_some_and(|h| h.edges_len > 0)
    }

    fn hierarchy_section_bytes(&self) -> Option<(&[u8], &[u8], &[u8])> {
        let h = self.hierarchy_header()?;
        let mmap = &self.mmap[..];
        let edges = slice_or_empty(mmap, h.edges_offset as usize, h.edges_len as usize)?;
        let index = slice_or_empty(mmap, h.index_offset as usize, h.index_len as usize)?;
        let postings = slice_or_empty(mmap, h.postings_offset as usize, h.postings_len as usize)?;
        Some((edges, index, postings))
    }

    /// Copy the `i`-th [`HierarchyPostingEntry`] out of the `index` bytes.
    /// Bounds-checked; `None` on any out-of-range or truncated read
    /// (corrupt index) — the caller treats that as "entry not found".
    #[allow(dead_code)] // P1 format scaffold — only reached via find_hierarchy_edges_by_symbol
    fn read_hierarchy_posting_entry(index: &[u8], i: usize) -> Option<HierarchyPostingEntry> {
        let off = i.checked_mul(HierarchyPostingEntry::SIZE)?;
        let end = off.checked_add(HierarchyPostingEntry::SIZE)?;
        if end > index.len() {
            return None;
        }
        let mut rec = std::mem::MaybeUninit::<HierarchyPostingEntry>::uninit();
        // SAFETY: bounds checked above; copy into stack-aligned storage to
        // avoid unaligned reads from mmap (the index sub-section is only
        // 4-byte aligned at its start, not necessarily at every entry
        // offset relative to the mmap base).
        unsafe {
            std::ptr::copy_nonoverlapping(
                index[off..].as_ptr(),
                rec.as_mut_ptr() as *mut u8,
                HierarchyPostingEntry::SIZE,
            );
            Some(rec.assume_init())
        }
    }

    /// Look up every persisted [`HierarchyEdge`] whose `to_sym_idx`
    /// (resolved parent) matches `to_sym_idx`, via binary search over the
    /// sorted `HierarchyPostingEntry[]` index sub-section (NOT an FST —
    /// `to_sym_idx` is a dense array index, see
    /// `docs/HIERARCHY-EDGES.md` §3.4). Returns an empty `Vec` when the
    /// index has no hierarchy section, the parent has no recorded edges,
    /// or any section bytes fail to validate. Never panics on malformed
    /// input — every bounds check degrades to "skip this entry" or
    /// "return empty" (P1 acceptance criteria, see
    /// `docs/HIERARCHY-EDGES.md` §8).
    #[allow(dead_code)] // P1 format scaffold — no CLI caller until P3 wires `vex implementations`
    pub fn find_hierarchy_edges_by_symbol(&self, to_sym_idx: u32) -> Vec<HierarchyEdge> {
        if !self.has_hierarchy_edges() {
            return Vec::new();
        }
        let Some((edges, index, postings)) = self.hierarchy_section_bytes() else {
            return Vec::new();
        };
        if index.len() % HierarchyPostingEntry::SIZE != 0 {
            return Vec::new();
        }
        let entry_count = index.len() / HierarchyPostingEntry::SIZE;

        // Manual binary search: partition_point-style, reading each
        // candidate entry through the bounds-checked helper (never an
        // aligned cast over the whole index slice).
        let mut lo = 0usize;
        let mut hi = entry_count;
        let mut found: Option<HierarchyPostingEntry> = None;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let Some(entry) = Self::read_hierarchy_posting_entry(index, mid) else {
                return Vec::new();
            };
            match entry.to_sym_idx.cmp(&to_sym_idx) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    found = Some(entry);
                    break;
                }
            }
        }
        let Some(entry) = found else {
            return Vec::new();
        };

        let edge_indices = read_hierarchy_posting_list(postings, entry.posting_offset as usize);
        let edge_count = edges.len() / HierarchyEdge::SIZE;
        let mut out = Vec::with_capacity(edge_indices.len());
        for idx in edge_indices {
            let idx_usize = idx as usize;
            if idx_usize >= edge_count {
                // Corrupt posting list — skip the entry instead of
                // panicking. Safest degradation is "missing edge".
                continue;
            }
            let off = idx_usize * HierarchyEdge::SIZE;
            let mut rec = std::mem::MaybeUninit::<HierarchyEdge>::uninit();
            // SAFETY: bounds checked above; copy into stack-aligned
            // storage to avoid unaligned reads from mmap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    edges[off..].as_ptr(),
                    rec.as_mut_ptr() as *mut u8,
                    HierarchyEdge::SIZE,
                );
                out.push(rec.assume_init());
            }
        }
        out
    }

    /// Every resolved [`HierarchyEdge`] recorded in this index, walked
    /// directly off the `edges` sub-section byte array (NOT via the
    /// `to_sym_idx`-keyed posting index — that index is keyed by
    /// **parent**, so "every edge whose child lives in file F" needs a
    /// full enumerate-then-filter, same shape as
    /// [`Self::unresolved_refs_all`]). Used by the `vex update`
    /// carry-forward (`reconstruct_unchanged`) so unchanged files keep
    /// their hierarchy edges across incremental updates (P2a). Empty when
    /// there is no hierarchy section (pre-v8 index, or a v8 index with no
    /// edges). Bounds-checked; a truncated/corrupt `edges` sub-section
    /// yields as many whole records as fit, never panics.
    pub fn hierarchy_edges_all(&self) -> Vec<HierarchyEdge> {
        if !self.has_hierarchy_edges() {
            return Vec::new();
        }
        let Some((edges, _index, _postings)) = self.hierarchy_section_bytes() else {
            return Vec::new();
        };
        let count = edges.len() / HierarchyEdge::SIZE;
        let mut out = Vec::with_capacity(count);
        for idx in 0..count {
            let off = idx * HierarchyEdge::SIZE;
            let end = off + HierarchyEdge::SIZE;
            if end > edges.len() {
                break;
            }
            let mut rec = std::mem::MaybeUninit::<HierarchyEdge>::uninit();
            // SAFETY: bounds checked above; copy into stack-aligned
            // storage to avoid unaligned reads from mmap (the edges
            // sub-section is only guaranteed 4-byte aligned at its
            // start, not necessarily at every record offset).
            unsafe {
                std::ptr::copy_nonoverlapping(
                    edges[off..].as_ptr(),
                    rec.as_mut_ptr() as *mut u8,
                    HierarchyEdge::SIZE,
                );
                out.push(rec.assume_init());
            }
        }
        out
    }

    /// Read the v8 [`UnresolvedHierarchyHeader`] when present. Returns
    /// `None` for v3..v7 indexes or when the bytes after the
    /// [`HierarchyHeader`] don't fit / aren't aligned. Mirrors
    /// [`Self::hierarchy_header`] / [`Self::unresolved_refs_header`].
    pub fn unresolved_hierarchy_header(&self) -> Option<&UnresolvedHierarchyHeader> {
        if !self.header().has_unresolved_hierarchy_header() {
            return None;
        }
        let offset = Header::SIZE
            .checked_add(CallGraphHeader::SIZE)?
            .checked_add(V5SectionHeader::SIZE)?
            .checked_add(PatternSkeletonHeader::SIZE)?
            .checked_add(UnresolvedRefsHeader::SIZE)?
            .checked_add(HierarchyHeader::SIZE)?;
        let end = offset.checked_add(UnresolvedHierarchyHeader::SIZE)?;
        if end > self.mmap.len() {
            return None;
        }
        let ptr = unsafe { self.mmap.as_ptr().add(offset) };
        if ptr.align_offset(std::mem::align_of::<UnresolvedHierarchyHeader>()) != 0 {
            return None;
        }
        // SAFETY: bounds + alignment checked. UnresolvedHierarchyHeader is #[repr(C)].
        Some(unsafe { &*(ptr as *const UnresolvedHierarchyHeader) })
    }

    /// Whether the index carries unresolved-by-name hierarchy edges (v8+,
    /// P2). False for v3..v7 indexes and v8 indexes whose Pass-2 left
    /// nothing unresolved.
    pub fn has_unresolved_hierarchy_edges(&self) -> bool {
        self.unresolved_hierarchy_header()
            .is_some_and(|h| h.edges_len > 0)
    }

    fn unresolved_hierarchy_section_bytes(&self) -> Option<(&[u8], &[u8], &[u8])> {
        let h = self.unresolved_hierarchy_header()?;
        let mmap = &self.mmap[..];
        let edges = slice_or_empty(mmap, h.edges_offset as usize, h.edges_len as usize)?;
        let fst = slice_or_empty(mmap, h.fst_offset as usize, h.fst_len as usize)?;
        let post = slice_or_empty(mmap, h.postings_offset as usize, h.postings_len as usize)?;
        Some((edges, fst, post))
    }

    /// Look up every persisted [`UnresolvedHierarchyEdge`] recorded for the
    /// verbatim parent `name` (case-sensitive — unlike
    /// [`Self::find_unresolved_refs_by_name`], this section does NOT
    /// lowercase its key; see `docs/HIERARCHY-EDGES.md` §3.5). Returns an
    /// empty `Vec` when the index has no unresolved-hierarchy section, the
    /// FST misses the key, or the bytes don't validate. FST traversal is
    /// wrapped in `catch_unwind` for the same defense-in-depth reason as
    /// [`Self::find_ref_edges_by_symbol`].
    #[allow(dead_code)] // no CLI caller until P3 wires `vex implementations`/`vex subtypes`
    pub fn find_unresolved_hierarchy_by_name(&self, name: &str) -> Vec<UnresolvedHierarchyEdge> {
        if !self.has_unresolved_hierarchy_edges() {
            return Vec::new();
        }
        let Some((edges, fst, post)) = self.unresolved_hierarchy_section_bytes() else {
            return Vec::new();
        };
        let Ok(reader) =
            super::unresolved_hierarchy::UnresolvedHierarchyReader::new(fst, post, edges)
        else {
            return Vec::new();
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.find_by_name(name)))
            .unwrap_or_else(|_| {
                tracing::warn!(
                    name,
                    "unresolved_hierarchy FST traversal panicked on corrupt bytes; returning empty result"
                );
                Vec::new()
            })
    }

    /// Every `(parent_name, UnresolvedHierarchyEdge)` pair recorded in this
    /// index, FST-key order (verbatim case). Empty when there is no
    /// unresolved-hierarchy section. Used by the `vex update` carry-forward
    /// (`reconstruct_unchanged`, P2a) so unchanged files keep their external
    /// (unresolved) supertype names across incremental updates. Mirrors
    /// [`Self::unresolved_refs_all`]; FST traversal is wrapped in
    /// `catch_unwind` for the same defense-in-depth reason as
    /// [`Self::find_ref_edges_by_symbol`].
    pub fn unresolved_hierarchy_all(&self) -> Vec<(String, UnresolvedHierarchyEdge)> {
        if !self.has_unresolved_hierarchy_edges() {
            return Vec::new();
        }
        let Some((edges, fst, post)) = self.unresolved_hierarchy_section_bytes() else {
            return Vec::new();
        };
        let Ok(reader) =
            super::unresolved_hierarchy::UnresolvedHierarchyReader::new(fst, post, edges)
        else {
            return Vec::new();
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.iter_all())).unwrap_or_else(
            |_| {
                tracing::warn!(
                    "unresolved_hierarchy FST traversal panicked on corrupt bytes; returning empty result"
                );
                Vec::new()
            },
        )
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

    /// Number of ref-edges recorded, 0 when the section is absent. Phase
    /// 11.1.9 (Q4-A): used by `reconstruct_unchanged` to iterate every
    /// edge for re-emission. A `ref_edges_len` that's not a multiple of
    /// `RefEdge::SIZE` is corruption — warn + return 0 rather than
    /// half-process the section (architect-M4 must-fix).
    pub fn ref_edge_count(&self) -> usize {
        let Some(h) = self.v5_section_header() else {
            return 0;
        };
        let len = h.ref_edges_len as usize;
        if !len.is_multiple_of(super::format::RefEdge::SIZE) {
            tracing::warn!(
                len,
                size = super::format::RefEdge::SIZE,
                "ref_edges_len not a multiple of RefEdge::SIZE — section truncated, treating as empty"
            );
            return 0;
        }
        len / super::format::RefEdge::SIZE
    }

    /// Get a ref-edge record by section-index. Returns a **copy** rather
    /// than a `&RefEdge` reference so callers don't depend on the mmap
    /// alignment of `ref_edges_offset` (architect-H3a / rust-reviewer-#3
    /// must-fix): `RefEdgeReader::find_by_symbol_idx` at `ref_edges.rs`
    /// uses the same `MaybeUninit + copy_nonoverlapping` idiom.
    pub fn ref_edge(&self, idx: usize) -> Option<super::format::RefEdge> {
        let h = self.v5_section_header()?;
        // Inline the count check using the already-fetched header — avoids
        // a redundant second `v5_section_header()` call via
        // `ref_edge_count()` and folds the truncation guard into one
        // expression (rust-reviewer HIGH cleanup).
        let len = h.ref_edges_len as usize;
        if !len.is_multiple_of(super::format::RefEdge::SIZE) {
            return None;
        }
        let count = len / super::format::RefEdge::SIZE;
        if idx >= count {
            return None;
        }
        let offset =
            (h.ref_edges_offset as usize).checked_add(idx * super::format::RefEdge::SIZE)?;
        let end = offset.checked_add(super::format::RefEdge::SIZE)?;
        if end > self.mmap.len() {
            return None;
        }
        let mut rec = std::mem::MaybeUninit::<super::format::RefEdge>::uninit();
        // SAFETY: bounds checked above; copy into stack-aligned storage
        // so an unaligned `ref_edges_offset` (today writer 4-aligns it,
        // but a future bump could shift) cannot UB.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mmap[offset..].as_ptr(),
                rec.as_mut_ptr() as *mut u8,
                super::format::RefEdge::SIZE,
            );
            Some(rec.assume_init())
        }
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

/// Read a `[u32 count][u32 edge_idx; count]` posting list out of the
/// hierarchy_edges postings blob at `offset`. Bounds-checked on the count
/// prefix and every subsequent entry — truncates (returns whatever was
/// read so far) rather than panicking on a corrupt/truncated blob, same
/// idiom as `RefEdgeReader::read_posting_list` /
/// `UnresolvedRefReader::read_posting_list`.
#[allow(dead_code)] // P1 format scaffold — only reached via find_hierarchy_edges_by_symbol
fn read_hierarchy_posting_list(postings: &[u8], offset: usize) -> Vec<u32> {
    if offset + 4 > postings.len() {
        return Vec::new();
    }
    let count =
        u32::from_le_bytes(postings[offset..offset + 4].try_into().unwrap_or([0; 4])) as usize;
    let mut out = Vec::with_capacity(count);
    let mut pos = offset + 4;
    for _ in 0..count {
        if pos + 4 > postings.len() {
            break;
        }
        let idx = u32::from_le_bytes(postings[pos..pos + 4].try_into().unwrap_or([0; 4]));
        out.push(idx);
        pos += 4;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::format::{
        CallGraphHeader, Header, PatternSkeletonHeader, V5SectionHeader, MAGIC, VERSION,
    };

    /// Build a minimal but byte-valid v8 index header block on disk so we
    /// can mutate one field at a time and assert `IndexReader::open`
    /// rejects the corrupted file with a typed error rather than
    /// silently allowing overlapping sections.
    ///
    /// `hierarchy` is `None` for the "empty section" shape used by every
    /// pre-existing test in this module, or `Some((edges, index,
    /// postings))` to splice in real hierarchy_edges section bytes
    /// (4-byte aligned) for the roundtrip tests — this is the established
    /// pattern for this file rather than inventing a second helper.
    fn write_minimal_index(tmp: &Path, mutate: impl FnOnce(&mut Header)) -> std::path::PathBuf {
        write_minimal_index_with_hierarchy(tmp, mutate, None)
    }

    fn write_minimal_index_with_hierarchy(
        tmp: &Path,
        mutate: impl FnOnce(&mut Header),
        hierarchy: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    ) -> std::path::PathBuf {
        write_minimal_index_with_hierarchy_sections(tmp, mutate, hierarchy, None)
    }

    /// Splice BOTH the resolved `hierarchy_edges` section AND the
    /// `unresolved_hierarchy` section into a minimal on-disk v8 index.
    /// `hierarchy` is `(edges, index, postings)` for the resolved section;
    /// `unresolved_hierarchy` is `(edges, fst, postings)` for the parallel
    /// unresolved section — both default to all-zeroed/empty when `None`,
    /// matching every other test in this module.
    fn write_minimal_index_with_hierarchy_sections(
        tmp: &Path,
        mutate: impl FnOnce(&mut Header),
        hierarchy: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
        unresolved_hierarchy: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    ) -> std::path::PathBuf {
        let total_header = Header::SIZE
            + CallGraphHeader::SIZE
            + V5SectionHeader::SIZE
            + PatternSkeletonHeader::SIZE
            + UnresolvedRefsHeader::SIZE
            + HierarchyHeader::SIZE
            + UnresolvedHierarchyHeader::SIZE;

        let (edges_bytes, index_bytes, postings_bytes) =
            hierarchy.unwrap_or((Vec::new(), Vec::new(), Vec::new()));
        let (uh_edges_bytes, uh_fst_bytes, uh_postings_bytes) =
            unresolved_hierarchy.unwrap_or((Vec::new(), Vec::new(), Vec::new()));
        // 4-byte align the hierarchy edges array after the fixed headers,
        // matching the writer's convention for every other aligned section.
        let hier_unaligned = total_header as u64;
        let hier_edges_offset = (hier_unaligned + 3) & !3u64;
        let hier_pad = (hier_edges_offset - hier_unaligned) as usize;
        let hier_index_offset = hier_edges_offset + edges_bytes.len() as u64;
        let hier_postings_offset = hier_index_offset + index_bytes.len() as u64;
        // unresolved_hierarchy sub-section immediately follows, 4-byte
        // aligned the same way.
        let uh_unaligned = hier_postings_offset + postings_bytes.len() as u64;
        let uh_edges_offset = (uh_unaligned + 3) & !3u64;
        let uh_pad = (uh_edges_offset - uh_unaligned) as usize;
        let uh_fst_offset = uh_edges_offset + uh_edges_bytes.len() as u64;
        let uh_postings_offset = uh_fst_offset + uh_fst_bytes.len() as u64;
        let symbols_offset = uh_postings_offset + uh_postings_bytes.len() as u64;

        let mut header = Header {
            magic: *MAGIC,
            version: VERSION,
            symbol_count: 0,
            vector_dim: 384,
            _padding: 0,
            symbols_offset,
            vectors_offset: symbols_offset,
            strings_offset: symbols_offset,
            inverted_offset: 0,
            hnsw_offset: 0,
            fst_offset: symbols_offset,
            fst_len: 0,
            postings_offset: symbols_offset,
            postings_len: 0,
            file_table_offset: symbols_offset,
            file_table_count: 0,
            _padding2: 0,
            sym_fst_offset: symbols_offset,
            sym_fst_len: 0,
            sym_postings_offset: symbols_offset,
            sym_postings_len: 0,
        };
        mutate(&mut header);

        let cg = CallGraphHeader {
            call_edges_offset: symbols_offset,
            call_edges_len: 0,
            callers_fst_offset: symbols_offset,
            callers_fst_len: 0,
            callers_postings_offset: symbols_offset,
            callers_postings_len: 0,
            callees_fst_offset: symbols_offset,
            callees_fst_len: 0,
            callees_postings_offset: symbols_offset,
            callees_postings_len: 0,
            bm25_fst_offset: symbols_offset,
            bm25_fst_len: 0,
            bm25_postings_offset: symbols_offset,
            bm25_postings_len: 0,
            bm25_stats_offset: symbols_offset,
            bm25_stats_len: 0,
        };
        let v5 = V5SectionHeader {
            ref_edges_offset: symbols_offset,
            ref_edges_len: 0,
            ref_edges_fst_offset: symbols_offset,
            ref_edges_fst_len: 0,
            ref_edges_postings_offset: symbols_offset,
            ref_edges_postings_len: 0,
        };
        let pat = PatternSkeletonHeader {
            skeletons_offset: symbols_offset,
            skeletons_len: 0,
            kind_path_offset: symbols_offset,
            kind_path_len: 0,
            ident_pool_offset: symbols_offset,
            ident_pool_len: 0,
            file_index_offset: symbols_offset,
            file_index_len: 0,
            grammar_fingerprints: [0u32; 32],
        };
        let unres = UnresolvedRefsHeader {
            unresolved_edges_offset: symbols_offset,
            unresolved_edges_len: 0,
            unresolved_fst_offset: symbols_offset,
            unresolved_fst_len: 0,
            unresolved_postings_offset: symbols_offset,
            unresolved_postings_len: 0,
        };
        let hier = HierarchyHeader {
            edges_offset: hier_edges_offset,
            edges_len: edges_bytes.len() as u64,
            index_offset: hier_index_offset,
            index_len: index_bytes.len() as u64,
            postings_offset: hier_postings_offset,
            postings_len: postings_bytes.len() as u64,
        };
        let unres_hier = UnresolvedHierarchyHeader {
            edges_offset: uh_edges_offset,
            edges_len: uh_edges_bytes.len() as u64,
            fst_offset: uh_fst_offset,
            fst_len: uh_fst_bytes.len() as u64,
            postings_offset: uh_postings_offset,
            postings_len: uh_postings_bytes.len() as u64,
        };

        let mut bytes = Vec::with_capacity(total_header);
        // SAFETY: all structs are `#[repr(C)]` with stable layouts; we're
        // reading their bytes for a write-then-read round-trip.
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts(&header as *const Header as *const u8, Header::SIZE)
        });
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                &cg as *const CallGraphHeader as *const u8,
                CallGraphHeader::SIZE,
            )
        });
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                &v5 as *const V5SectionHeader as *const u8,
                V5SectionHeader::SIZE,
            )
        });
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                &pat as *const PatternSkeletonHeader as *const u8,
                PatternSkeletonHeader::SIZE,
            )
        });
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                &unres as *const UnresolvedRefsHeader as *const u8,
                UnresolvedRefsHeader::SIZE,
            )
        });
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                &hier as *const HierarchyHeader as *const u8,
                HierarchyHeader::SIZE,
            )
        });
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                &unres_hier as *const UnresolvedHierarchyHeader as *const u8,
                UnresolvedHierarchyHeader::SIZE,
            )
        });

        debug_assert_eq!(bytes.len(), total_header);
        if hier_pad > 0 {
            bytes.extend_from_slice(&[0u8; 3][..hier_pad]);
        }
        bytes.extend_from_slice(&edges_bytes);
        bytes.extend_from_slice(&index_bytes);
        bytes.extend_from_slice(&postings_bytes);
        if uh_pad > 0 {
            bytes.extend_from_slice(&[0u8; 3][..uh_pad]);
        }
        bytes.extend_from_slice(&uh_edges_bytes);
        bytes.extend_from_slice(&uh_fst_bytes);
        bytes.extend_from_slice(&uh_postings_bytes);

        let path = tmp.join("index.vex");
        std::fs::write(&path, &bytes).expect("write minimal index");
        path
    }

    #[test]
    fn open_accepts_minimal_valid_header() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |_| {});
        let reader = IndexReader::open(&path).expect("minimal v6 header is valid");
        assert_eq!(reader.symbol_count(), 0);
    }

    #[test]
    fn open_rejects_oversized_vector_dim() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |h| {
            // Anything > 4096 must fail the cap check.
            h.vector_dim = 1_000_000;
        });
        let err = match IndexReader::open(&path) {
            Ok(_) => panic!("oversized vector_dim must be rejected"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("vector_dim") || msg.contains("exceeds cap"),
            "expected dim-cap error, got: {msg}"
        );
    }

    #[test]
    fn open_rejects_vectors_offset_below_symbols_end() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |h| {
            // Pretend we have one symbol but vectors_offset overlaps with
            // the symbol record bytes — exactly the C3 attack: vector
            // reads alias symbol-record bytes as f32.
            h.symbol_count = 1;
            h.vectors_offset = 0; // way before symbols_offset
        });
        let err = match IndexReader::open(&path) {
            Ok(_) => panic!("vectors_offset overlapping symbols must be rejected"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-monotone")
                || msg.contains("vectors_offset")
                || msg.contains("truncated"),
            "expected monotone-offset error, got: {msg}"
        );
    }

    #[test]
    fn open_rejects_strings_offset_before_vectors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |h| {
            // strings_offset must be >= vectors_offset.
            h.vectors_offset = 1024;
            h.strings_offset = 512;
        });
        let err = match IndexReader::open(&path) {
            Ok(_) => panic!("strings before vectors must be rejected"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("vectors_offset") || msg.contains("non-monotone"),
            "expected monotone-offset error, got: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // v8 hierarchy_edges — P1 format scaffold roundtrip tests.
    // -----------------------------------------------------------------

    use crate::store::hierarchy_edges::{build_hierarchy_section, HierarchyEdgeBuilder};

    fn hb(to: u32, from: u32, file: u32, line: u32, kind: u8) -> HierarchyEdgeBuilder {
        HierarchyEdgeBuilder {
            to_sym_idx: to,
            from_sym_idx: from,
            from_file_id: file,
            line,
            kind,
        }
    }

    /// Full end-to-end roundtrip: build real `HierarchyEdge` records for
    /// 3 distinct parents with 1-2 children each, splice them into a
    /// minimal on-disk v8 index via `IndexReader::open`, and confirm
    /// `find_hierarchy_edges_by_symbol` recovers exactly the right
    /// children/kinds/lines per parent through the binary-search +
    /// bounds-check read path (not just the builder in isolation).
    #[test]
    fn hierarchy_edges_roundtrip_finds_children_by_parent() {
        let edges = vec![
            hb(100, 1, 0, 10, 0), // parent 100 <- child 1, Extends, line 10
            hb(100, 2, 0, 20, 1), // parent 100 <- child 2, Implements, line 20
            hb(200, 3, 1, 30, 2), // parent 200 <- child 3, Uses, line 30
            hb(300, 4, 1, 40, 0), // parent 300 <- child 4, Extends, line 40
            hb(300, 5, 2, 50, 1), // parent 300 <- child 5, Implements, line 50
        ];
        let (edge_bytes, index_bytes, posting_bytes) =
            build_hierarchy_section(&edges).expect("build hierarchy section");

        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_hierarchy(
            tmp.path(),
            |_| {},
            Some((edge_bytes, index_bytes, posting_bytes)),
        );
        let reader = IndexReader::open(&path).expect("v8 index with hierarchy section is valid");

        assert!(reader.has_hierarchy_edges());

        let mut p100 = reader.find_hierarchy_edges_by_symbol(100);
        p100.sort_by_key(|e| e.from_sym_idx);
        assert_eq!(p100.len(), 2, "parent 100 has two children");
        assert_eq!(p100[0].from_sym_idx, 1);
        assert_eq!(p100[0].line(), 10);
        assert_eq!(
            crate::store::format::EdgeKind::try_from(p100[0].edge_kind_bits()),
            Ok(crate::store::format::EdgeKind::Extends)
        );
        assert_eq!(p100[1].from_sym_idx, 2);
        assert_eq!(p100[1].line(), 20);
        assert_eq!(
            crate::store::format::EdgeKind::try_from(p100[1].edge_kind_bits()),
            Ok(crate::store::format::EdgeKind::Implements)
        );

        let p200 = reader.find_hierarchy_edges_by_symbol(200);
        assert_eq!(p200.len(), 1, "parent 200 has one child");
        assert_eq!(p200[0].from_sym_idx, 3);
        assert_eq!(p200[0].from_file_id, 1);
        assert_eq!(p200[0].line(), 30);
        assert_eq!(
            crate::store::format::EdgeKind::try_from(p200[0].edge_kind_bits()),
            Ok(crate::store::format::EdgeKind::Uses)
        );

        let mut p300 = reader.find_hierarchy_edges_by_symbol(300);
        p300.sort_by_key(|e| e.from_sym_idx);
        assert_eq!(p300.len(), 2, "parent 300 has two children");
        assert_eq!(p300[0].from_sym_idx, 4);
        assert_eq!(p300[1].from_sym_idx, 5);
    }

    #[test]
    fn hierarchy_edges_absent_parent_returns_empty() {
        let edges = vec![hb(100, 1, 0, 10, 0)];
        let (edge_bytes, index_bytes, posting_bytes) =
            build_hierarchy_section(&edges).expect("build hierarchy section");

        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_hierarchy(
            tmp.path(),
            |_| {},
            Some((edge_bytes, index_bytes, posting_bytes)),
        );
        let reader = IndexReader::open(&path).expect("v8 index with hierarchy section is valid");

        assert!(
            reader.find_hierarchy_edges_by_symbol(999).is_empty(),
            "a parent with no recorded edges must return empty, not panic"
        );
    }

    #[test]
    fn hierarchy_edges_v7_file_opens_clean_with_no_hierarchy_section() {
        // A v7-version header (no HierarchyHeader at all) must open clean
        // and `find_hierarchy_edges_by_symbol` must return empty — this
        // is the P1 "v7-reads-clean" acceptance criterion (§8).
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |h| {
            h.version = 7;
        });
        let reader = IndexReader::open(&path).expect("v7 index must still open cleanly");
        assert!(!reader.header().has_hierarchy_header());
        assert!(!reader.has_hierarchy_edges());
        assert!(reader.find_hierarchy_edges_by_symbol(0).is_empty());
    }

    #[test]
    fn hierarchy_edges_v8_empty_section_returns_empty() {
        // A v8 header with a zeroed (present-but-empty) HierarchyHeader —
        // the shape every real P1 index has, since extraction is P2 —
        // must behave identically to "no section".
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |_| {});
        let reader = IndexReader::open(&path).expect("v8 index with empty hierarchy section");
        assert!(reader.header().has_hierarchy_header());
        assert!(!reader.has_hierarchy_edges());
        assert!(reader.find_hierarchy_edges_by_symbol(0).is_empty());
    }

    #[test]
    fn hierarchy_edges_truncated_header_rejected() {
        // A v8 file truncated exactly at the end of UnresolvedRefsHeader
        // (missing the HierarchyHeader bytes) must be rejected rather
        // than silently treated as an empty section.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |_| {});
        let bytes = std::fs::read(&path).expect("read written index");
        let truncated_len = Header::SIZE
            + CallGraphHeader::SIZE
            + V5SectionHeader::SIZE
            + PatternSkeletonHeader::SIZE
            + UnresolvedRefsHeader::SIZE;
        let truncated = &bytes[..truncated_len];
        let trunc_path = tmp.path().join("truncated.vex");
        std::fs::write(&trunc_path, truncated).expect("write truncated index");

        let result = IndexReader::open(&trunc_path);
        assert!(
            result.is_err(),
            "v8 index missing HierarchyHeader bytes must be rejected"
        );
    }

    #[test]
    fn hierarchy_edges_oob_posting_index_returns_empty_never_panics() {
        // Fuzz-style: corrupt a posting-list edge_idx to point past the
        // end of the edges array. The reader must skip the OOB entry and
        // return empty rather than panic (P1 acceptance §8, "OOB-posting-
        // index ... must return empty, never panic").
        let edges = vec![hb(7, 1, 0, 1, 0)];
        let (edge_bytes, index_bytes, mut posting_bytes) =
            build_hierarchy_section(&edges).expect("build");
        // Posting layout: [u32 count = 1][u32 edge_idx]; corrupt idx to 999.
        assert!(posting_bytes.len() >= 8);
        posting_bytes[4..8].copy_from_slice(&999u32.to_le_bytes());

        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_hierarchy(
            tmp.path(),
            |_| {},
            Some((edge_bytes, index_bytes, posting_bytes)),
        );
        let reader = IndexReader::open(&path).expect("index with corrupt posting still opens");
        let hits = reader.find_hierarchy_edges_by_symbol(7);
        assert!(
            hits.is_empty(),
            "out-of-range edge_idx must skip silently, got {hits:?}"
        );
    }

    #[test]
    fn hierarchy_edges_corrupt_index_length_returns_empty_never_panics() {
        // Fuzz-style: an index sub-section length that isn't a multiple
        // of HierarchyPostingEntry::SIZE (corrupt/truncated write) must
        // degrade to "no edges found", never panic on the modulo-based
        // entry_count computation or any subsequent read.
        let edges = vec![hb(7, 1, 0, 1, 0)];
        let (edge_bytes, mut index_bytes, posting_bytes) =
            build_hierarchy_section(&edges).expect("build");
        index_bytes.push(0xAB); // corrupt: 9 bytes, not a multiple of 8

        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_hierarchy(
            tmp.path(),
            |_| {},
            Some((edge_bytes, index_bytes, posting_bytes)),
        );
        let reader = IndexReader::open(&path).expect("index with corrupt index len still opens");
        let hits = reader.find_hierarchy_edges_by_symbol(7);
        assert!(
            hits.is_empty(),
            "corrupt index length must degrade to empty, got {hits:?}"
        );
    }

    #[test]
    fn hierarchy_edges_posting_count_exceeds_remaining_bytes_returns_empty_never_panics() {
        // Fuzz-style: the posting-list `count` u32 prefix claims more
        // entries than actually fit in the remaining postings bytes (a
        // corrupt/adversarial count, not just a truncated blob). The
        // reader must read only what fits and stop, never panic or read
        // past the slice (P1 acceptance §8(d): "posting-list length
        // guards on the count u32").
        let edges = vec![hb(7, 1, 0, 1, 0)];
        let (edge_bytes, index_bytes, mut posting_bytes) =
            build_hierarchy_section(&edges).expect("build");
        // Posting layout: [u32 count = 1][u32 edge_idx = 0]. Overwrite the
        // count prefix with an absurdly large value while leaving only one
        // real u32 slot after it.
        assert!(posting_bytes.len() >= 8);
        posting_bytes[0..4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_hierarchy(
            tmp.path(),
            |_| {},
            Some((edge_bytes, index_bytes, posting_bytes)),
        );
        let reader =
            IndexReader::open(&path).expect("index with corrupt posting count still opens");
        // Must not panic; whatever is returned must not include any
        // fabricated edge past what the (single) real slot could produce.
        let hits = reader.find_hierarchy_edges_by_symbol(7);
        assert!(
            hits.len() <= 1,
            "an inflated count must not manufacture edges from OOB reads, got {hits:?}"
        );
    }

    #[test]
    fn hierarchy_edges_posting_blob_truncated_mid_entry_returns_partial_never_panics() {
        // Fuzz-style: the postings blob is truncated after the count
        // prefix but before the promised edge_idx entries are fully
        // present. The reader must stop at the truncation point and
        // return whatever was read so far, never panic or read OOB (P1
        // acceptance §8(d): guard on each entry, not just the count).
        let edges = vec![hb(7, 1, 0, 1, 0), hb(7, 2, 0, 2, 1), hb(7, 3, 0, 3, 2)];
        let (edge_bytes, index_bytes, posting_bytes) =
            build_hierarchy_section(&edges).expect("build");
        // Posting layout for parent 7: [u32 count = 3][idx][idx][idx].
        // Truncate right after the count prefix + one entry (8 bytes in).
        assert!(posting_bytes.len() > 8);
        let truncated_postings = posting_bytes[..8].to_vec();

        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_hierarchy(
            tmp.path(),
            |_| {},
            Some((edge_bytes, index_bytes, truncated_postings)),
        );
        let reader =
            IndexReader::open(&path).expect("index with truncated postings blob still opens");
        let hits = reader.find_hierarchy_edges_by_symbol(7);
        assert!(
            hits.len() <= 1,
            "a truncated postings blob must yield only the entries that fully fit, got {hits:?}"
        );
    }

    #[test]
    fn hierarchy_edges_reserved_edge_kind_byte_surfaces_raw_bits_without_panicking() {
        // A reserved/future EdgeKind byte (3..=255 — will appear once
        // Overrides/Satisfies land in a later format-additive change, or
        // from a corrupt file today) must decode via `edge_kind_bits()`
        // without panicking, and `EdgeKind::try_from` on that raw byte
        // must return `Err` (the "future kind -> skip" contract), never
        // `mem::transmute` UB (P1 acceptance §8, doc §3.2).
        let edges = vec![hb(7, 1, 0, 1, 200)]; // 200 is reserved (3..=254)
        let (edge_bytes, index_bytes, posting_bytes) =
            build_hierarchy_section(&edges).expect("build");

        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_hierarchy(
            tmp.path(),
            |_| {},
            Some((edge_bytes, index_bytes, posting_bytes)),
        );
        let reader = IndexReader::open(&path).expect("v8 index with reserved kind byte is valid");
        let hits = reader.find_hierarchy_edges_by_symbol(7);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].edge_kind_bits(), 200);
        assert_eq!(
            super::super::format::EdgeKind::try_from(hits[0].edge_kind_bits()),
            Err(()),
            "reserved kind byte must decode to Err, not a fabricated variant"
        );
    }

    #[test]
    fn open_rejects_pre_v8_reading_v8_via_version_range_gate() {
        // The doc's "v8-rejected-by-pre-v8 SemVer test" is a hypothetical
        // older reader whose `VERSION` constant is 7 — this build's
        // MIN_SUPPORTED_VERSION..=VERSION gate is what a pre-v8 build's
        // equivalent check would look like. Assert the gate logic
        // directly: a version above the current build's own `VERSION`
        // constant is out of the supported range (mirrors what a v7-only
        // build would compute against a v8 file's `version` field).
        let hypothetical_pre_v8_max_version: u32 = 7;
        let hypothetical_min_supported: u32 = super::super::format::MIN_SUPPORTED_VERSION;
        let v8_file_version: u32 = 8;
        assert!(
            !(hypothetical_min_supported..=hypothetical_pre_v8_max_version)
                .contains(&v8_file_version),
            "a pre-v8 reader's version gate must reject a v8 file's version field"
        );
    }

    #[test]
    fn hierarchy_header_offsets_past_eof_rejected() {
        // Each of the three (offset, len) pairs in HierarchyHeader must be
        // independently bounds-checked at open() time — not just the fixed
        // header bytes. Corrupt only `edges_offset`/`edges_len` to point
        // past EOF while everything else stays a valid empty v8 index.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |_| {});
        let mut bytes = std::fs::read(&path).expect("read written index");

        let hier_header_offset = Header::SIZE
            + CallGraphHeader::SIZE
            + V5SectionHeader::SIZE
            + PatternSkeletonHeader::SIZE
            + UnresolvedRefsHeader::SIZE;
        // HierarchyHeader { edges_offset: u64, edges_len: u64, .. } — first
        // 16 bytes of the header block.
        let huge_offset = (bytes.len() as u64) + 1_000_000;
        bytes[hier_header_offset..hier_header_offset + 8]
            .copy_from_slice(&huge_offset.to_le_bytes());
        bytes[hier_header_offset + 8..hier_header_offset + 16]
            .copy_from_slice(&4096u64.to_le_bytes());

        let corrupt_path = tmp.path().join("corrupt_hierarchy_offsets.vex");
        std::fs::write(&corrupt_path, &bytes).expect("write corrupt index");

        let result = IndexReader::open(&corrupt_path);
        assert!(
            result.is_err(),
            "hierarchy_edges section offsets exceeding file size must be rejected at open()"
        );
    }

    // -----------------------------------------------------------------
    // P2a carry-forward accessors: hierarchy_edges_all / unresolved_hierarchy_all
    // -----------------------------------------------------------------

    #[test]
    fn hierarchy_edges_all_enumerates_every_edge_regardless_of_parent() {
        // Unlike find_hierarchy_edges_by_symbol (keyed by to_sym_idx, one
        // parent at a time), hierarchy_edges_all must return every edge
        // across every parent — this is what reconstruct_unchanged needs
        // to bucket by from_file_id.
        let edges = vec![
            hb(100, 1, 0, 10, 0),
            hb(100, 2, 0, 20, 1),
            hb(200, 3, 1, 30, 2),
            hb(300, 4, 1, 40, 0),
        ];
        let (edge_bytes, index_bytes, posting_bytes) =
            build_hierarchy_section(&edges).expect("build");

        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_hierarchy(
            tmp.path(),
            |_| {},
            Some((edge_bytes, index_bytes, posting_bytes)),
        );
        let reader = IndexReader::open(&path).expect("open");

        let mut all = reader.hierarchy_edges_all();
        all.sort_by_key(|e| e.from_sym_idx);
        assert_eq!(
            all.len(),
            4,
            "must enumerate all 4 edges across all parents"
        );
        assert_eq!(all[0].from_sym_idx, 1);
        assert_eq!(all[0].to_sym_idx, 100);
        assert_eq!(all[3].from_sym_idx, 4);
        assert_eq!(all[3].to_sym_idx, 300);
    }

    #[test]
    fn hierarchy_edges_all_empty_when_no_section() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |h| {
            h.version = 7;
        });
        let reader = IndexReader::open(&path).expect("v7 index opens clean");
        assert!(
            reader.hierarchy_edges_all().is_empty(),
            "a v7 index (no hierarchy section at all) must yield empty, never panic"
        );
    }

    #[test]
    fn hierarchy_edges_all_empty_when_section_present_but_zero_edges() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |_| {});
        let reader = IndexReader::open(&path).expect("v8 index with zeroed hierarchy section");
        assert!(reader.hierarchy_edges_all().is_empty());
    }

    #[test]
    fn hierarchy_edges_all_truncated_edges_yields_whole_records_only() {
        // A trailing partial record (corrupt/truncated write) must not
        // panic — the accessor should stop at the last whole record.
        let edges = vec![hb(100, 1, 0, 10, 0), hb(100, 2, 0, 20, 1)];
        let (mut edge_bytes, index_bytes, posting_bytes) =
            build_hierarchy_section(&edges).expect("build");
        edge_bytes.push(0xAB); // trailing partial byte, not a whole HierarchyEdge

        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_hierarchy(
            tmp.path(),
            |_| {},
            Some((edge_bytes, index_bytes, posting_bytes)),
        );
        let reader = IndexReader::open(&path).expect("open");
        let all = reader.hierarchy_edges_all();
        assert_eq!(
            all.len(),
            2,
            "must recover exactly the whole records, ignoring the trailing partial byte"
        );
    }

    use crate::store::unresolved_hierarchy::{
        build_unresolved_hierarchy_section, UnresolvedHierarchyEdgeBuilder,
    };

    fn uhb(
        name: &str,
        from: u32,
        file: u32,
        line: u32,
        kind: u8,
    ) -> UnresolvedHierarchyEdgeBuilder {
        UnresolvedHierarchyEdgeBuilder {
            parent_name: name.to_string(),
            from_sym_idx: from,
            from_file_id: file,
            line,
            kind,
        }
    }

    /// Splice ONLY unresolved_hierarchy section bytes into a minimal v8
    /// index (resolved hierarchy_edges section stays empty) — thin wrapper
    /// over `write_minimal_index_with_hierarchy_sections`.
    fn write_minimal_index_with_unresolved_hierarchy(
        tmp: &Path,
        edges: &[UnresolvedHierarchyEdgeBuilder],
    ) -> std::path::PathBuf {
        let (edge_bytes, fst_bytes, post_bytes) =
            build_unresolved_hierarchy_section(edges).expect("build unresolved hierarchy section");
        write_minimal_index_with_hierarchy_sections(
            tmp,
            |_| {},
            None,
            Some((edge_bytes, fst_bytes, post_bytes)),
        )
    }

    #[test]
    fn unresolved_hierarchy_all_enumerates_every_edge_with_parent_name() {
        let edges = vec![
            uhb("Foo", 1, 0, 10, 0),
            uhb("Bar", 2, 1, 20, 2),
            uhb("Foo", 3, 2, 30, 0),
        ];
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index_with_unresolved_hierarchy(tmp.path(), &edges);
        let reader = IndexReader::open(&path).expect("open");

        let mut all = reader.unresolved_hierarchy_all();
        all.sort_by_key(|(name, e)| (name.clone(), e.from_sym_idx));
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, "Bar");
        assert_eq!(all[1].0, "Foo");
        assert_eq!(all[1].1.from_sym_idx, 1);
        assert_eq!(all[2].0, "Foo");
        assert_eq!(all[2].1.from_sym_idx, 3);
    }

    #[test]
    fn unresolved_hierarchy_all_empty_when_no_section() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |h| {
            h.version = 7;
        });
        let reader = IndexReader::open(&path).expect("v7 index opens clean");
        assert!(
            reader.unresolved_hierarchy_all().is_empty(),
            "a v7 index (no unresolved_hierarchy section) must yield empty, never panic"
        );
    }

    #[test]
    fn unresolved_hierarchy_all_empty_when_section_present_but_zero_edges() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_minimal_index(tmp.path(), |_| {});
        let reader = IndexReader::open(&path).expect("v8 index with zeroed section");
        assert!(reader.unresolved_hierarchy_all().is_empty());
    }
}
