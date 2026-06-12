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

// Module-level `dead_code` allow scoped to the still-unused
// scaffold pieces (Step 5 `update_history_section` + the locked
// `symbol_postings` field that's only consumed by the future
// inline-section writer). Step 4b implementation lifts the rest.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command as StdCommand, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use crate::index::parse_cache::BlobCache;
use crate::parse::language::Language;
use crate::parse::parse_file;
use crate::store::git_history::StringTable;
use crate::util::config;

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
    ///
    /// **Step 4a note**: when the writer takes the sidecar path
    /// (`crate::store::git_history::write_sidecar`) the FST is built
    /// from the per-entry `entry_names: &[String]` argument instead
    /// of this map — `symbol_postings` stays unused until the future
    /// inline-section promotion lands and we want a single source of
    /// truth shared with `HistorySection::merge`.
    pub symbol_postings: std::collections::HashMap<u32, Vec<u32>>,
    /// Private strings sub-section (architect M1, expanded from
    /// author-only to all history strings: author names + file paths +
    /// signature lines). Layout: packed sequence of
    /// `[u32 byte_len][UTF-8 bytes]`, indexed by byte offset. Offset 0
    /// is reserved as the empty-string sentinel. `HistoryEntry`'s
    /// `file_offset`/`signature_offset` and `Commit::author_offset`
    /// all point in here.
    pub strings: Vec<u8>,
    /// Set when the depth cap stopped the walk before reaching
    /// the root commit. Surfaced via the section header's
    /// `flags` bit 0 so `vex status` can warn.
    pub was_depth_capped: bool,
    /// **Phase 14.10 build-time-only.** Body-token strings keyed by
    /// `entry_idx` (parallel to `entries`); `None` if the parser did
    /// not extract a body for the symbol. Consumed by
    /// `crate::index::rename_chains::build_rename_chains` to produce
    /// the rename-chains sidecar.
    ///
    /// **Not persisted to disk.** The `git_history` sidecar has no
    /// space for body tokens (HistoryEntry is fixed 28 B with no slack)
    /// and adding a body_tokens-by-entry sidecar would double indexing
    /// time on history-heavy repos. The merge path therefore pads prior
    /// (loaded-from-disk) entries with `None`, which means the
    /// rename-chain builder cannot detect renames that cross a
    /// `vex update --history` incremental-merge boundary. A full
    /// rebuild restores full coverage.
    pub entry_body_tokens: Vec<Option<String>>,
    /// **Phase 14.10 build-time-only.** Signature-token strings keyed
    /// by `entry_idx`, same semantics as `entry_body_tokens`. We carry
    /// these separately rather than re-reading the strings table at
    /// chain-build time because the chain builder needs the raw bytes
    /// (whitespace-split) rather than the `signature_offset` u32.
    pub entry_sig_tokens: Vec<Option<String>>,
}

/// Build a [`HistorySection`] over `cfg.tip..cfg.depth`. Used
/// once per `vex index --history` invocation and once per
/// `vex update --history` invocation (incremental update walks
/// only `Manifest::history_indexed_at..cfg.tip`, with the same
/// dedup contract).
///
/// Algorithm:
///   1. Shell `git log --raw --no-renames --no-merges -nDEPTH tip`
///      with `--pretty=format:"COMMIT %H|%ct|%an"` to enumerate
///      every `(commit, path, blob_sha)` triple plus per-commit
///      metadata (unix-seconds timestamp + author name).
///   2. Assign stable indices: commits sorted chronologically
///      (oldest→newest, so `first_commit_idx <= last_commit_idx`
///      always); blob SHAs in first-seen order; paths interned
///      into the private string table.
///   3. For each unique `(blob_idx, path)` pair, parse the blob
///      via `git cat-file --batch` + `parse_file` through the
///      14.7 blob cache. Cache hits skip the parse.
///   4. Emit one `HistoryEntry` per (parsed_symbol, blob, path)
///      with the commit span derived from the triples that
///      touched that pair.
///
/// Architect-locked decisions honoured: dedup by blob SHA
/// (architect C2), convex-hull spans (architect H1), private
/// string table (architect M1 expanded scope), global depth cap
/// (architect M3).
pub fn build_history_section(cfg: &BuildConfig) -> Result<HistorySection> {
    // Phase 14.8 review pass (both reviewers MUST-FIX / SHOULD-FIX):
    // delegate to the writer-aware variant and discard the names. The
    // returned section is INTENTIONALLY missing FST-ready name data
    // (it would require the writer to walk symbol_postings, which is
    // empty — the sidecar writer in `crate::store::git_history` takes
    // names via the paired `entry_names` argument instead). This
    // function exists so the future inline-section writer has a
    // names-discarded entrypoint matching its data flow; today
    // (sidecar-only) it is dead code that the build still emits to
    // keep the contract aligned with the locked design doc.
    let (section, _entry_names_dropped) = build_history_section_with_names(cfg)?;
    Ok(section)
}

