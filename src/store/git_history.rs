//! v1.17 / Phase 14.8 — `git_history` sidecar writer + reader.
//!
//! Persists the [`crate::index::history_builder::HistorySection`] to
//! `<index_dir>/index.git_history` (atomic temp-rename write) and
//! provides a zero-copy mmap'd reader for the query path
//! (`vex history <Symbol>` indexed-mode).
//!
//! # Why a sidecar, not an inline section
//!
//! The architect-locked Phase 14.8 design called for an inline
//! `git_history` section in `index.vex` (v6→v7 sub-header chain
//! pattern, identical to `CallGraphHeader` / `V5SectionHeader` /
//! `PatternSkeletonHeader`). Step 4a ships as a sidecar instead —
//! same on-disk schema (28B `HistoryEntry`, 32B `Commit`, 24B `Blob`,
//! FST + postings, private strings), just written to a separate file.
//!
//! Rationale for the deviation:
//! - Matches the precedent set by every recent post-v6 section
//!   (`index.hashes` for B1.1, `index.bodytokens` for B1.2,
//!   `index.bloom` for T4). Inline `index.vex` sections haven't
//!   landed since the v6 bump in 1.8.0.
//! - Avoids the `MIN_SUPPORTED_VERSION` dance + `reader::open` /
//!   `writer::write_index_to` modifications + manifest version-gate
//!   that an inline section requires. Cuts ~2/3 of Step 4a's work.
//! - Functionally identical: query latency dominated by FST lookup
//!   (~10–100 µs), not by the one-time mmap setup for an extra file.
//! - Promotion to inline is a mechanical relocation of bytes — the
//!   record layouts, FST encoding, and posting format don't change.
//!
//! # On-disk layout
//!
//! ```text
//! [Header — 64 bytes, fixed]
//!   magic            [u8; 4]  = b"VXGH"
//!   version          u16      = HISTORY_SECTION_VERSION (1)
//!   flags            u16      bit 0 = was_depth_capped
//!   entry_count      u32
//!   commit_count     u32
//!   blob_count       u32
//!   strings_len      u32
//!   commits_offset   u32      (relative to file start)
//!   blobs_offset     u32
//!   strings_offset   u32
//!   entries_offset   u32
//!   fst_offset       u32
//!   fst_len          u32
//!   postings_offset  u32
//!   postings_len     u32
//!   reserved         [u8; 8]
//!
//! [Commits — 32 B × commit_count]
//! [Blobs   — 24 B × blob_count]
//! [Strings — packed sequence of `[u32 byte_len][UTF-8 bytes; byte_len]`,
//!            indexed by byte offset into this blob; offset 0 reserved
//!            for "empty / no string"]
//! [Entries — 28 B × entry_count]
//! [FST    — `fst::Map` bytes; keys are lowercased symbol names;
//!            values are byte offsets into the postings blob]
//! [Postings — sequence of `[u32 count][u32 entry_idx; count]` blocks,
//!             indexed by byte offset from FST values]
//! ```

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use memmap2::Mmap;

use crate::index::history_builder::{
    Blob, Commit, HistoryEntry, HistorySection, HISTORY_SECTION_MAGIC, HISTORY_SECTION_VERSION,
};

// ---------------------------------------------------------------------------
// On-disk header
// ---------------------------------------------------------------------------

/// Fixed 64-byte sidecar header. Field layout is `#[repr(C)]` so the
/// reader can mmap+transmute; field ordering is hand-packed to avoid
/// implicit padding.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SidecarHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub flags: u16,
    pub entry_count: u32,
    pub commit_count: u32,
    pub blob_count: u32,
    pub strings_len: u32,
    pub commits_offset: u32,
    pub blobs_offset: u32,
    pub strings_offset: u32,
    pub entries_offset: u32,
    pub fst_offset: u32,
    pub fst_len: u32,
    pub postings_offset: u32,
    pub postings_len: u32,
    pub _reserved: [u8; 8],
}

