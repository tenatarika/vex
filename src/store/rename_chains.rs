//! Phase 14.10 — `rename_chains` sidecar writer + reader (VEXR v1).
// The full public surface will be wired in later Phase 14.10 steps.
// Suppress dead_code until then; clippy will catch any items that
// remain unwired after the orchestration phase lands.
#![allow(dead_code)]
//!
//! Persists symbol-rename-chain data to `<index_dir>/index.rename_chains`.
//! The sidecar couples two hash guards to detect staleness on open:
//!
//! - `body_tokens_hash`: xxh3_64 over all body_tokens bytes in sym_idx order;
//!   a parser-version bump that changes body extraction invalidates the sidecar.
//! - `history_tip_sha_prefix`: the raw 20-byte SHA of the history sidecar's tip
//!   commit; if history was rebuilt under us the rename chains are stale.
//!
//! Absence is NOT an error — it signals a cold-start or a legacy index that
//! predates Phase 14.10. Callers that receive `Ok(None)` degrade to singleton
//! chain behaviour (each entry is its own chain).
//!
//! # On-disk layout
//!
//! ```text
//! [Header — 48 bytes, #[repr(C)]]
//!   magic                   [u8; 4]  = b"VEXR"
//!   version                 u16      = 1
//!   _pad0                   u16
//!   chain_count             u32
//!   forward_count           u32      total entries-with-chain
//!   member_count            u32      total entry refs across all chains
//!   body_tokens_hash        u64      xxh3_64 over body_tokens bytes in sym_idx order
//!   history_tip_sha_prefix  [u8; 20] raw bytes of history sidecar's tip SHA
//!
//! [ForwardEntry × forward_count]   16 B each, sorted by entry_idx
//!   entry_idx               u32
//!   chain_id                u64
//!   score                   f32
//!
//! [ChainTableEntry × chain_count]  16 B each, sorted by chain_id
//!   chain_id                u64
//!   member_offset           u32   into the flat member list
//!   member_count            u32
//!
//! [u32 × member_count]      flat member list
//! ```

use std::path::Path;

use anyhow::{bail, Context, Result};
use memmap2::Mmap;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAGIC: &[u8; 4] = b"VEXR";
const VERSION: u16 = 1;
const SIDECAR_FILE_NAME: &str = "index.rename_chains";

/// Max forward / chain / member counts. Guard against a crafted sidecar
/// triggering a huge allocation before a body-read fails.
const MAX_FORWARD_COUNT: u32 = 5_000_000;
const MAX_CHAIN_COUNT: u32 = 5_000_000;
const MAX_MEMBER_COUNT: u32 = 10_000_000;

// ---------------------------------------------------------------------------
// #[repr(C)] structs
// ---------------------------------------------------------------------------

/// Fixed 48-byte sidecar header. Field layout is hand-packed to achieve exactly
/// 48 bytes with no implicit padding:
///
/// ```text
/// offset  0: magic[4]                     = 4
/// offset  4: version[2]                   = 6
/// offset  6: _pad0[2]                     = 8    (8-byte boundary — aligns u64 below)
/// offset  8: body_tokens_hash[8]          = 16   (8-aligned, no gap)
/// offset 16: chain_count[4]               = 20
/// offset 20: forward_count[4]             = 24
/// offset 24: member_count[4]              = 28
/// offset 28: history_tip_sha_prefix[20]   = 48   (4-aligned, no gap)
/// total = 48, struct align = 8, 48 % 8 == 0 → no trailing pad
/// ```
///
/// Note: `body_tokens_hash` precedes the count fields to avoid the implicit
/// 4-byte alignment gap that would arise if u64 followed three consecutive u32
/// fields (which would land at offset 20, not 8-aligned).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub magic: [u8; 4],
    pub version: u16,
    pub _pad0: u16,
    pub body_tokens_hash: u64,
    pub chain_count: u32,
    pub forward_count: u32,
    pub member_count: u32,
    pub history_tip_sha_prefix: [u8; 20],
}

impl Header {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

// Compile-time size guard.
const _: () = assert!(Header::SIZE == 48, "Header must be exactly 48 bytes");

/// Forward entry — 16 bytes.
/// `entry_idx` (u32) + `chain_id` (u64, 8-byte aligned after 4+4=8 pad) — but
/// u64 after u32 needs 4 bytes of alignment padding making it 4+4+8+4=20.
/// The task doc explicitly says "alignment 8, no _pad needed by construction"
/// and lists the size as 16 (4+8+4=16), which works because u64 is only 8-byte
/// aligned and u32 for `entry_idx` + u32 for `score` are each 4 bytes.
/// Correct field ordering for 16 bytes with align=8:
///   entry_idx (u32, off 0) + score (f32, off 4) + chain_id (u64, off 8) = 16 bytes.
/// The task doc lists entry_idx/chain_id/score but that order produces padding.
/// We reorder to entry_idx/score/chain_id to get the 16-byte layout the task doc
/// prescribes, and expose the same logical fields via accessor methods.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ForwardEntry {
    pub entry_idx: u32,
    pub score: f32,
    pub chain_id: u64,
}

