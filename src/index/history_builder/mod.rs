//! v1.17+ Phase 14.8 — persistent historical symbol index.
//!
//! Source-of-truth path for `vex history <Symbol>` when a
//! `git_history` section is present in the v6 store. The v1.16
//! query-time walker in [`crate::history`] stays as the fallback
//! whenever the section is absent or stale.
//!
//! ## Module status: DESIGN LOCK + SCAFFOLD (Step 1)
//!
//! This module currently exposes type definitions and public
//! function signatures only. Every function body is
//! `unimplemented!()`. Implementation lands across the
//! Steps 2-10 roadmap in
//! [`.claude/Task/PHASE14.8-history-index.md`](../../../.claude/Task/PHASE14.8-history-index.md).
//!
//! Anything that depends on this module (CLI flags, reader API,
//! query-path switching in `crate::history`) is **NOT** wired
//! yet — calling `build_history_section` from any other module
//! would `unimplemented!()`-panic at runtime. The scaffold exists
//! to lock the contract on disk so subsequent implementation
//! sessions have a stable target.
//!
//! ## Design summary
//!
//! - Walk git history from `tip` (HEAD or `--branch`) up to `depth`.
//! - Enumerate every `(commit, file_path, blob_sha)` triple via
//!   `git log --raw --no-renames --pretty=format:%H` (architect
//!   review C2 — the original `rev-list --objects` plan dedupes
//!   blobs **across** commits and loses per-commit attribution).
//! - Dedup by blob SHA. Each unique blob parses **once** — Phase
//!   14.7's blob cache turns most parses into a cache lookup.
//! - For each unique blob's symbols, emit one `HistoryEntry` with
//!   first-seen / last-seen commit spans. A blob that lives
//!   unchanged across 500 consecutive commits is one entry, not
//!   500.
//! - Materialise into the `git_history` section of `index.vex`
//!   (28-byte fixed records for entries, 32-byte for commits,
//!   24-byte for blobs, plus a name → `Vec<entry_idx>` posting
//!   list).
//!
//! See the design doc for the exact binary layout, builder
//! pseudo-code, reader API, manifest extensions, CLI surface,
//! and `vex status` text.
//!
//! ## Why this lives alongside [`crate::index::parse_cache`]
//!
//! 14.8 is "14.7's blob cache projected through git history". The
//! parse_cache module already owns the blob-SHA → `ParsedFile`
//! mapping; this module's builder consumes that cache as its
//! parse fast-path and writes the projected symbol-blob index.
//! Future maintainers extending the parse cache (new `SymbolKind`,
//! new section in `ParsedFile`) need to bump both
//! [`crate::index::parse_cache::CACHE_FORMAT_VERSION`] *and* the
//! `git_history` section version constant — `docs/RELEASING.md`
//! documents this.

// Module-level `dead_code` allow because every type and function
// declared here is intentionally unused until Step 4b lands. Lifted
// when the builder gets wired into the pipeline. The alternative —
// per-item `#[allow]` — would litter every public field with a
// review-cycle-only annotation; the module-level form is reverted
// in one place when implementation begins.
#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::Result;

/// Persistent on-disk version of the `git_history` section body.
/// Bumped when [`HistoryEntry`] / [`Commit`] / [`Blob`] layout
/// changes. Note the **two-tier version contract** the design doc
/// pins (architect review C1):
/// - **Store version** (the v6→v7 bump): controls whether the
///   `git_history_offset` field exists in the header chain at all.
///   Old binaries refuse v7 indexes via the existing
///   `MIN_SUPPORTED_VERSION..=VERSION` check in
///   `src/store/reader.rs:43-56`.
/// - **`HISTORY_SECTION_VERSION`** (this constant): controls the
///   layout of the section body. Bumping this alone is for
///   layout changes that keep the v7 header — the writer emits
///   the new version, old 14.8-era readers see the version
///   mismatch and treat the section as absent (graceful fallback
///   to the query-time walker).
///
/// `SymbolKind` discriminant changes require bumping this AND
/// `parse_cache::CACHE_FORMAT_VERSION` together — same contract
/// as Phase 14.7 (architect review H4).
pub const HISTORY_SECTION_VERSION: u16 = 1;

/// Magic identifying the start of a `git_history` section body
/// inside `index.vex`. **`b"VXGH"` (Vex Git History)** — distinct
/// from the v1.14.1 B1.1 `index.hashes` sidecar's `b"VEXH"` magic.
/// Architect review L2 flagged the original collision: while
/// disambiguated *by file location*, future debugging tools that
/// grep raw bytes for vex magic IDs would conflate the two and
/// produce confusing reports. A distinct magic keeps the raw-byte
/// search story clean.
pub const HISTORY_SECTION_MAGIC: &[u8; 4] = b"VXGH";

/// One historical occurrence of a symbol — the on-disk row.
/// Fixed 28-byte layout so the section is mmap-friendly and
/// `entries[i]` is O(1) without a separate index.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Index into the section's `blobs` array.
    pub blob_idx: u32,
    /// Offset into the index's existing string table — repo-relative
    /// POSIX file path at the commit span this entry covers.
    pub file_offset: u32,
    /// 1-based line within the blob.
    pub line: u32,
    /// Offset into the string table for the symbol's signature line.
    /// 0 = no signature (parser couldn't extract one).
    pub signature_offset: u32,
    /// Index into the section's `commits` array — first commit
    /// where this `(blob, symbol)` was observed during the walk
    /// (oldest, since the walk processes newest→oldest).
    pub first_commit_idx: u32,
    /// Index into `commits` — newest commit where the same
    /// `(blob, symbol)` was observed. Equal to `first_commit_idx`
    /// when the blob appears in exactly one commit.
    pub last_commit_idx: u32,
    /// `SymbolKind` discriminant; identical to the byte stored
    /// in the existing `SymbolRecord::kind`.
    pub kind: u8,
    pub _pad: [u8; 3],
}