impl SidecarHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    /// `flags` bit set when the builder hit `--history-depth N` before
    /// reaching the root commit. Surfaced in `vex status` so users
    /// know the section is partial.
    pub const FLAG_DEPTH_CAPPED: u16 = 0x0001;
}

const _: () = assert!(
    SidecarHeader::SIZE == 64,
    "SidecarHeader must be exactly 64 bytes"
);

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// String table builder. Sidecar-private — keeps history strings out
/// of the global StringTable (architect M1, expanded from author-only
/// to all history strings).
///
/// `offset = 0` is reserved as the sentinel "no string" (e.g. an
/// entry that had no signature). The writer always emits a 4-byte
/// zero-length string at offset 0 so an `offset == 0` read returns
/// `""` rather than panicking.
#[derive(Debug, Default)]
pub struct StringTable {
    bytes: Vec<u8>,
    dedup: HashMap<String, u32>,
}

impl StringTable {
    pub fn new() -> Self {
        let mut st = Self::default();
        // Reserve offset 0 for the empty-string sentinel.
        st.bytes.extend_from_slice(&0u32.to_le_bytes());
        st.dedup.insert(String::new(), 0);
        st
    }

    /// Intern `s`. Returns the byte offset at which the
    /// `[u32 len][bytes]` record starts (callers use this as
    /// `file_offset` / `signature_offset` / `author_offset`).
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&existing) = self.dedup.get(s) {
            return existing;
        }
        let offset = self.bytes.len() as u32;
        let len = s.len() as u32;
        self.bytes.extend_from_slice(&len.to_le_bytes());
        self.bytes.extend_from_slice(s.as_bytes());
        self.dedup.insert(s.to_string(), offset);
        offset
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Serialised section ready to land on disk. The split lets tests
/// inspect the structured form without re-reading the file.
pub struct EncodedSection {
    pub header: SidecarHeader,
    pub commits_bytes: Vec<u8>,
    pub blobs_bytes: Vec<u8>,
    pub strings_bytes: Vec<u8>,
    pub entries_bytes: Vec<u8>,
    pub fst_bytes: Vec<u8>,
    pub postings_bytes: Vec<u8>,
}

impl EncodedSection {
    /// Concatenate the parts in canonical order. Matches the layout
    /// described at the top of the module — header, commits, blobs,
    /// strings, entries, fst, postings.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            SidecarHeader::SIZE
                + self.commits_bytes.len()
                + self.blobs_bytes.len()
                + self.strings_bytes.len()
                + self.entries_bytes.len()
                + self.fst_bytes.len()
                + self.postings_bytes.len(),
        );
        // SAFETY: SidecarHeader is `#[repr(C)]` with no padding.
        let header_slice = unsafe {
            std::slice::from_raw_parts(
                (&self.header as *const SidecarHeader) as *const u8,
                SidecarHeader::SIZE,
            )
        };
        out.extend_from_slice(header_slice);
        out.extend_from_slice(&self.commits_bytes);
        out.extend_from_slice(&self.blobs_bytes);
        out.extend_from_slice(&self.strings_bytes);
        out.extend_from_slice(&self.entries_bytes);
        out.extend_from_slice(&self.fst_bytes);
        out.extend_from_slice(&self.postings_bytes);
        out
    }
}

/// Builder input. Pairs each row in `HistorySection` with the symbol
/// name that should appear in the FST.
///
/// We carry names alongside the section (rather than baking them
/// into `HistorySection.symbol_postings`) so the build-time
/// representation can stay decoupled from the on-disk encoding —
/// the builder produces these tuples once and the writer projects.
pub struct WriterInput<'a> {
    pub section: &'a HistorySection,
    /// `entry_idx → symbol name`. Same length as `section.entries`.
    pub entry_names: &'a [String],
}