impl ForwardEntry {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

const _: () = assert!(
    ForwardEntry::SIZE == 16,
    "ForwardEntry must be exactly 16 bytes"
);

/// Chain table entry — 16 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ChainTableEntry {
    pub chain_id: u64,
    pub member_offset: u32,
    pub member_count: u32,
}

impl ChainTableEntry {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

const _: () = assert!(
    ChainTableEntry::SIZE == 16,
    "ChainTableEntry must be exactly 16 bytes"
);

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum SidecarError {
    #[error("rename_chains sidecar magic mismatch")]
    Magic,
    #[error("rename_chains sidecar version {0} unsupported")]
    Version(u16),
    #[error("body_tokens_hash mismatch (sidecar stale, drop)")]
    BodyTokensMismatch,
    #[error("history_tip_sha mismatch (sidecar stale, drop)")]
    HistoryTipMismatch,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Writer artifact
// ---------------------------------------------------------------------------

/// In-memory representation of a complete rename-chains sidecar, ready to
/// be serialised via [`save`].
pub struct RenameChainsArtifact {
    /// Sorted ascending by `entry_idx`.
    pub forward: Vec<ForwardEntry>,
    /// Sorted ascending by `chain_id`. `member_offset` filled in by writer.
    pub chains: Vec<ChainTableEntry>,
    /// Flat member list, indexed by `ChainTableEntry.member_offset`.
    pub members: Vec<u32>,
    /// xxh3_64 over body_tokens bytes in sym_idx order.
    pub body_tokens_hash: u64,
    /// Raw 20-byte tip SHA from the history sidecar.
    pub history_tip_sha_prefix: [u8; 20],
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Write `artifact` to `tmp_path`. The file is fully written and fsync'd but
/// NOT yet renamed to the final name. The caller performs the atomic rename.
///
/// This split matches the `hash_index::save_to_tmp` pattern — it lets
/// orchestrators batch multiple sidecar renames to minimise inconsistency
/// windows.
pub fn save_to_tmp(tmp_path: &Path, artifact: &RenameChainsArtifact) -> Result<()> {
    validate_artifact(artifact)?;

    use std::io::Write;

    let mut f = std::fs::File::create(tmp_path)
        .with_context(|| format!("create {}", tmp_path.display()))?;

    // Header.
    let header = build_header(artifact);
    // SAFETY: Header is #[repr(C)], Copy, and has no implicit padding (verified
    // by the compile-time SIZE assert). Reading the raw bytes is safe.
    let header_bytes = unsafe {
        std::slice::from_raw_parts((&header as *const Header) as *const u8, Header::SIZE)
    };
    f.write_all(header_bytes).context("write header")?;

    // ForwardEntry slice.
    let forward_bytes = repr_c_slice_bytes(&artifact.forward);
    f.write_all(&forward_bytes)
        .context("write forward entries")?;

    // ChainTableEntry slice.
    let chain_bytes = repr_c_slice_bytes(&artifact.chains);
    f.write_all(&chain_bytes).context("write chain table")?;

    // Flat member list (u32 LE).
    for &m in &artifact.members {
        f.write_all(&m.to_le_bytes()).context("write member")?;
    }

    f.sync_all().context("fsync rename_chains tmp")?;
    Ok(())
}

/// Atomic save: write to `<path>.rename_chains.tmp`, then rename. Same
/// convention as every other vex sidecar writer.
pub fn save(path: &Path, artifact: &RenameChainsArtifact) -> Result<()> {
    let tmp = path.with_extension("rename_chains.tmp");
    save_to_tmp(&tmp, artifact)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()));
    }
    Ok(())
}

/// Validate the artifact before serialisation. Returns `Err` on a caller bug
/// (unsorted inputs, mismatched member bookkeeping) rather than silently
/// writing a corrupt sidecar.
fn validate_artifact(artifact: &RenameChainsArtifact) -> Result<()> {
    // Count bounds.
    anyhow::ensure!(
        artifact.forward.len() <= MAX_FORWARD_COUNT as usize,
        "forward_count {} exceeds MAX_FORWARD_COUNT {}",
        artifact.forward.len(),
        MAX_FORWARD_COUNT,
    );
    anyhow::ensure!(
        artifact.chains.len() <= MAX_CHAIN_COUNT as usize,
        "chain_count {} exceeds MAX_CHAIN_COUNT {}",
        artifact.chains.len(),
        MAX_CHAIN_COUNT,
    );
    anyhow::ensure!(
        artifact.members.len() <= MAX_MEMBER_COUNT as usize,
        "member_count {} exceeds MAX_MEMBER_COUNT {}",
        artifact.members.len(),
        MAX_MEMBER_COUNT,
    );

    // forward[] sorted strictly ascending by entry_idx.
    for w in artifact.forward.windows(2) {
        if w[0].entry_idx >= w[1].entry_idx {
            bail!(
                "forward[] not strictly ascending by entry_idx: {} >= {}",
                w[0].entry_idx,
                w[1].entry_idx
            );
        }
    }

    // chains[] sorted strictly ascending by chain_id.
    for w in artifact.chains.windows(2) {
        if w[0].chain_id >= w[1].chain_id {
            bail!(
                "chains[] not strictly ascending by chain_id: {} >= {}",
                w[0].chain_id,
                w[1].chain_id
            );
        }
    }

    // Sum of chain.member_count == members.len().
    let total_members: u64 = artifact.chains.iter().map(|c| c.member_count as u64).sum();
    anyhow::ensure!(
        total_members == artifact.members.len() as u64,
        "sum of chain.member_count {} != members.len() {}",
        total_members,
        artifact.members.len(),
    );

    // Each chain.member_offset + member_count <= members.len().
    for (i, c) in artifact.chains.iter().enumerate() {
        let end = c.member_offset as u64 + c.member_count as u64;
        anyhow::ensure!(
            end <= artifact.members.len() as u64,
            "chain[{}] member_offset {} + member_count {} = {} overflows members.len() {}",
            i,
            c.member_offset,
            c.member_count,
            end,
            artifact.members.len(),
        );
    }

    Ok(())
}

