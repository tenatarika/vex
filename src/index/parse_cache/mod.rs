//! Blob-SHA addressed parse cache (Phase 14.7).
//!
//! Each entry stores a serialized [`ParsedFile`] keyed by git blob SHA,
//! sharded by the first two hex characters:
//!
//! ```text
//! <root>/blobs/<sha[0..2]>/<sha>.bin
//! ```
//!
//! ## On-disk format
//!
//! ```text
//! offset  size  field
//!      0     4  magic = b"VXBC"
//!      4     2  CACHE_FORMAT_VERSION: u16 (little-endian)
//!      6     4  grammar_fingerprint: u32 (little-endian)
//!     10     *  bincode-1.3 payload of ParsedFile
//! ```
//!
//! ## Invalidation
//!
//! Lookup rejects entries whose `CACHE_FORMAT_VERSION` or `grammar_fingerprint`
//! do not match the current values. Stale entries are overwritten lazily on the
//! next `insert` for the same SHA; `evict_to_cap` cleans up excess entries.
//!
//! ## Concurrency
//!
//! No file locking is used. The cache is content-addressed: concurrent writes
//! of the same SHA produce identical bytes, so last-writer-wins is safe.
//! `insert` writes to a per-PID temporary file and renames atomically.
//!
pub mod git_blobs;

use std::fs;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::index::symbols::ParsedFile;
use crate::parse::language::Language;
use crate::store::pattern_skeletons::grammar_fingerprint_for_lang;

/// Magic bytes at the start of every cache file.
pub const MAGIC: &[u8; 4] = b"VXBC";

/// Bump this whenever `ParsedFile` (or any type it embeds) changes shape.
/// Stale entries with a different version are silently ignored.
pub const CACHE_FORMAT_VERSION: u16 = 1;

/// On-disk header size: 4 (magic) + 2 (version) + 4 (fingerprint).
const HEADER_SIZE: usize = 10;