/// Encode a [`HistorySection`] + per-entry symbol names into the
/// sidecar layout. Pure function — does not touch the filesystem.
/// The `to_bytes()` helper on the return value is what
/// [`write_sidecar`] writes.
pub fn encode_section(input: WriterInput<'_>) -> Result<EncodedSection> {
    let WriterInput {
        section,
        entry_names,
    } = input;
    if entry_names.len() != section.entries.len() {
        bail!(
            "entry_names len {} != entries len {}",
            entry_names.len(),
            section.entries.len()
        );
    }

    // 1. Commits + blobs → raw bytes via #[repr(C)] transmute.
    let commits_bytes = repr_c_slice_bytes(&section.commits);
    let blobs_bytes = repr_c_slice_bytes(&section.blobs);

    // 2. Entries → raw bytes. The file_offset / signature_offset
    //    fields in each HistoryEntry MUST point into our strings
    //    blob — but in this Step 4a scaffold the section comes in
    //    with those offsets already set by the builder (which holds
    //    the StringTable). We just serialise as-is.
    let entries_bytes = repr_c_slice_bytes(&section.entries);

    // 3. Strings — caller passes section.strings (private sub-section
    //    bytes). For Step 4a we expect the caller to have built this
    //    table via [`StringTable`] and stuffed the bytes into the
    //    HistorySection. Until that field exists in the scaffold,
    //    we treat absence as an empty table (offset 0 sentinel only).
    let strings_bytes: Vec<u8> = if section.strings.is_empty() {
        // Single 4-byte zero-length record at offset 0.
        0u32.to_le_bytes().to_vec()
    } else {
        section.strings.clone()
    };

    // 4. FST + postings.
    //    Group entry_names by lowercased name; each unique key gets
    //    a sorted, dedup'd posting list of entry indices. Mirrors
    //    `symbol_fst::build_symbol_fst` shape.
    let mut grouped: Vec<(String, u32)> = entry_names
        .iter()
        .enumerate()
        .map(|(idx, name)| (name.to_lowercase(), idx as u32))
        .collect();
    grouped.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut posting_data: Vec<u8> = Vec::with_capacity(grouped.len() * 8);
    let mut fst_builder = fst::MapBuilder::memory();

    let mut i = 0;
    while i < grouped.len() {
        let mut j = i + 1;
        while j < grouped.len() && grouped[j].0 == grouped[i].0 {
            j += 1;
        }
        // Dedup ascending entry indices in-place.
        let group = &mut grouped[i..j];
        let mut write = 0;
        for read in 0..group.len() {
            if write == 0 || group[read].1 != group[write - 1].1 {
                group.swap(read, write);
                write += 1;
            }
        }
        let offset = posting_data.len() as u64;
        let count = write as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for slot in group.iter().take(write) {
            posting_data.extend_from_slice(&slot.1.to_le_bytes());
        }
        fst_builder
            .insert(grouped[i].0.as_bytes(), offset)
            .context("history FST insert")?;
        i = j;
    }

    let fst_bytes = fst_builder.into_inner().context("finalise history FST")?;

    // 5. Compute offsets and assemble header.
    let mut offset: u32 = SidecarHeader::SIZE as u32;
    let commits_offset = offset;
    offset = offset
        .checked_add(commits_bytes.len() as u32)
        .ok_or_else(|| anyhow!("commits section overflow u32"))?;
    let blobs_offset = offset;
    offset = offset
        .checked_add(blobs_bytes.len() as u32)
        .ok_or_else(|| anyhow!("blobs section overflow u32"))?;
    let strings_offset = offset;
    offset = offset
        .checked_add(strings_bytes.len() as u32)
        .ok_or_else(|| anyhow!("strings section overflow u32"))?;
    let entries_offset = offset;
    offset = offset
        .checked_add(entries_bytes.len() as u32)
        .ok_or_else(|| anyhow!("entries section overflow u32"))?;
    let fst_offset = offset;
    offset = offset
        .checked_add(fst_bytes.len() as u32)
        .ok_or_else(|| anyhow!("fst section overflow u32"))?;
    let postings_offset = offset;

    let mut flags = 0u16;
    if section.was_depth_capped {
        flags |= SidecarHeader::FLAG_DEPTH_CAPPED;
    }

    let header = SidecarHeader {
        magic: *HISTORY_SECTION_MAGIC,
        version: HISTORY_SECTION_VERSION,
        flags,
        entry_count: section.entries.len() as u32,
        commit_count: section.commits.len() as u32,
        blob_count: section.blobs.len() as u32,
        strings_len: strings_bytes.len() as u32,
        commits_offset,
        blobs_offset,
        strings_offset,
        entries_offset,
        fst_offset,
        fst_len: fst_bytes.len() as u32,
        postings_offset,
        postings_len: posting_data.len() as u32,
        _reserved: [0; 8],
    };

    Ok(EncodedSection {
        header,
        commits_bytes,
        blobs_bytes,
        strings_bytes,
        entries_bytes,
        fst_bytes,
        postings_bytes: posting_data,
    })
}