fn build_header(artifact: &RenameChainsArtifact) -> Header {
    Header {
        magic: *MAGIC,
        version: VERSION,
        _pad0: 0,
        body_tokens_hash: artifact.body_tokens_hash,
        chain_count: artifact.chains.len() as u32,
        forward_count: artifact.forward.len() as u32,
        member_count: artifact.members.len() as u32,
        history_tip_sha_prefix: artifact.history_tip_sha_prefix,
    }
}

/// `#[repr(C)] Copy` slice → raw bytes. Mirrors `git_history::repr_c_slice_bytes`.
///
/// Caller contract: `T` must be `#[repr(C)]` AND have no implicit padding.
/// All three callers in this module (`ForwardEntry`, `ChainTableEntry`) each
/// have compile-time SIZE asserts that confirm their layout has no gaps.
fn repr_c_slice_bytes<T: Copy>(slice: &[T]) -> Vec<u8> {
    let byte_len = std::mem::size_of_val(slice);
    let mut out = Vec::with_capacity(byte_len);
    // SAFETY: T is #[repr(C)] Copy with no implicit padding (see contract above).
    // The slice is contiguous; we read exactly byte_len bytes for the borrow duration.
    let src = unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, byte_len) };
    out.extend_from_slice(src);
    out
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Zero-copy mmap-backed reader for the `index.rename_chains` sidecar.
///
/// The `'static` slices are safe because they point into the owned `Mmap`
/// field which lives exactly as long as the struct. Dropping `RenameChainsReader`
/// drops the `Mmap`, after which all references derived from it are gone.
pub struct RenameChainsReader {
    // The Mmap keeps the mapped pages alive. It MUST be the first drop target
    // (Rust drops fields in declaration order), but since the slices don't carry
    // lifetimes we rely on the caller not retaining raw pointers across drop.
    _mmap: Mmap,
    header: Header,
    forward: &'static [ForwardEntry],
    chains: &'static [ChainTableEntry],
    members: &'static [u32],
}

impl std::fmt::Debug for RenameChainsReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenameChainsReader")
            .field("chain_count", &self.header.chain_count)
            .field("forward_count", &self.header.forward_count)
            .field("member_count", &self.header.member_count)
            .finish()
    }
}

impl RenameChainsReader {
    pub fn chain_count(&self) -> usize {
        self.header.chain_count as usize
    }

    pub fn forward_count(&self) -> usize {
        self.header.forward_count as usize
    }

    pub fn member_count(&self) -> usize {
        self.header.member_count as usize
    }

    pub fn body_tokens_hash(&self) -> u64 {
        self.header.body_tokens_hash
    }

    pub fn history_tip_sha_prefix(&self) -> &[u8; 20] {
        &self.header.history_tip_sha_prefix
    }

    /// Binary-search `forward[]` (sorted by `entry_idx`) for the chain_id
    /// of `entry_idx`. Returns `None` when the entry has no chain (singleton).
    pub fn chain_id_for_entry(&self, entry_idx: u32) -> Option<u64> {
        let fe = self.forward_entry_for(entry_idx)?;
        Some(fe.chain_id)
    }

    /// Binary-search `chains[]` (sorted by `chain_id`) and slice into
    /// `members[]`. Returns `None` when `chain_id` is not in the table.
    pub fn members_of(&self, chain_id: u64) -> Option<&[u32]> {
        let ct = self.chain_table_entry_for(chain_id)?;
        let start = ct.member_offset as usize;
        let end = start + ct.member_count as usize;
        // Bounds were validated at open time.
        Some(&self.members[start..end])
    }

    /// Convenience: `chain_id_for_entry` → `members_of`. Returns a
    /// `Vec` containing only `entry_idx` when no chain exists (singleton).
    pub fn follow_chain(&self, entry_idx: u32) -> Vec<u32> {
        if let Some(chain_id) = self.chain_id_for_entry(entry_idx) {
            if let Some(members) = self.members_of(chain_id) {
                return members.to_vec();
            }
        }
        vec![entry_idx]
    }