/// Step 7-opt — encode `(header || bincode(pf))` for `lang` into a single
/// buffer. Splitting this out of [`BlobCache::insert`] lets the parse pipeline
/// run the CPU-bound serialize on its parallel parse workers while a single
/// background drain thread does only the filesystem writes.
///
/// `pub(crate)` because only the pipeline needs it; the public `insert` API
/// continues to be the canonical "serialize + write" entry point for tests
/// and any future callers.
pub(crate) fn encode_entry(lang: Language, pf: &ParsedFile) -> bincode::Result<Vec<u8>> {
    let payload = bincode::serialize(pf)?;
    let fingerprint = grammar_fingerprint_for_lang(lang);
    let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&CACHE_FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&fingerprint.to_le_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Content-addressed on-disk parse cache.
///
/// Construct with an explicit root path; there is no process-global state.
/// Each test can use its own [`tempfile::TempDir`] root without coordination.
pub struct BlobCache {
    root: PathBuf,
}

impl BlobCache {
    /// Create a cache instance rooted at `root`. The directory need not exist
    /// yet; it is created lazily by [`insert`].
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Look up `sha` for `lang`. Returns `None` on any miss or validation
    /// failure (missing file, bad magic, wrong version, wrong fingerprint,
    /// decode error). Never deletes or modifies files.
    pub fn lookup(&self, sha: &str, lang: Language) -> Option<ParsedFile> {
        let path = self.entry_path(sha);
        let data = fs::read(&path).ok()?;

        if data.len() < HEADER_SIZE {
            return None;
        }

        // Validate magic.
        if &data[0..4] != MAGIC {
            return None;
        }

        // Validate version.
        let version = u16::from_le_bytes([data[4], data[5]]);
        if version != CACHE_FORMAT_VERSION {
            return None;
        }

        // Validate grammar fingerprint.
        let stored_fp = u32::from_le_bytes([data[6], data[7], data[8], data[9]]);
        let expected_fp = grammar_fingerprint_for_lang(lang);
        if stored_fp != expected_fp {
            return None;
        }

        // Decode payload.
        bincode::deserialize::<ParsedFile>(&data[HEADER_SIZE..]).ok()
    }

    /// Serialize `pf` and write it to the cache entry for `sha` / `lang`.
    ///
    /// Uses an atomic rename (write to `<sha>.bin.tmp.<pid>`, then rename onto
    /// `<sha>.bin`) so concurrent writers do not observe partial files.
    ///
    /// The production pipeline does not call this directly — it uses
    /// [`encode_entry`] on parse workers + [`Self::write_entry_bytes`] on a
    /// single drain thread so serialize stays parallel and only the syscalls
    /// serialize. `insert` remains the canonical "serialize + write" entry
    /// point for tests and any future direct-write callers.
    #[allow(dead_code)]
    pub fn insert(&self, sha: &str, lang: Language, pf: &ParsedFile) -> anyhow::Result<()> {
        let buf = encode_entry(lang, pf)
            .with_context(|| format!("failed to serialize ParsedFile for sha {sha}"))?;
        self.write_entry_bytes(sha, &buf)
    }

    /// Phase 14.7 Step 7-opt — write a fully-encoded entry blob (header +
    /// bincode payload) for `sha`. The blob MUST have been produced by
    /// [`encode_entry`] so the magic / version / grammar fingerprint header
    /// is correct. This is the "I/O only" half of [`insert`], split out so
    /// the parse pipeline can serialize on the parse worker (parallelized
    /// CPU work) and only forward the bytes to a single drain thread that
    /// does the two filesystem syscalls. Public API of [`insert`] is
    /// unchanged.
    pub(crate) fn write_entry_bytes(&self, sha: &str, buf: &[u8]) -> anyhow::Result<()> {
        let entry_path = self.entry_path(sha);
        let shard_dir = entry_path
            .parent()
            .expect("entry path always has a parent shard directory");

        fs::create_dir_all(shard_dir).with_context(|| {
            format!("failed to create cache shard dir: {}", shard_dir.display())
        })?;

        // Write to a temp file, then atomically rename onto the final path.
        let tmp_path = shard_dir.join(format!("{sha}.bin.tmp.{}", std::process::id()));
        fs::write(&tmp_path, buf)
            .with_context(|| format!("failed to write temp cache file: {}", tmp_path.display()))?;

        fs::rename(&tmp_path, &entry_path).with_context(|| {
            format!(
                "failed to rename {} → {}",
                tmp_path.display(),
                entry_path.display()
            )
        })?;

        Ok(())
    }

    /// Evict the oldest cache entries (by mtime) until the total size of all
    /// entries in the blobs directory is ≤ `cap_bytes`. If the total is already
    /// within the cap, this is a no-op.
    ///
    /// Per-file errors (e.g. concurrent deletion by another process) are logged
    /// and skipped; the function always returns `Ok(())`.
    pub fn evict_to_cap(&self, cap_bytes: u64) -> anyhow::Result<()> {
        if !self.root.exists() {
            return Ok(());
        }

        // Collect (path, size, mtime) for every .bin file under <root>/<shard>/.
        let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();

        let shard_iter = match fs::read_dir(&self.root) {
            Ok(it) => it,
            Err(e) => {
                tracing::warn!("evict_to_cap: failed to read blobs dir: {e}");
                return Ok(());
            }
        };

        for shard_entry in shard_iter {
            let shard_entry = match shard_entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("evict_to_cap: failed to read shard dir entry: {e}");
                    continue;
                }
            };

            let shard_path = shard_entry.path();
            if !shard_path.is_dir() {
                continue;
            }

            let file_iter = match fs::read_dir(&shard_path) {
                Ok(it) => it,
                Err(e) => {
                    tracing::warn!(
                        "evict_to_cap: failed to read shard {}: {e}",
                        shard_path.display()
                    );
                    continue;
                }
            };

            for file_entry in file_iter {
                let file_entry = match file_entry {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("evict_to_cap: failed to read file entry: {e}");
                        continue;
                    }
                };

                let file_path = file_entry.path();
                let meta = match fs::metadata(&file_path) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("evict_to_cap: failed to stat {}: {e}", file_path.display());
                        continue;
                    }
                };

                let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                entries.push((file_path, meta.len(), mtime));
            }
        }

        // Check whether eviction is needed.
        let total: u64 = entries.iter().map(|(_, sz, _)| sz).sum();
        if total <= cap_bytes {
            return Ok(());
        }

        // Sort oldest-first by mtime (ascending).
        entries.sort_by_key(|(_, _, mtime)| *mtime);

        // Delete oldest-first until the total fits the cap. The cap is a soft
        // budget — a single oversized entry that alone exceeds the cap will
        // still be evicted, which is the intent (over-budget caches must
        // shrink, even at the cost of a single re-parse on the next miss).
        let mut remaining = total;
        for (path, size, _) in entries {
            if remaining <= cap_bytes {
                break;
            }
            if let Err(e) = fs::remove_file(&path) {
                tracing::warn!("evict_to_cap: failed to remove {}: {e}", path.display());
                continue;
            }
            remaining = remaining.saturating_sub(size);
        }

        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Build the path `<root>/<sha[0..2]>/<sha>.bin`.
    ///
    /// `root` is the value passed to [`BlobCache::new`]; callers (notably
    /// [`crate::util::config::blob_cache_dir`]) own the directory naming. Avoid
    /// hardcoding extra path segments here so that a configured `<root>` is
    /// the actual root rather than an accidental grandparent.
    fn entry_path(&self, sha: &str) -> PathBuf {
        debug_assert!(
            sha.len() >= 2,
            "blob SHA must be at least 2 chars; got {} for {sha:?}",
            sha.len()
        );
        let shard = &sha[..2.min(sha.len())];
        self.root.join(shard).join(format!("{sha}.bin"))
    }
}