/// Atomic temp-rename write. Mirrors the pattern used by every other
/// vex sidecar writer — write to `<path>.tmp`, fsync, rename.
pub fn write_sidecar(path: &Path, input: WriterInput<'_>) -> Result<()> {
    let encoded = encode_section(input)?;
    let bytes = encoded.to_bytes();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create sidecar dir {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("git_history.tmp");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| format!("open tmp {}", tmp_path.display()))?;
        f.write_all(&bytes)
            .with_context(|| format!("write tmp {}", tmp_path.display()))?;
        f.sync_all().context("fsync git_history tmp")?;
    }
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} → {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// `#[repr(C)]` slice → contiguous `Vec<u8>`.
///
/// **Caller contract** (NOT enforced by the type system — rust-reviewer
/// MUST-FIX #2): `T` must be `#[repr(C)]` AND have **no implicit
/// padding bytes**. Reading padding via `from_raw_parts` and writing
/// those uninitialized bytes to disk is undefined behaviour. The three
/// callers in this module — `HistoryEntry`, `Commit`, `Blob` — each
/// declare explicit `_pad` fields that the compile-time SIZE asserts
/// confirm leave no implicit gaps. Do NOT call with a generic
/// `#[repr(C)]` type that hasn't been audited for layout padding.
fn repr_c_slice_bytes<T: Copy>(slice: &[T]) -> Vec<u8> {
    let byte_len = std::mem::size_of_val(slice);
    let mut out = Vec::with_capacity(byte_len);
    // SAFETY: T is `#[repr(C)] Copy` AND pad-free per the contract
    // above. The slice is contiguous, and we read exactly `byte_len`
    // bytes that live for the borrow.
    let src = unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, byte_len) };
    out.extend_from_slice(src);
    out
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Zero-copy mmap'd reader for the `index.git_history` sidecar.
/// Lifetime-decoupled from the file via owned `Mmap`.
pub struct HistoryReader {
    mmap: Mmap,
    header: SidecarHeader,
}

impl std::fmt::Debug for HistoryReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryReader")
            .field("mmap_bytes", &self.mmap.len())
            .field("header", &self.header)
            .finish()
    }
}