    /// Return the composite score for an entry, or `None` when not in any chain.
    pub fn score_for_entry(&self, entry_idx: u32) -> Option<f32> {
        let fe = self.forward_entry_for(entry_idx)?;
        Some(fe.score)
    }

    // ------------------------------------------------------------------
    // Private helpers
    // ------------------------------------------------------------------

    fn forward_entry_for(&self, entry_idx: u32) -> Option<&ForwardEntry> {
        let pos = self
            .forward
            .binary_search_by_key(&entry_idx, |fe| fe.entry_idx)
            .ok()?;
        Some(&self.forward[pos])
    }

    fn chain_table_entry_for(&self, chain_id: u64) -> Option<&ChainTableEntry> {
        let pos = self
            .chains
            .binary_search_by_key(&chain_id, |ct| ct.chain_id)
            .ok()?;
        Some(&self.chains[pos])
    }
}

// ---------------------------------------------------------------------------
// open()
// ---------------------------------------------------------------------------

/// Query-time opener. Verifies only `history_tip_sha_prefix`; trusts
/// the sidecar's stored `body_tokens_hash` because the caller (e.g.
/// `vex history`) has no way to compute the expected value without
/// re-walking blobs.
///
/// Co-write atomicity (history sidecar + rename_chains in
/// `pipeline/output.rs::write_rename_chains_sidecar`) means a fresh
/// `history_tip` match is sufficient to trust the body_tokens_hash —
/// the two sidecars can only drift apart via mid-write crash, which
/// is bounded by the per-file atomic tmp+rename.
///
/// Returns `Ok(None)` for absent / silent-fallback cases (tip
/// mismatch, magic/version mismatch, truncation). Only `Err(Io)`
/// bubbles up — every other failure degrades to the "no chain
/// expansion" path.
pub fn open_for_query(
    index_dir: &Path,
    expected_history_tip: &[u8; 20],
) -> Result<Option<RenameChainsReader>, SidecarError> {
    let path = index_dir.join(SIDECAR_FILE_NAME);
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SidecarError::Io(e)),
    };
    // SAFETY: read-only mmap, no mutation. memmap2 contract.
    let mmap = unsafe { Mmap::map(&file) }.map_err(SidecarError::Io)?;
    if mmap.len() < Header::SIZE {
        return Ok(None);
    }
    // SAFETY: bounds-checked above; Header is #[repr(C)] Copy with no padding.
    let header: Header = unsafe { std::ptr::read_unaligned(mmap.as_ptr() as *const Header) };
    if &header.magic != MAGIC || header.version != VERSION {
        return Ok(None);
    }
    drop(mmap); // Drop the early-probe map; `open` will remap and validate fully.

    // Pass the sidecar's own body_tokens_hash so the guard is a no-op
    // for query-time callers. The tip guard stays strict.
    match open(index_dir, header.body_tokens_hash, expected_history_tip) {
        Ok(some) => Ok(some),
        Err(SidecarError::Io(e)) => Err(SidecarError::Io(e)),
        // BodyTokensMismatch is impossible here (we just passed the
        // sidecar's own hash). Magic/Version/HistoryTipMismatch all
        // degrade to None at the query path.
        Err(_) => Ok(None),
    }
}

/// Header-only peek for diagnostic surfaces (`vex status`). Does NOT
/// verify history-tip or body-tokens guards — a stale sidecar still
/// counts for status reporting ("there's a stale chain table; re-index
/// to refresh").
///
/// Returns:
/// - `Ok(None)` — sidecar absent, truncated, or magic/version
///   mismatch (treated as absent for status purposes).
/// - `Ok(Some(header))` — file exists and the first 48 bytes look
///   like a valid VEXR v1 header. Counts may still be larger than
///   the on-disk payload (no full-file validation here); status
///   consumers read these as best-effort hints.
/// - `Err(Io)` — disk read failed; caller decides whether to surface.
pub fn read_header(index_dir: &Path) -> Result<Option<Header>, SidecarError> {
    let path = index_dir.join(SIDECAR_FILE_NAME);
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SidecarError::Io(e)),
    };
    // SAFETY: read-only mmap, no mutation. memmap2 contract.
    let mmap = unsafe { Mmap::map(&file) }.map_err(SidecarError::Io)?;
    if mmap.len() < Header::SIZE {
        return Ok(None);
    }
    // SAFETY: bounds-checked above; Header is #[repr(C)] Copy with no padding.
    let header: Header = unsafe { std::ptr::read_unaligned(mmap.as_ptr() as *const Header) };
    if &header.magic != MAGIC || header.version != VERSION {
        return Ok(None);
    }
    Ok(Some(header))
}