/// Variant of [`build_history_section`] that also returns the
/// per-entry symbol names needed by the sidecar writer. The plain
/// `build_history_section` discards names because the in-memory
/// `HistorySection` shape doesn't carry them — the inline-section
/// writer (future) projects names through the global StringTable
/// instead.
pub fn build_history_section_with_names(
    cfg: &BuildConfig,
) -> Result<(HistorySection, Vec<String>)> {
    build_with_range(cfg, &cfg.tip)
}

/// Phase 14.8 Step 5c — build a "delta" section over the commit
/// range `<prior_tip>..<cfg.tip>`. Walked only over commits new
/// since the prior index run; the caller is responsible for merging
/// the result into an existing [`HistorySection`] via
/// [`merge_history_sections`].
///
/// Different from [`build_history_section_with_names`] only in the
/// git log range argument; the rest of the algorithm (dedup by blob
/// SHA, chronological commit-idx assignment, parse via 14.7 cache,
/// private strings table) is identical so the merged result has the
/// same invariants as a from-scratch full build.
pub fn build_history_section_for_range(
    cfg: &BuildConfig,
    prior_tip: &str,
) -> Result<(HistorySection, Vec<String>)> {
    let range = format!("{prior_tip}..{}", cfg.tip);
    build_with_range(cfg, &range)
}