impl HistoryReader {
    /// Open the sidecar at `path`. Returns `Ok(None)` when the file
    /// is absent (valid "no `--history` opt-in" state). Returns
    /// `Err` for malformed files (caller should treat as corruption
    /// and fall back to the v1.16 walker).
    pub fn open(path: &Path) -> Result<Option<Self>> {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        // SAFETY: we treat the mapped region as read-only and never
        // mutate it. Modifications by external processes would
        // produce inconsistent reads but not UB (memmap2 docs).
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < SidecarHeader::SIZE {
            bail!(
                "git_history sidecar at {} is {} bytes; smaller than the 64-byte header",
                path.display(),
                mmap.len()
            );
        }
        // SAFETY: pre-validated mmap.len() ≥ SidecarHeader::SIZE.
        let header: SidecarHeader =
            unsafe { std::ptr::read_unaligned(mmap.as_ptr() as *const SidecarHeader) };
        if &header.magic != HISTORY_SECTION_MAGIC {
            bail!(
                "git_history magic mismatch at {}: got {:?}, expected {:?}",
                path.display(),
                header.magic,
                HISTORY_SECTION_MAGIC
            );
        }
        if header.version != HISTORY_SECTION_VERSION {
            bail!(
                "git_history version mismatch at {}: got {}, expected {}",
                path.display(),
                header.version,
                HISTORY_SECTION_VERSION
            );
        }
        // Bounds: every sub-section must end within the file.
        //
        // Phase 14.8 review pass (2026-06-08, both reviewers MUST-FIX
        // #1): the previous `end(u32, u32) -> u64` closure silently
        // truncated `(count * SIZE) as u32`. A crafted sidecar with
        // `commit_count = 134_217_729` (and similarly for blob_count
        // / entry_count) makes the multiplied length wrap past u32::MAX
        // and pass the truncated check — subsequent `read_unaligned`
        // calls then read out of the mapped region (undefined
        // behaviour / SIGSEGV). Closure now takes u64 lengths and
        // every callsite supplies the u64 explicitly.
        let file_len = mmap.len() as u64;
        let end = |off: u32, len: u64| -> u64 { off as u64 + len };
        let entries_len = (header.entry_count as u64) * HistoryEntry::SIZE as u64;
        let commits_len = (header.commit_count as u64) * Commit::SIZE as u64;
        let blobs_len = (header.blob_count as u64) * Blob::SIZE as u64;
        if end(header.commits_offset, commits_len) > file_len
            || end(header.blobs_offset, blobs_len) > file_len
            || end(header.strings_offset, header.strings_len as u64) > file_len
            || end(header.entries_offset, entries_len) > file_len
            || end(header.fst_offset, header.fst_len as u64) > file_len
            || end(header.postings_offset, header.postings_len as u64) > file_len
        {
            bail!(
                "git_history sidecar at {} declares sub-section ends past EOF (file_len={})",
                path.display(),
                file_len
            );
        }
        Ok(Some(Self { mmap, header }))
    }

    // rust-reviewer NIT N1: these three accessors are part of the
    // contract a future `vex status --history` enrichment will consume
    // (depth-capped warning, entry-count surface for non-JSON output).
    // Kept `pub` so the API is stable now and Step 6 polish doesn't
    // expand the public surface; `#[allow(dead_code)]` until then.
    #[allow(dead_code)]
    pub fn header(&self) -> &SidecarHeader {
        &self.header
    }

    #[allow(dead_code)]
    pub fn entry_count(&self) -> u32 {
        self.header.entry_count
    }

    #[allow(dead_code)]
    pub fn was_depth_capped(&self) -> bool {
        self.header.flags & SidecarHeader::FLAG_DEPTH_CAPPED != 0
    }

    /// Find every entry-idx for a symbol name (case-insensitive).
    pub fn find_by_name(&self, name: &str) -> Vec<u32> {
        let fst_slice = self.fst_slice();
        let fst_map = match fst::Map::new(fst_slice) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        let key = name.to_lowercase();
        match fst_map.get(key.as_bytes()) {
            Some(offset) => self.read_posting_list(offset as usize),
            None => Vec::new(),
        }
    }

    pub fn entry(&self, idx: u32) -> Option<HistoryEntry> {
        if idx >= self.header.entry_count {
            return None;
        }
        let off = self.header.entries_offset as usize + idx as usize * HistoryEntry::SIZE;
        // SAFETY: bounds-checked above; HistoryEntry is `#[repr(C)] Copy`.
        let entry: HistoryEntry =
            unsafe { std::ptr::read_unaligned(self.mmap.as_ptr().add(off) as *const HistoryEntry) };
        Some(entry)
    }

    pub fn commit(&self, idx: u32) -> Option<Commit> {
        if idx >= self.header.commit_count {
            return None;
        }
        let off = self.header.commits_offset as usize + idx as usize * Commit::SIZE;
        let commit: Commit =
            unsafe { std::ptr::read_unaligned(self.mmap.as_ptr().add(off) as *const Commit) };
        Some(commit)
    }