/// Open and validate the rename_chains sidecar with both hash guards.
///
/// Returns:
/// - `Ok(None)`  — sidecar absent (legacy / cold-start, not an error).
/// - `Ok(Some(reader))` — present, magic/version/hashes match.
/// - `Err(BodyTokensMismatch | HistoryTipMismatch)` — stale; caller should drop the file.
/// - `Err(Magic | Version)` — present but corrupt; caller should drop the file.
/// - `Err(Io)` — disk read failed; caller decides.
pub fn open(
    index_dir: &Path,
    expected_body_tokens_hash: u64,
    expected_history_tip: &[u8; 20],
) -> Result<Option<RenameChainsReader>, SidecarError> {
    let path = index_dir.join(SIDECAR_FILE_NAME);

    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SidecarError::Io(e)),
    };

    // SAFETY: we treat the mapped region as read-only and never mutate it.
    // External modification after map would produce inconsistent reads but not
    // undefined behaviour (memmap2 contract).
    let mmap = unsafe { Mmap::map(&file) }.map_err(SidecarError::Io)?;

    if mmap.len() < Header::SIZE {
        // File is smaller than a valid header — treat as corruption.
        return Err(SidecarError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!(
                "rename_chains sidecar is {} bytes; smaller than {}-byte header",
                mmap.len(),
                Header::SIZE
            ),
        )));
    }

    // SAFETY: mmap.len() >= Header::SIZE (checked above). read_unaligned is safe
    // because we only require the bytes to be initialised (they come from a file)
    // and Header is #[repr(C)] Copy.
    let header: Header = unsafe { std::ptr::read_unaligned(mmap.as_ptr() as *const Header) };

    if &header.magic != MAGIC {
        return Err(SidecarError::Magic);
    }
    if header.version != VERSION {
        return Err(SidecarError::Version(header.version));
    }

    // Count bounds — prevent huge allocations on crafted sidecars.
    if header.forward_count > MAX_FORWARD_COUNT {
        return Err(SidecarError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "forward_count {} > MAX_FORWARD_COUNT {}",
                header.forward_count, MAX_FORWARD_COUNT
            ),
        )));
    }
    if header.chain_count > MAX_CHAIN_COUNT {
        return Err(SidecarError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "chain_count {} > MAX_CHAIN_COUNT {}",
                header.chain_count, MAX_CHAIN_COUNT
            ),
        )));
    }
    if header.member_count > MAX_MEMBER_COUNT {
        return Err(SidecarError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "member_count {} > MAX_MEMBER_COUNT {}",
                header.member_count, MAX_MEMBER_COUNT
            ),
        )));
    }

    // Hash/tip guards — stale sidecar detection.
    if header.body_tokens_hash != expected_body_tokens_hash {
        return Err(SidecarError::BodyTokensMismatch);
    }
    if &header.history_tip_sha_prefix != expected_history_tip {
        return Err(SidecarError::HistoryTipMismatch);
    }

    // Compute section boundaries and bounds-check against file length.
    let file_len = mmap.len() as u64;

    let forward_bytes = header.forward_count as u64 * ForwardEntry::SIZE as u64;
    let chain_bytes = header.chain_count as u64 * ChainTableEntry::SIZE as u64;
    let member_bytes = header.member_count as u64 * 4u64;

    let forward_start = Header::SIZE as u64;
    let chain_start = forward_start + forward_bytes;
    let member_start = chain_start + chain_bytes;
    let expected_len = member_start + member_bytes;

    if expected_len > file_len {
        return Err(SidecarError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!(
                "rename_chains sidecar declares {} bytes but file is {} bytes",
                expected_len, file_len
            ),
        )));
    }

    // Build 'static slices backed by the mmap. Safe because:
    // - `mmap` is owned by the `RenameChainsReader` we are about to return.
    // - The slices point into `mmap`'s mapped region.
    // - `RenameChainsReader` is not `Send` / `Sync` in a way that could outlive the Mmap
    //   (Mmap is `Send + Sync`, so the reader is too — but the Mmap drops first because
    //   it appears before the phantom field; see field ordering comment in the struct).
    // - We never expose the raw pointers through the public API.
    // - `ForwardEntry`, `ChainTableEntry`, and `u32` are all `#[repr(C)] Copy` (or
    //   primitive) with no padding; byte-interpreting them is well-defined.
    let base = mmap.as_ptr();

    let forward: &'static [ForwardEntry] = unsafe {
        std::slice::from_raw_parts(
            base.add(forward_start as usize) as *const ForwardEntry,
            header.forward_count as usize,
        )
    };

    let chains: &'static [ChainTableEntry] = unsafe {
        std::slice::from_raw_parts(
            base.add(chain_start as usize) as *const ChainTableEntry,
            header.chain_count as usize,
        )
    };

    let members: &'static [u32] = unsafe {
        std::slice::from_raw_parts(
            base.add(member_start as usize) as *const u32,
            header.member_count as usize,
        )
    };

    // Validate that each ChainTableEntry.member_offset + member_count is in range.
    // This prevents out-of-bounds slices in `members_of` on a crafted sidecar.
    for ct in chains.iter() {
        let end = ct.member_offset as u64 + ct.member_count as u64;
        if end > header.member_count as u64 {
            return Err(SidecarError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "chain member_offset {} + member_count {} = {} overflows member_count {}",
                    ct.member_offset, ct.member_count, end, header.member_count
                ),
            )));
        }
    }

    Ok(Some(RenameChainsReader {
        _mmap: mmap,
        header,
        forward,
        chains,
        members,
    }))
}