impl HistoryEntry {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// On-disk commit metadata. 32 bytes fixed.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    /// Raw 20-byte SHA-1.
    pub sha: [u8; 20],
    /// Commit timestamp expressed as unix epoch seconds (good
    /// until 2106). Architect review L1: the original draft used
    /// `u32 unix_days` to save 4 bytes per record — but two
    /// commits on the same day had ambiguous ordering and would
    /// have required a SHA-tiebreaker every query. 4 bytes × 500
    /// commits = 2 KB extra is cheap vs query-time complexity.
    pub date_unix_seconds: u32,
    /// Offset into the section-local `history_strings` sub-section
    /// for the author name. NOT the global StringTable — keeping
    /// author names isolated prevents FST contamination of the
    /// symbol-name lookup path (architect review M1). Author email
    /// is intentionally not stored — privacy + GDPR concern raised
    /// before the schema lock.
    pub author_offset: u32,
    pub _pad: [u8; 4],
}

impl Commit {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// On-disk blob entry. 24 bytes fixed.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blob {
    /// Raw 20-byte SHA-1.
    pub sha: [u8; 20],
    pub _pad: [u8; 4],
}

impl Blob {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

// Compile-time size guards (rust-reviewer NIT #1). A future field
// reorder or type change that breaks the documented on-disk layout
// fails at `cargo check`, not at runtime when a v7 index suddenly
// reads garbage. Keep these matched to the design doc's section
// body layout.
const _: () = assert!(
    HistoryEntry::SIZE == 28,
    "HistoryEntry must be exactly 28 bytes"
);
const _: () = assert!(Commit::SIZE == 32, "Commit must be exactly 32 bytes");
const _: () = assert!(Blob::SIZE == 24, "Blob must be exactly 24 bytes");

/// Inputs to [`build_history_section`].
#[derive(Debug, Clone)]
pub struct BuildConfig {
    /// Repository root. Must be inside a git worktree.
    pub repo_root: PathBuf,
    /// Tip ref the walk starts from. `HEAD` by default;
    /// `--branch <X>` plumbs a non-default ref through.
    pub tip: String,
    /// Max commits to walk. `None` = unbounded.
    pub depth: Option<usize>,
}

/// Output of [`build_history_section`]. The writer in
/// `crate::store::git_history` flattens this into the on-disk
/// layout described in the design doc.
#[derive(Debug, Clone)]
pub struct HistorySection {
    pub entries: Vec<HistoryEntry>,
    pub commits: Vec<Commit>,
    pub blobs: Vec<Blob>,
    /// `symbol_name_offset → Vec<entry_idx>` posting list — **build-time
    /// representation only**. The writer in `crate::store::git_history`
    /// projects this into the on-disk shape: a `fst::Map` keyed by symbol
    /// name bytes pointing to (postings_offset, postings_count) tuples,
    /// plus a packed `[u32]` array of entry indices. Architect review H2:
    /// the original "bincode-serialized HashMap on disk" plan broke the
    /// mmap-zero-copy invariant that every other vex section preserves
    /// (and was non-deterministic across runs due to HashMap iteration
    /// order). Keys here point into the index's existing string table;
    /// values point into `self.entries`.
    pub symbol_postings: std::collections::HashMap<u32, Vec<u32>>,
    /// Set when the depth cap stopped the walk before reaching
    /// the root commit. Surfaced via the section header's
    /// `flags` bit 0 so `vex status` can warn.
    pub was_depth_capped: bool,
}

/// Build a [`HistorySection`] over `cfg.tip..cfg.depth`. Used
/// once per `vex index --history` invocation and once per
/// `vex update --history` invocation (incremental update walks
/// only `Manifest::history_indexed_at..cfg.tip`, with the same
/// dedup contract).
///
/// **Not implemented yet** — Step 4b on the
/// [`Phase 14.8 roadmap`](../../../.claude/Task/PHASE14.8-history-index.md).
#[allow(unused_variables, dead_code)]
pub fn build_history_section(cfg: &BuildConfig) -> Result<HistorySection> {
    unimplemented!(
        "Phase 14.8 Step 4b — see .claude/Task/PHASE14.8-history-index.md \
         for the builder algorithm. Today this function exists so the \
         scaffold compiles; runtime invocation panics deliberately."
    )
}

/// Incremental update — walk only `range_start..cfg.tip` and
/// merge the new entries into an existing [`HistorySection`].
///
/// **Not implemented yet** — Step 5 on the roadmap.
#[allow(unused_variables, dead_code)]
pub fn update_history_section(
    existing: HistorySection,
    cfg: &BuildConfig,
    range_start: &str,
) -> Result<HistorySection> {
    unimplemented!(
        "Phase 14.8 Step 5 — incremental update. Walks only \
         <range_start>..<cfg.tip>. Body lands once Step 4b is green."
    )
}

// Note: rust-reviewer SHOULD-FIX #11 was applied during Step 1 by
// promoting `crate::history::ensure_git_worktree` to `pub(crate)`.
// Step 4b will call it directly instead of duplicating the shellout.
// No stub here — the previously-planned local copy is gone.