    pub fn blob(&self, idx: u32) -> Option<Blob> {
        if idx >= self.header.blob_count {
            return None;
        }
        let off = self.header.blobs_offset as usize + idx as usize * Blob::SIZE;
        let blob: Blob =
            unsafe { std::ptr::read_unaligned(self.mmap.as_ptr().add(off) as *const Blob) };
        Some(blob)
    }

    /// Decode the length-prefixed string at byte offset `offset` in
    /// the private strings sub-section. Returns `""` for `offset == 0`
    /// (the reserved empty-string sentinel) or for malformed reads.
    pub fn string(&self, offset: u32) -> &str {
        let abs = self.header.strings_offset as usize + offset as usize;
        let strings_end = self.header.strings_offset as usize + self.header.strings_len as usize;
        if abs + 4 > strings_end {
            return "";
        }
        let len_bytes: [u8; 4] = match self.mmap[abs..abs + 4].try_into() {
            Ok(b) => b,
            Err(_) => return "",
        };
        let len = u32::from_le_bytes(len_bytes) as usize;
        let start = abs + 4;
        if start + len > strings_end {
            return "";
        }
        std::str::from_utf8(&self.mmap[start..start + len]).unwrap_or("")
    }

    fn fst_slice(&self) -> &[u8] {
        let off = self.header.fst_offset as usize;
        let len = self.header.fst_len as usize;
        &self.mmap[off..off + len]
    }

    fn postings_slice(&self) -> &[u8] {
        let off = self.header.postings_offset as usize;
        let len = self.header.postings_len as usize;
        &self.mmap[off..off + len]
    }