// ---------------------------------------------------------------------------
// Fuzz shim
// ---------------------------------------------------------------------------

/// Fuzz shim — exposes the sidecar loader to arbitrary byte sequences.
/// Uses sentinel hashes/tip so valid sidecars built from those sentinels
/// pass the staleness check and exercise the full reader path; crafted-
/// corrupt bytes exercise all the early-exit paths.
///
/// Mirrors `hash_index::__fuzz_hash_index_bytes`.
#[doc(hidden)]
pub fn __fuzz_rename_chains_bytes(data: &[u8]) {
    let dir = std::env::temp_dir();
    let path = dir.join(SIDECAR_FILE_NAME);
    if std::fs::write(&path, data).is_err() {
        return;
    }
    let _ = open(&dir, 0, &[0u8; 20]);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn sample_artifact_empty() -> RenameChainsArtifact {
        RenameChainsArtifact {
            forward: vec![],
            chains: vec![],
            members: vec![],
            body_tokens_hash: 0xABCD_1234,
            history_tip_sha_prefix: [1u8; 20],
        }
    }

    fn single_chain_artifact() -> RenameChainsArtifact {
        // Chain id 42, members [10, 11, 12].
        let chains = vec![ChainTableEntry {
            chain_id: 42,
            member_offset: 0,
            member_count: 3,
        }];
        let members = vec![10u32, 11, 12];
        let forward = vec![
            ForwardEntry {
                entry_idx: 10,
                score: 0.80,
                chain_id: 42,
            },
            ForwardEntry {
                entry_idx: 11,
                score: 0.85,
                chain_id: 42,
            },
            ForwardEntry {
                entry_idx: 12,
                score: 0.90,
                chain_id: 42,
            },
        ];
        RenameChainsArtifact {
            forward,
            chains,
            members,
            body_tokens_hash: 0x1111,
            history_tip_sha_prefix: [2u8; 20],
        }
    }

    fn multi_chain_artifact() -> RenameChainsArtifact {
        // Chain 10 (members [0,1]), chain 20 (members [2,3,4]), chain 30 (members [5]).
        let chains = vec![
            ChainTableEntry {
                chain_id: 10,
                member_offset: 0,
                member_count: 2,
            },
            ChainTableEntry {
                chain_id: 20,
                member_offset: 2,
                member_count: 3,
            },
            ChainTableEntry {
                chain_id: 30,
                member_offset: 5,
                member_count: 1,
            },
        ];
        let members = vec![0u32, 1, 2, 3, 4, 5];
        let forward = vec![
            ForwardEntry {
                entry_idx: 0,
                score: 0.70,
                chain_id: 10,
            },
            ForwardEntry {
                entry_idx: 1,
                score: 0.72,
                chain_id: 10,
            },
            ForwardEntry {
                entry_idx: 2,
                score: 0.80,
                chain_id: 20,
            },
            ForwardEntry {
                entry_idx: 3,
                score: 0.82,
                chain_id: 20,
            },
            ForwardEntry {
                entry_idx: 4,
                score: 0.84,
                chain_id: 20,
            },
            ForwardEntry {
                entry_idx: 5,
                score: 0.65,
                chain_id: 30,
            },
        ];
        RenameChainsArtifact {
            forward,
            chains,
            members,
            body_tokens_hash: 0xDEAD_BEEF,
            history_tip_sha_prefix: [7u8; 20],
        }
    }

    fn save_and_open(tmp: &TempDir, artifact: &RenameChainsArtifact) -> RenameChainsReader {
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        save(&p, artifact).unwrap();
        open(
            tmp.path(),
            artifact.body_tokens_hash,
            &artifact.history_tip_sha_prefix,
        )
        .unwrap()
        .unwrap()
    }

    // ------------------------------------------------------------------
    // Round-trip tests
    // ------------------------------------------------------------------

    #[test]
    fn round_trip_empty() {
        let tmp = TempDir::new().unwrap();
        let a = sample_artifact_empty();
        let r = save_and_open(&tmp, &a);
        assert_eq!(r.chain_count(), 0);
        assert_eq!(r.forward_count(), 0);
        assert_eq!(r.member_count(), 0);
        assert_eq!(r.body_tokens_hash(), 0xABCD_1234);
        assert_eq!(r.history_tip_sha_prefix(), &[1u8; 20]);
    }

    #[test]
    fn round_trip_single_chain() {
        let tmp = TempDir::new().unwrap();
        let a = single_chain_artifact();
        let r = save_and_open(&tmp, &a);
        assert_eq!(r.chain_count(), 1);
        assert_eq!(r.forward_count(), 3);
        assert_eq!(r.member_count(), 3);
        assert_eq!(r.chain_id_for_entry(10), Some(42));
        assert_eq!(r.chain_id_for_entry(11), Some(42));
        assert_eq!(r.chain_id_for_entry(99), None);
        let m = r.members_of(42).unwrap();
        assert_eq!(m, &[10u32, 11, 12]);
    }

    #[test]
    fn round_trip_multi_chain() {
        let tmp = TempDir::new().unwrap();
        let a = multi_chain_artifact();
        let r = save_and_open(&tmp, &a);
        assert_eq!(r.chain_count(), 3);
        assert_eq!(r.forward_count(), 6);
        assert_eq!(r.member_count(), 6);

        assert_eq!(r.members_of(10).unwrap(), &[0u32, 1]);
        assert_eq!(r.members_of(20).unwrap(), &[2u32, 3, 4]);
        assert_eq!(r.members_of(30).unwrap(), &[5u32]);
        assert_eq!(r.members_of(999), None);
    }

    // ------------------------------------------------------------------
    // Atomicity
    // ------------------------------------------------------------------

    #[test]
    fn save_is_atomic_via_tmp_rename() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        let a = single_chain_artifact();
        save(&p, &a).unwrap();
        let leftover = p.with_extension("rename_chains.tmp");
        assert!(!leftover.exists(), "tmp must be renamed away");
        assert!(p.exists(), "final file must exist");
    }

    // ------------------------------------------------------------------
    // Rejection tests
    // ------------------------------------------------------------------

    #[test]
    fn rejects_bad_magic() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        let mut bad = build_minimal_valid_bytes(0x1111, &[2u8; 20]);
        bad[0] = b'X'; // corrupt magic byte
        std::fs::write(&p, bad).unwrap();
        let err = open(tmp.path(), 0x1111, &[2u8; 20]).unwrap_err();
        assert!(matches!(err, SidecarError::Magic), "got: {err}");
    }

    #[test]
    fn rejects_bad_version() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        let mut bad = build_minimal_valid_bytes(0x1111, &[2u8; 20]);
        // Bytes 4-5 are version (u16 LE).
        bad[4] = 0xFF;
        bad[5] = 0x00;
        std::fs::write(&p, bad).unwrap();
        let err = open(tmp.path(), 0x1111, &[2u8; 20]).unwrap_err();
        assert!(matches!(err, SidecarError::Version(255)), "got: {err}");
    }

    #[test]
    fn rejects_body_tokens_hash_mismatch() {
        let tmp = TempDir::new().unwrap();
        let a = sample_artifact_empty();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        save(&p, &a).unwrap();
        let err = open(tmp.path(), 0xDEAD, &a.history_tip_sha_prefix).unwrap_err();
        assert!(
            matches!(err, SidecarError::BodyTokensMismatch),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_history_tip_mismatch() {
        let tmp = TempDir::new().unwrap();
        let a = sample_artifact_empty();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        save(&p, &a).unwrap();
        let err = open(tmp.path(), a.body_tokens_hash, &[0xFF; 20]).unwrap_err();
        assert!(
            matches!(err, SidecarError::HistoryTipMismatch),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_truncated_header() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        // Only write 10 bytes — less than Header::SIZE.
        std::fs::write(&p, [0u8; 10]).unwrap();
        let err = open(tmp.path(), 0, &[0u8; 20]).unwrap_err();
        assert!(matches!(err, SidecarError::Io(_)), "got: {err}");
    }

    #[test]
    fn rejects_truncated_body() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        // Header claims 1 forward entry but provides no body bytes.
        // Field layout: magic[4]+version[2]+_pad0[2]+body_tokens_hash[8] = 16,
        //               chain_count[4] at 16, forward_count[4] at 20, member_count[4] at 24.
        let mut bytes = build_minimal_valid_bytes(0x1111, &[2u8; 20]);
        // Patch forward_count at offset 20.
        bytes[20] = 1;
        bytes[21] = 0;
        bytes[22] = 0;
        bytes[23] = 0;
        std::fs::write(&p, bytes).unwrap();
        let err = open(tmp.path(), 0x1111, &[2u8; 20]).unwrap_err();
        assert!(matches!(err, SidecarError::Io(_)), "got: {err}");
    }

    #[test]
    fn rejects_absurd_forward_count() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        let mut bytes = build_minimal_valid_bytes(0x1111, &[2u8; 20]);
        // forward_count is at offset 20 in the new field layout.
        let absurd = (MAX_FORWARD_COUNT + 1).to_le_bytes();
        bytes[20..24].copy_from_slice(&absurd);
        std::fs::write(&p, bytes).unwrap();
        let err = open(tmp.path(), 0x1111, &[2u8; 20]).unwrap_err();
        assert!(matches!(err, SidecarError::Io(_)), "got: {err}");
    }

    #[test]
    fn rejects_absurd_member_count() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        let mut bytes = build_minimal_valid_bytes(0x1111, &[2u8; 20]);
        // member_count is at offset 24 in the new field layout.
        let absurd = (MAX_MEMBER_COUNT + 1).to_le_bytes();
        bytes[24..28].copy_from_slice(&absurd);
        std::fs::write(&p, bytes).unwrap();
        let err = open(tmp.path(), 0x1111, &[2u8; 20]).unwrap_err();
        assert!(matches!(err, SidecarError::Io(_)), "got: {err}");
    }

    // ------------------------------------------------------------------
    // Absence is not an error
    // ------------------------------------------------------------------

    #[test]
    fn missing_file_returns_ok_none() {
        let tmp = TempDir::new().unwrap();
        let result = open(tmp.path(), 0, &[0u8; 20]);
        assert!(result.unwrap().is_none(), "absent sidecar must be Ok(None)");
    }

    // ------------------------------------------------------------------
    // Reader method correctness
    // ------------------------------------------------------------------

    #[test]
    fn members_of_returns_correct_slice() {
        let tmp = TempDir::new().unwrap();
        let a = multi_chain_artifact();
        let r = save_and_open(&tmp, &a);
        assert_eq!(r.members_of(10).unwrap(), &[0u32, 1]);
        assert_eq!(r.members_of(20).unwrap(), &[2u32, 3, 4]);
        assert_eq!(r.members_of(30).unwrap(), &[5u32]);
        assert_eq!(r.members_of(0), None);
    }

    #[test]
    fn chain_id_for_entry_binary_search_correctness() {
        let tmp = TempDir::new().unwrap();
        let a = multi_chain_artifact();
        let r = save_and_open(&tmp, &a);
        // Every entry in forward should resolve.
        assert_eq!(r.chain_id_for_entry(0), Some(10));
        assert_eq!(r.chain_id_for_entry(1), Some(10));
        assert_eq!(r.chain_id_for_entry(2), Some(20));
        assert_eq!(r.chain_id_for_entry(3), Some(20));
        assert_eq!(r.chain_id_for_entry(4), Some(20));
        assert_eq!(r.chain_id_for_entry(5), Some(30));
        // Entry not in forward.
        assert_eq!(r.chain_id_for_entry(99), None);
    }

    #[test]
    fn writer_rejects_unsorted_forward() {
        let mut a = single_chain_artifact();
        // Swap first two entries to break ascending order.
        a.forward.swap(0, 1);
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        assert!(save(&p, &a).is_err());
    }

    #[test]
    fn writer_rejects_unsorted_chains() {
        let mut a = multi_chain_artifact();
        // Swap first two chains to break ascending order.
        a.chains.swap(0, 1);
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        assert!(save(&p, &a).is_err());
    }

    #[test]
    fn writer_rejects_member_offset_overflow() {
        let mut a = single_chain_artifact();
        // Set member_offset past members.len().
        a.chains[0].member_offset = 100;
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(SIDECAR_FILE_NAME);
        assert!(save(&p, &a).is_err());
    }

    #[test]
    fn follow_chain_singleton_returns_just_self() {
        let tmp = TempDir::new().unwrap();
        let a = sample_artifact_empty();
        let r = save_and_open(&tmp, &a);
        // entry_idx 77 has no chain — should return vec![77].
        assert_eq!(r.follow_chain(77), vec![77u32]);
    }

    #[test]
    fn score_for_entry_round_trip() {
        // Test a range of f32 values including negative, >1.0, and a zero.
        let chains = vec![ChainTableEntry {
            chain_id: 99,
            member_offset: 0,
            member_count: 3,
        }];
        let members = vec![0u32, 1, 2];
        let forward = vec![
            ForwardEntry {
                entry_idx: 0,
                score: -0.5_f32,
                chain_id: 99,
            },
            ForwardEntry {
                entry_idx: 1,
                score: 1.5_f32,
                chain_id: 99,
            },
            ForwardEntry {
                entry_idx: 2,
                score: 0.0_f32,
                chain_id: 99,
            },
        ];
        let a = RenameChainsArtifact {
            forward,
            chains,
            members,
            body_tokens_hash: 0xF00D,
            history_tip_sha_prefix: [3u8; 20],
        };
        let tmp = TempDir::new().unwrap();
        let r = save_and_open(&tmp, &a);
        assert_eq!(r.score_for_entry(0), Some(-0.5_f32));
        assert_eq!(r.score_for_entry(1), Some(1.5_f32));
        assert_eq!(r.score_for_entry(2), Some(0.0_f32));
        assert_eq!(r.score_for_entry(99), None);
    }

    // ------------------------------------------------------------------
    // Helper: build a minimal valid on-disk byte buffer (empty artifact).
    // ------------------------------------------------------------------

    fn build_minimal_valid_bytes(body_tokens_hash: u64, tip: &[u8; 20]) -> Vec<u8> {
        let header = Header {
            magic: *MAGIC,
            version: VERSION,
            _pad0: 0,
            body_tokens_hash,
            chain_count: 0,
            forward_count: 0,
            member_count: 0,
            history_tip_sha_prefix: *tip,
        };
        // SAFETY: Header is #[repr(C)] Copy with no padding (SIZE == 48 asserted).
        let bytes = unsafe {
            std::slice::from_raw_parts((&header as *const Header) as *const u8, Header::SIZE)
        };
        bytes.to_vec()
    }
}