fn build_with_range(cfg: &BuildConfig, git_log_arg: &str) -> Result<(HistorySection, Vec<String>)> {
    crate::history::ensure_git_worktree(&cfg.repo_root)?;

    let triples = enumerate_git_log(&cfg.repo_root, git_log_arg, cfg.depth)?;
    if triples.commits_ordered.is_empty() {
        return Ok((empty_section(), Vec::new()));
    }

    let mut commits_meta: Vec<CommitMeta> = triples.commits_ordered.clone();
    commits_meta.reverse();
    let mut commit_sha_to_idx: HashMap<[u8; 20], u32> = HashMap::with_capacity(commits_meta.len());
    for (i, m) in commits_meta.iter().enumerate() {
        commit_sha_to_idx.insert(m.sha, i as u32);
    }

    let mut spans: HashMap<([u8; 20], String), CommitSpan> = HashMap::new();
    let mut blob_to_idx: HashMap<[u8; 20], u32> = HashMap::new();
    let mut blob_order: Vec<[u8; 20]> = Vec::new();
    let mut chrono_triples: Vec<&RawTriple> = triples.triples.iter().collect();
    chrono_triples.sort_by_key(|t| {
        commit_sha_to_idx
            .get(&t.commit_sha)
            .copied()
            .unwrap_or(u32::MAX)
    });
    for t in &chrono_triples {
        let commit_idx = match commit_sha_to_idx.get(&t.commit_sha) {
            Some(&idx) => idx,
            None => continue,
        };
        if let std::collections::hash_map::Entry::Vacant(e) = blob_to_idx.entry(t.blob_sha) {
            e.insert(blob_order.len() as u32);
            blob_order.push(t.blob_sha);
        }
        let key = (t.blob_sha, t.path.clone());
        spans
            .entry(key)
            .and_modify(|s| {
                if commit_idx < s.first {
                    s.first = commit_idx;
                }
                if commit_idx > s.last {
                    s.last = commit_idx;
                }
            })
            .or_insert(CommitSpan {
                first: commit_idx,
                last: commit_idx,
            });
    }

    let cache = BlobCache::new(config::blob_cache_dir());
    let mut batch = CatFileBatch::spawn(&cfg.repo_root)?;
    let mut strings = StringTable::new();

    let mut commits: Vec<Commit> = Vec::with_capacity(commits_meta.len());
    for m in &commits_meta {
        let author_offset = strings.intern(&m.author);
        commits.push(Commit {
            sha: m.sha,
            date_unix_seconds: m.unix_seconds,
            author_offset,
            _pad: [0; 4],
        });
    }

    let mut entries: Vec<HistoryEntry> = Vec::new();
    let mut entry_names: Vec<String> = Vec::new();
    let mut entry_body_tokens: Vec<Option<String>> = Vec::new();
    let mut entry_sig_tokens: Vec<Option<String>> = Vec::new();

    for ((blob_sha, path), span) in spans.iter() {
        let lang = match std::path::Path::new(path)
            .extension()
            .and_then(|s| s.to_str())
            .and_then(Language::from_extension)
        {
            Some(l) => l,
            None => continue,
        };

        let blob_idx = blob_to_idx[blob_sha];
        let blob_hex = hex_sha(blob_sha);

        let parsed = match cache.lookup(&blob_hex, lang) {
            Some(pf) => pf,
            None => {
                let bytes = match batch.read(&blob_hex) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let content = match std::str::from_utf8(&bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let pf = match parse_file(path, content, lang) {
                    Ok(pf) => pf,
                    Err(_) => continue,
                };
                let _ = cache.insert(&blob_hex, lang, &pf);
                pf
            }
        };

        let file_offset = strings.intern(path);
        for sym in &parsed.symbols {
            let sig = sym.signature.as_deref().unwrap_or("");
            let signature_offset = strings.intern(sig);
            entries.push(HistoryEntry {
                blob_idx,
                file_offset,
                line: sym.line as u32,
                signature_offset,
                first_commit_idx: span.first,
                last_commit_idx: span.last,
                kind: sym.kind as u8,
                _pad: [0; 3],
            });
            entry_names.push(sym.name.clone());
            // Phase 14.10: capture per-entry token strings for the
            // rename-chains sidecar. The parser already produced these
            // — discarding them at this site is the only reason a
            // future Phase 14.10 caller would have to re-parse blobs.
            entry_body_tokens.push(sym.body_tokens.clone());
            entry_sig_tokens.push(sym.signature.clone());
        }
    }
    drop(batch);

    let blobs: Vec<Blob> = blob_order
        .iter()
        .map(|sha| Blob {
            sha: *sha,
            _pad: [0; 4],
        })
        .collect();

    let was_depth_capped = match cfg.depth {
        Some(cap) => commits.len() >= cap && cap > 0,
        None => false,
    };

    Ok((
        HistorySection {
            entries,
            commits,
            blobs,
            symbol_postings: HashMap::new(),
            strings: strings.as_bytes().to_vec(),
            was_depth_capped,
            entry_body_tokens,
            entry_sig_tokens,
        },
        entry_names,
    ))
}

/// Phase 14.8 Step 5c — merge a delta section (from
/// [`build_history_section_for_range`]) into a prior section
/// (extracted via [`crate::store::git_history::HistoryReader::extract_owned`]).
/// Returns the union with all indices reassigned to the merged
/// space.
///
/// Merge rules:
///   - **Commits**: concat prior + delta. Delta is by construction
///     disjoint from prior (walked `<prior_tip>..<tip>`), so no
///     dedup needed. Delta commit indices shift by `prior.commits.len()`.
///   - **Blobs**: union by SHA. Delta blobs already in prior reuse
///     the prior index; new blobs append. Delta entries' `blob_idx`
///     is remapped through the merged blob table.
///   - **Strings**: concat raw bytes. Delta string offsets shift by
///     `prior.strings.len()`. The duplicate "empty string sentinel"
///     at offset prior.strings.len() is harmless (4 wasted bytes).
///   - **Entries**: delta entries get shifted commit indices +
///     remapped blob indices + shifted string offsets, then appended.
///   - **was_depth_capped**: OR of the two (incremental update
///     inherits the cap state).
pub fn merge_history_sections(
    prior: HistorySection,
    prior_names: Vec<String>,
    delta: HistorySection,
    delta_names: Vec<String>,
) -> (HistorySection, Vec<String>) {
    let prior_commit_count = prior.commits.len() as u32;
    let prior_strings_len = prior.strings.len() as u32;

    // 1. Commits: concat.
    let mut commits = prior.commits;
    for mut c in delta.commits {
        c.author_offset = c.author_offset.saturating_add(prior_strings_len);
        commits.push(c);
    }

    // 2. Blobs: union by SHA.
    let mut blobs = prior.blobs;
    let mut blob_sha_to_idx: HashMap<[u8; 20], u32> = HashMap::with_capacity(blobs.len());
    for (i, b) in blobs.iter().enumerate() {
        blob_sha_to_idx.insert(b.sha, i as u32);
    }
    // Build delta blob_idx → merged blob_idx remap.
    let mut delta_blob_remap: Vec<u32> = Vec::with_capacity(delta.blobs.len());
    for db in &delta.blobs {
        match blob_sha_to_idx.get(&db.sha) {
            Some(&existing_idx) => delta_blob_remap.push(existing_idx),
            None => {
                let new_idx = blobs.len() as u32;
                blob_sha_to_idx.insert(db.sha, new_idx);
                blobs.push(*db);
                delta_blob_remap.push(new_idx);
            }
        }
    }

    // 3. Strings: concat raw bytes. Delta offsets shift unconditionally.
    let mut strings = prior.strings;
    strings.extend_from_slice(&delta.strings);

    // 4. Entries: shift commit indices, remap blob indices, shift
    //    string offsets. Append to prior entries.
    let prior_entry_count = prior.entries.len();
    let delta_entry_count = delta.entries.len();
    let mut entries = prior.entries;
    let mut names = prior_names;
    entries.reserve(delta_entry_count);
    names.reserve(delta_names.len());
    for (mut e, name) in delta.entries.into_iter().zip(delta_names) {
        e.blob_idx = delta_blob_remap
            .get(e.blob_idx as usize)
            .copied()
            .unwrap_or(e.blob_idx); // defensive — should always hit
        e.first_commit_idx = e.first_commit_idx.saturating_add(prior_commit_count);
        e.last_commit_idx = e.last_commit_idx.saturating_add(prior_commit_count);
        e.file_offset = e.file_offset.saturating_add(prior_strings_len);
        e.signature_offset = e.signature_offset.saturating_add(prior_strings_len);
        entries.push(e);
        names.push(name);
    }

    // 5. Phase 14.10: entry_body_tokens / entry_sig_tokens. Prior
    //    side comes from disk; `extract_owned` now pads with `None`
    //    to the entry count so the pass-through branch in
    //    `pad_or_pass` handles it. The else branch is a defensive
    //    fallback for any future caller that hands in a
    //    partially-populated vec — `debug_assert` surfaces the
    //    drift in dev. Documented limitation: chains across the
    //    merge boundary are not detected until the next full
    //    rebuild (prior body tokens are all `None`).
    debug_assert_eq!(delta.entry_body_tokens.len(), delta_entry_count);
    let mut entry_body_tokens = pad_or_pass(prior.entry_body_tokens, prior_entry_count);
    entry_body_tokens.extend(delta.entry_body_tokens);
    let mut entry_sig_tokens = pad_or_pass(prior.entry_sig_tokens, prior_entry_count);
    entry_sig_tokens.extend(delta.entry_sig_tokens);

    let merged = HistorySection {
        entries,
        commits,
        blobs,
        symbol_postings: HashMap::new(),
        strings,
        was_depth_capped: prior.was_depth_capped || delta.was_depth_capped,
        entry_body_tokens,
        entry_sig_tokens,
    };
    (merged, names)
}

/// Helper for [`merge_history_sections`]: pass `tokens` through when
/// it matches `expected`, otherwise replace with a `None`-filled vec
/// of the right length.
///
/// In normal flow both `extract_owned` (disk path, pads with `None`)
/// and `build_with_range` (in-memory path, populates from parser) emit
/// length-matching vecs — so the else branch is dead code today and
/// the `debug_assert!` surfaces any future caller bug that hands in
/// a partially-populated input. The fallback is kept (rather than
/// `assert!`/panic) so a release-mode merge against a malformed prior
/// degrades to "no chain coverage on prior side" rather than aborting
/// `vex update --history`.
fn pad_or_pass(tokens: Vec<Option<String>>, expected: usize) -> Vec<Option<String>> {
    debug_assert!(
        tokens.is_empty() || tokens.len() == expected,
        "pad_or_pass: partially-populated token vec ({} of {})",
        tokens.len(),
        expected,
    );
    if tokens.len() == expected {
        tokens
    } else {
        vec![None; expected]
    }
}

fn empty_section() -> HistorySection {
    HistorySection {
        entries: Vec::new(),
        commits: Vec::new(),
        blobs: Vec::new(),
        symbol_postings: HashMap::new(),
        strings: Vec::new(),
        was_depth_capped: false,
        entry_body_tokens: Vec::new(),
        entry_sig_tokens: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// git log enumeration + cat-file batch
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CommitMeta {
    sha: [u8; 20],
    unix_seconds: u32,
    author: String,
}

#[derive(Debug)]
struct RawTriple {
    commit_sha: [u8; 20],
    path: String,
    blob_sha: [u8; 20],
}

#[derive(Default)]
struct GitLogResult {
    /// Commits in git-log encounter order (newest first). Reverse
    /// for chronological (oldest first) — the order indices are
    /// assigned in for first/last spans. NOT a HashMap because
    /// second-resolution unix timestamps tie often enough on
    /// rapid synthetic commits that sort-by-time degenerates to
    /// sort-by-SHA, scrambling commit_idx.
    commits_ordered: Vec<CommitMeta>,
    seen_shas: std::collections::HashSet<[u8; 20]>,
    triples: Vec<RawTriple>,
}

impl GitLogResult {
    fn push_commit(&mut self, m: CommitMeta) {
        if self.seen_shas.insert(m.sha) {
            self.commits_ordered.push(m);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CommitSpan {
    first: u32,
    last: u32,
}

fn enumerate_git_log(
    repo: &std::path::Path,
    tip: &str,
    depth: Option<usize>,
) -> Result<GitLogResult> {
    let mut argv: Vec<String> = vec![
        "log".into(),
        "--raw".into(),
        // Critical: git defaults to abbreviating blob SHAs in --raw
        // (typically to 7 chars). Without --no-abbrev, decode_sha20
        // rejects every triple as malformed and the section comes
        // out empty.
        "--no-abbrev".into(),
        "--no-renames".into(),
        "--no-merges".into(),
        "--pretty=format:COMMIT %H|%ct|%an".into(),
    ];
    if let Some(n) = depth {
        argv.push(format!("-n{n}"));
    }
    argv.push(tip.to_string());

    let out = StdCommand::new("git")
        .current_dir(repo)
        .args(&argv)
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn git log in {}", repo.display()))?;
    if !out.status.success() {
        bail!(
            "git log failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let mut result = GitLogResult::default();
    let mut current_sha = [0u8; 20];
    let mut have_current = false;
    for line in out.stdout.split(|&b| b == b'\n') {
        let line = std::str::from_utf8(line).unwrap_or("");
        if let Some(rest) = line.strip_prefix("COMMIT ") {
            let mut parts = rest.splitn(3, '|');
            let sha_hex = parts.next().unwrap_or("");
            let ts_str = parts.next().unwrap_or("");
            let author = parts.next().unwrap_or("").to_string();
            let sha = match decode_sha20(sha_hex) {
                Some(s) => s,
                None => {
                    have_current = false;
                    continue;
                }
            };
            let ts: u32 = ts_str.parse().unwrap_or(0);
            current_sha = sha;
            have_current = true;
            result.push_commit(CommitMeta {
                sha,
                unix_seconds: ts,
                author,
            });
            continue;
        }
        if !have_current || !line.starts_with(':') {
            continue;
        }
        let mut tab = line.splitn(2, '\t');
        let meta = tab.next().unwrap_or("");
        let path = tab.next().unwrap_or("");
        if path.is_empty() {
            continue;
        }
        let toks: Vec<&str> = meta.split_whitespace().collect();
        if toks.len() < 5 {
            continue;
        }
        let status = toks[4];
        if status == "D" {
            continue;
        }
        let new_sha_hex = toks[3];
        if new_sha_hex.bytes().all(|b| b == b'0') {
            continue;
        }
        let blob_sha = match decode_sha20(new_sha_hex) {
            Some(s) => s,
            None => continue,
        };
        result.triples.push(RawTriple {
            commit_sha: current_sha,
            path: path.to_string(),
            blob_sha,
        });
    }
    Ok(result)
}

fn decode_sha20(hex_str: &str) -> Option<[u8; 20]> {
    if hex_str.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for i in 0..20 {
        let byte = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16).ok()?;
        out[i] = byte;
    }
    Some(out)
}

fn hex_sha(bytes: &[u8; 20]) -> String {
    let mut s = String::with_capacity(40);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Long-lived `git cat-file --batch` driven by stdin/stdout pipes.
/// Reuses the deadlock-safe pattern from `examples/phase148_bench.rs`:
/// read THROUGH the BufReader on both header line and blob bytes
/// (`get_mut()` bypass would skip buffered bytes); kill child before
/// wait on Drop to avoid stdin-still-open deadlock.
struct CatFileBatch {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl CatFileBatch {
    fn spawn(repo: &std::path::Path) -> Result<Self> {
        let mut child = StdCommand::new("git")
            .current_dir(repo)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn git cat-file --batch")?;
        let stdin = child.stdin.take().context("take stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("take stdout")?);
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn read(&mut self, sha_hex: &str) -> Result<Vec<u8>> {
        writeln!(self.stdin, "{sha_hex}")?;
        self.stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        let header = header.trim_end();
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() == 2 && parts[1] == "missing" {
            return Err(anyhow!("blob missing: {sha_hex}"));
        }
        if parts.len() != 3 {
            return Err(anyhow!("unexpected cat-file header: {header}"));
        }
        let size: usize = parts[2].parse().context("parse blob size")?;
        let mut buf = vec![0u8; size];
        self.stdout.read_exact(&mut buf)?;
        let mut nl = [0u8; 1];
        self.stdout.read_exact(&mut nl)?;
        Ok(buf)
    }
}

impl Drop for CatFileBatch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn git(repo: &std::path::Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {:?}", args);
    }

    fn init_repo(dir: &std::path::Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "Tester"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn commit_file(repo: &std::path::Path, rel: &str, content: &str, msg: &str) {
        let abs = repo.join(rel);
        if let Some(p) = abs.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&abs, content).unwrap();
        git(repo, &["add", rel]);
        git(repo, &["commit", "-q", "-m", msg]);
    }

    fn delete_committed(repo: &std::path::Path, rel: &str, msg: &str) {
        git(repo, &["rm", "-q", rel]);
        git(repo, &["commit", "-q", "-m", msg]);
    }

    #[test]
    fn build_picks_up_two_symbols_in_one_blob() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file(
            repo,
            "src/lib.rs",
            "pub fn alpha() -> u8 { 1 }\npub fn beta() -> u8 { 2 }\n",
            "c1",
        );

        let cfg = BuildConfig {
            repo_root: repo.to_path_buf(),
            tip: "HEAD".to_string(),
            depth: None,
        };
        let (section, names) = build_history_section_with_names(&cfg).unwrap();

        assert_eq!(section.commits.len(), 1);
        assert_eq!(section.blobs.len(), 1);
        assert!(
            section.entries.len() >= 2,
            "expected >=2 symbols, got {}: {:?}",
            section.entries.len(),
            names
        );
        assert!(names.iter().any(|n| n == "alpha"));
        assert!(names.iter().any(|n| n == "beta"));
        assert!(!section.was_depth_capped);
    }

    #[test]
    fn build_finds_symbol_in_deleted_file() {
        // The NEW capability from the design: a symbol whose name no
        // longer appears at HEAD must still be in the section. The
        // walker can't find it via `git grep HEAD`; the indexer
        // walks history directly.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file(
            repo,
            "src/lib.rs",
            "pub fn doomed_helper() -> u8 { 7 }\npub fn live() {}\n",
            "c1: doomed_helper exists",
        );
        commit_file(
            repo,
            "src/lib.rs",
            "pub fn live() {}\n",
            "c2: doomed_helper removed",
        );

        let cfg = BuildConfig {
            repo_root: repo.to_path_buf(),
            tip: "HEAD".to_string(),
            depth: None,
        };
        let (_section, names) = build_history_section_with_names(&cfg).unwrap();

        assert!(
            names.iter().any(|n| n == "doomed_helper"),
            "section must contain `doomed_helper` even though it's \
             absent at HEAD. Names: {:?}",
            names
        );
    }

    #[test]
    fn build_emits_two_entries_for_two_blobs_same_symbol() {
        // Two different blobs of the same file → two HistoryEntry
        // rows for the same symbol name, each with its own
        // first_commit_idx == last_commit_idx (the blob appears in
        // exactly one commit each via `git log --raw`, the typical
        // case). Pins that the builder does NOT collapse different
        // blobs into one entry just because the symbol name matches.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file(repo, "src/lib.rs", "pub fn s() -> u8 { 1 }\n", "c1");
        commit_file(repo, "src/lib.rs", "pub fn s() -> u32 { 2 }\n", "c2");

        let cfg = BuildConfig {
            repo_root: repo.to_path_buf(),
            tip: "HEAD".to_string(),
            depth: None,
        };
        let (section, names) = build_history_section_with_names(&cfg).unwrap();

        let s_idxs: Vec<usize> = names
            .iter()
            .enumerate()
            .filter_map(|(i, n)| (n == "s").then_some(i))
            .collect();
        assert_eq!(
            s_idxs.len(),
            2,
            "expected 2 entries for `s` (one per distinct blob), got {}: names={:?}",
            s_idxs.len(),
            names
        );

        // Each entry's span must be a single commit (first == last)
        // because each blob appears in exactly one commit. The two
        // entries must point at DIFFERENT blobs.
        let e0 = section.entries[s_idxs[0]];
        let e1 = section.entries[s_idxs[1]];
        assert_eq!(
            e0.first_commit_idx, e0.last_commit_idx,
            "blob in one commit → first==last; got first={} last={}",
            e0.first_commit_idx, e0.last_commit_idx
        );
        assert_eq!(e1.first_commit_idx, e1.last_commit_idx);
        assert_ne!(
            e0.blob_idx, e1.blob_idx,
            "two entries must point at different blobs"
        );
    }

    #[test]
    fn build_blob_revert_dedups_via_span() {
        // Architect-locked dedup contract: when blob X appears at
        // commits A and C (with a different blob at B in between —
        // a revert / cherry-pick), the section emits ONE entry per
        // symbol of blob X with first=A, last=C (convex hull span,
        // architect H1 accepted lossy approximation).
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file(repo, "src/lib.rs", "pub fn s() -> u8 { 1 }\n", "c1: first");
        commit_file(
            repo,
            "src/lib.rs",
            "pub fn s() -> u8 { 99 }\n",
            "c2: change",
        );
        // c3: revert back to c1's exact content — same blob SHA as c1
        commit_file(repo, "src/lib.rs", "pub fn s() -> u8 { 1 }\n", "c3: revert");

        let cfg = BuildConfig {
            repo_root: repo.to_path_buf(),
            tip: "HEAD".to_string(),
            depth: None,
        };
        let (section, names) = build_history_section_with_names(&cfg).unwrap();

        // Two unique blobs (c1==c3 blob, c2 blob). Two entries for
        // `s` (one per distinct blob).
        assert_eq!(section.blobs.len(), 2);
        let s_idxs: Vec<usize> = names
            .iter()
            .enumerate()
            .filter_map(|(i, n)| (n == "s").then_some(i))
            .collect();
        assert_eq!(s_idxs.len(), 2);

        // Find the entry whose blob span goes from c1 to c3 (the
        // revert case): first < last and difference > 0.
        let revert_entry = section
            .entries
            .iter()
            .find(|e| e.last_commit_idx > e.first_commit_idx)
            .expect("expected one entry whose span covers the revert");
        assert!(
            revert_entry.last_commit_idx - revert_entry.first_commit_idx >= 2,
            "span should jump from c1 to c3 (skipping c2's different blob); \
             got first={} last={}",
            revert_entry.first_commit_idx,
            revert_entry.last_commit_idx
        );
    }

    #[test]
    fn build_with_depth_cap_records_flag() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        for i in 0..3 {
            commit_file(
                repo,
                "src/lib.rs",
                &format!("pub fn v{i}() {{}}\n"),
                &format!("c{i}"),
            );
        }

        let cfg = BuildConfig {
            repo_root: repo.to_path_buf(),
            tip: "HEAD".to_string(),
            depth: Some(2),
        };
        let (section, _names) = build_history_section_with_names(&cfg).unwrap();

        assert_eq!(section.commits.len(), 2);
        assert!(section.was_depth_capped);
    }

    #[test]
    fn build_on_empty_repo_returns_empty_section() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        // No commits.
        let cfg = BuildConfig {
            repo_root: repo.to_path_buf(),
            tip: "HEAD".to_string(),
            depth: None,
        };
        // git log on an unborn HEAD exits non-zero — surfaces as an
        // error. Verify we propagate cleanly without panicking.
        let err = build_history_section_with_names(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("git log failed"),
            "expected git log failure, got: {err}"
        );
    }

    #[test]
    fn build_skips_non_supported_extensions() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        // .xyz extension has no Language::from_extension mapping.
        commit_file(repo, "README.xyz", "irrelevant\n", "c1");
        commit_file(repo, "src/lib.rs", "pub fn hello() {}\n", "c2");

        let cfg = BuildConfig {
            repo_root: repo.to_path_buf(),
            tip: "HEAD".to_string(),
            depth: None,
        };
        let (_section, names) = build_history_section_with_names(&cfg).unwrap();
        // Only hello (from src/lib.rs) should show.
        assert!(names.iter().any(|n| n == "hello"));
    }

    // Drop-deadlock smoke test (mirrors the fix logged in
    // examples/phase148_bench.rs Step 2 retro). If CatFileBatch's
    // Drop blocks on child.wait without first killing the child,
    // this test hangs indefinitely. With the kill-then-wait fix it
    // returns immediately.
    #[test]
    fn cat_file_batch_drop_does_not_deadlock() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file(repo, "x.rs", "pub fn x() {}\n", "c1");

        // Spawn and immediately drop — exercises Drop without using
        // the read() path.
        let _batch = CatFileBatch::spawn(repo).unwrap();
        drop(_batch);
        // If we get here, Drop returned.
    }
}

// Note: rust-reviewer SHOULD-FIX #11 was applied during Step 1 by
// promoting `crate::history::ensure_git_worktree` to `pub(crate)`.
// Step 4b will call it directly instead of duplicating the shellout.
// No stub here — the previously-planned local copy is gone.
//
// `update_history_section()` stub (Step 5 incremental walker)
// previously lived here but was removed in the Step 9 review pass —
// the dead `unimplemented!()` body was outliving its usefulness
// (clippy "items after test module" rejection + rust-reviewer dead-
// code flag). The function will be re-added in a real follow-up
// phase that implements the linear-history range walk.