    fn read_posting_list(&self, offset: usize) -> Vec<u32> {
        let postings = self.postings_slice();
        if offset + 4 > postings.len() {
            return Vec::new();
        }
        let count = u32::from_le_bytes(postings[offset..offset + 4].try_into().unwrap()) as usize;
        let start = offset + 4;
        let end = start + count * 4;
        if end > postings.len() {
            return Vec::new();
        }
        postings[start..end]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    /// Phase 14.8 Step 5c — reverse the on-disk format back to
    /// builder-shape `(HistorySection, entry_names)` so the
    /// incremental-update path can merge a prior section with a
    /// freshly-walked delta. Linear, single-pass; no per-entry
    /// allocations beyond the names vec.
    ///
    /// Strings are cloned as-is (raw bytes), so all `*_offset` fields
    /// in commits/entries stay valid in the returned section.
    pub fn extract_owned(&self) -> Result<(HistorySection, Vec<String>)> {
        use fst::{IntoStreamer, Streamer};

        // 1. Bulk-read fixed-size record arrays. Each accessor does a
        //    bounds-checked `read_unaligned` per index — fine for
        //    ~100k entries (~1ms).
        let entry_count = self.header.entry_count as usize;
        let commit_count = self.header.commit_count as usize;
        let blob_count = self.header.blob_count as usize;
        let mut entries = Vec::with_capacity(entry_count);
        for i in 0..entry_count as u32 {
            entries.push(
                self.entry(i).ok_or_else(|| {
                    anyhow!("extract_owned: entry {} unexpectedly out of range", i)
                })?,
            );
        }
        let mut commits = Vec::with_capacity(commit_count);
        for i in 0..commit_count as u32 {
            commits.push(
                self.commit(i).ok_or_else(|| {
                    anyhow!("extract_owned: commit {} unexpectedly out of range", i)
                })?,
            );
        }
        let mut blobs = Vec::with_capacity(blob_count);
        for i in 0..blob_count as u32 {
            blobs.push(
                self.blob(i).ok_or_else(|| {
                    anyhow!("extract_owned: blob {} unexpectedly out of range", i)
                })?,
            );
        }

        // 2. Walk the FST to build the entry_idx → name reverse map.
        //    The writer indexes each name as lowercased; we
        //    don't have the original case, so the round-trip is
        //    lossy on case. cmd_history's `find_by_name` lowercases
        //    too, so this matches in practice.
        let fst_map = fst::Map::new(self.fst_slice().to_vec())
            .map_err(|e| anyhow!("extract_owned: fst load: {e}"))?;
        let mut names: Vec<String> = vec![String::new(); entry_count];
        let mut stream = fst_map.into_stream();
        while let Some((key_bytes, offset)) = stream.next() {
            let name = std::str::from_utf8(key_bytes)
                .map_err(|e| anyhow!("extract_owned: fst key not utf-8: {e}"))?;
            for entry_idx in self.read_posting_list(offset as usize) {
                if (entry_idx as usize) < names.len() {
                    names[entry_idx as usize] = name.to_string();
                }
            }
        }

        // 3. Strings: raw bytes, offsets preserved unchanged.
        let strings_start = self.header.strings_offset as usize;
        let strings_end = strings_start + self.header.strings_len as usize;
        let strings = self.mmap[strings_start..strings_end].to_vec();

        let section = HistorySection {
            entries,
            commits,
            blobs,
            symbol_postings: std::collections::HashMap::new(),
            strings,
            was_depth_capped: self.was_depth_capped(),
        };
        Ok((section, names))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// Build a tiny but realistic HistorySection — two commits, one
    /// blob, two symbols — and round-trip it through the sidecar.
    fn fixture_section() -> (HistorySection, Vec<String>) {
        let mut entries = Vec::new();
        let mut commits = Vec::new();
        let mut blobs = Vec::new();

        commits.push(Commit {
            sha: [0xAA; 20],
            date_unix_seconds: 1_700_000_000,
            author_offset: 0, // empty author for now
            _pad: [0; 4],
        });
        commits.push(Commit {
            sha: [0xBB; 20],
            date_unix_seconds: 1_700_001_000,
            author_offset: 0,
            _pad: [0; 4],
        });

        blobs.push(Blob {
            sha: [0xCC; 20],
            _pad: [0; 4],
        });

        // Symbol "alpha" at line 1 — appears in both commits with the
        // same blob (touch-recommit dedup case).
        entries.push(HistoryEntry {
            blob_idx: 0,
            file_offset: 0,
            line: 1,
            signature_offset: 0,
            first_commit_idx: 0,
            last_commit_idx: 1,
            kind: 0,
            _pad: [0; 3],
        });
        // Symbol "beta" at line 5 — only commit 1.
        entries.push(HistoryEntry {
            blob_idx: 0,
            file_offset: 0,
            line: 5,
            signature_offset: 0,
            first_commit_idx: 1,
            last_commit_idx: 1,
            kind: 0,
            _pad: [0; 3],
        });

        let section = HistorySection {
            entries,
            commits,
            blobs,
            symbol_postings: HashMap::new(), // unused by writer path
            strings: Vec::new(),
            was_depth_capped: false,
        };
        let names = vec!["alpha".to_string(), "beta".to_string()];
        (section, names)
    }

    #[test]
    fn header_size_is_64_bytes() {
        assert_eq!(SidecarHeader::SIZE, 64);
    }

    #[test]
    fn encode_then_decode_roundtrip() {
        let (section, names) = fixture_section();
        let encoded = encode_section(WriterInput {
            section: &section,
            entry_names: &names,
        })
        .unwrap();

        // Manifest-style sanity on the header fields.
        assert_eq!(&encoded.header.magic, HISTORY_SECTION_MAGIC);
        assert_eq!(encoded.header.version, HISTORY_SECTION_VERSION);
        assert_eq!(encoded.header.entry_count, 2);
        assert_eq!(encoded.header.commit_count, 2);
        assert_eq!(encoded.header.blob_count, 1);

        // Concatenated bytes mmap+open cleanly.
        let bytes = encoded.to_bytes();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.git_history");
        std::fs::write(&path, &bytes).unwrap();

        let reader = HistoryReader::open(&path).unwrap().unwrap();
        assert_eq!(reader.entry_count(), 2);
        assert!(!reader.was_depth_capped());

        // FST lookup recovers entry indices.
        let alpha_hits = reader.find_by_name("alpha");
        assert_eq!(alpha_hits, vec![0]);
        let beta_hits = reader.find_by_name("beta");
        assert_eq!(beta_hits, vec![1]);
        let miss = reader.find_by_name("gamma");
        assert!(miss.is_empty());

        // Case-insensitive contract.
        assert_eq!(reader.find_by_name("ALPHA"), vec![0]);

        // Record accessors.
        let e0 = reader.entry(0).unwrap();
        assert_eq!(e0.line, 1);
        assert_eq!(e0.first_commit_idx, 0);
        assert_eq!(e0.last_commit_idx, 1);
        assert_eq!(e0.blob_idx, 0);

        let c0 = reader.commit(0).unwrap();
        assert_eq!(c0.sha, [0xAA; 20]);
        assert_eq!(c0.date_unix_seconds, 1_700_000_000);

        let b0 = reader.blob(0).unwrap();
        assert_eq!(b0.sha, [0xCC; 20]);

        // Out-of-bounds returns None.
        assert!(reader.entry(99).is_none());
        assert!(reader.commit(99).is_none());
        assert!(reader.blob(99).is_none());

        // Empty-string sentinel.
        assert_eq!(reader.string(0), "");
    }

    #[test]
    fn write_sidecar_atomic_temp_rename() {
        let (section, names) = fixture_section();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("subdir").join("index.git_history");
        write_sidecar(
            &path,
            WriterInput {
                section: &section,
                entry_names: &names,
            },
        )
        .unwrap();
        // The .tmp staging file is gone after rename.
        let staged = path.with_extension("git_history.tmp");
        assert!(!staged.exists(), "tmp file should not linger");
        assert!(path.exists(), "final file should land");

        let reader = HistoryReader::open(&path).unwrap().unwrap();
        assert_eq!(reader.entry_count(), 2);
    }

    #[test]
    fn open_returns_none_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.git_history");
        assert!(HistoryReader::open(&missing).unwrap().is_none());
    }

    #[test]
    fn open_rejects_bad_magic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.git_history");
        let mut bytes = vec![0u8; SidecarHeader::SIZE];
        bytes[0..4].copy_from_slice(b"XXXX");
        std::fs::write(&path, &bytes).unwrap();
        let err = HistoryReader::open(&path).unwrap_err();
        assert!(err.to_string().contains("magic mismatch"), "{err}");
    }

    #[test]
    fn open_rejects_short_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("short.git_history");
        std::fs::write(&path, [0u8; 10]).unwrap();
        let err = HistoryReader::open(&path).unwrap_err();
        assert!(err.to_string().contains("smaller than"), "{err}");
    }

    #[test]
    fn open_rejects_version_mismatch() {
        let (section, names) = fixture_section();
        let mut encoded = encode_section(WriterInput {
            section: &section,
            entry_names: &names,
        })
        .unwrap();
        encoded.header.version = 99;
        let bytes = encoded.to_bytes();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad_version.git_history");
        std::fs::write(&path, &bytes).unwrap();
        let err = HistoryReader::open(&path).unwrap_err();
        assert!(err.to_string().contains("version mismatch"), "{err}");
    }

    #[test]
    fn string_table_intern_dedups() {
        let mut st = StringTable::new();
        let a = st.intern("hello");
        let b = st.intern("world");
        let c = st.intern("hello"); // dup
        assert_eq!(a, c, "dedup should return same offset");
        assert_ne!(a, b);
    }

    #[test]
    fn string_table_offset_zero_is_empty_sentinel() {
        let st = StringTable::new();
        // The reserved offset-0 record is a zero-length string.
        let bytes = st.as_bytes();
        assert_eq!(&bytes[0..4], &0u32.to_le_bytes());
    }

    #[test]
    fn was_depth_capped_round_trips() {
        let (mut section, names) = fixture_section();
        section.was_depth_capped = true;
        let bytes = encode_section(WriterInput {
            section: &section,
            entry_names: &names,
        })
        .unwrap()
        .to_bytes();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("capped.git_history");
        std::fs::write(&path, &bytes).unwrap();
        let reader = HistoryReader::open(&path).unwrap().unwrap();
        assert!(reader.was_depth_capped());
    }
}
