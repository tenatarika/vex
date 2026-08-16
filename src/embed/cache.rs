//! v1.13 E2b — persistent content-addressed embedding cache.
//!
//! Closes the unforced error in `vex update`: when ANY symbol in a
//! file changes, the entire file is re-parsed, and historically every
//! symbol in that file was re-embedded — even the 99 of 100 that are
//! byte-identical to what was already in the index.
//!
//! The cache is a sidecar at `<index_dir>/embed_cache_<embedder_id>.bin`
//! keyed by `xxh3_64(embedder_id || \0 || context_string)`. The same
//! `context_string` produces the same hash across `vex update`
//! invocations (and across file renames — path isn't in the hash by
//! intention; only the symbol-shape contributed fields are), so any
//! symbol whose context didn't change skips the embed step.
//!
//! Pre-1.13 behaviour is the all-miss path: cache file absent → every
//! symbol is embedded → cache populated → subsequent invocations hit.
//!
//! ## Format (binary, little-endian)
//!
//! ```text
//! magic:           4 bytes "VEXE"
//! version:         u32 = 1
//! embedder_id_len: u32
//! embedder_id:     utf-8 bytes
//! dim:             u32
//! entry_count:     u32
//! For each entry:
//!   hash:   u64
//!   vector: f32 × dim
//! ```
//!
//! Magic / version / `embedder_id` / `dim` mismatch on load → discard.
//! Malformed body → discard. The cache is owner-trusted (same trust
//! level as `index.vex`) so we don't sign it; integrity boundary stays
//! at the ONNX SHA-256 check (P2).

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use xxhash_rust::xxh3::xxh3_64;

const MAGIC: &[u8; 4] = b"VEXE";
const VERSION: u32 = 1;

/// Largest entry count a well-formed cache can claim. Past this the header is
/// rejected outright, before the body it describes is read.
const MAX_ENTRIES: u32 = 10_000_000;

/// Largest embedder_id a well-formed cache can carry.
const MAX_ID_LEN: u32 = 256;

/// In-memory cache state. Built via [`EmbedCache::load`] or
/// [`EmbedCache::empty`]; [`EmbedCache::save`] persists.
pub struct EmbedCache {
    embedder_id: String,
    dim: u32,
    entries: HashMap<u64, Vec<f32>>,
}

impl EmbedCache {
    /// Empty cache for the given embedder + dim. Used both as the
    /// fresh-start path and as the fallback on any load error.
    pub fn empty(embedder_id: &str, dim: u32) -> Self {
        Self {
            embedder_id: embedder_id.to_string(),
            dim,
            entries: HashMap::new(),
        }
    }

    /// Load from disk. Returns an empty cache on any failure
    /// (missing file, malformed header, embedder/dim mismatch). The
    /// caller doesn't need to distinguish — every failure mode just
    /// degrades to "no hits".
    pub fn load(path: &Path, embedder_id: &str, dim: u32) -> Self {
        match Self::try_load(path, embedder_id, dim) {
            Ok(cache) => cache,
            Err(e) => {
                // Only log at debug — missing cache is the cold-start
                // case and shouldn't surface as a warning to users.
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "embed cache: load failed, starting empty"
                );
                Self::empty(embedder_id, dim)
            }
        }
    }

    fn try_load(path: &Path, embedder_id: &str, dim: u32) -> Result<Self> {
        // Header first — magic, version, embedder_id, dim, entry_count — every
        // one validated before a single vector byte is read. The header is
        // variable-length (the id), so it comes off in three steps. The body
        // read is then bounded by `entry_count × vector size`, so a corrupt
        // cache costs what it claims to be rather than what it weighs on disk.
        let mut reader =
            crate::util::sidecar::SidecarReader::open(path, MAGIC).context("open cache file")?;

        let head = reader.take_header(8).context("read header")?;
        let version = u32::from_le_bytes(head[0..4].try_into().expect("4 bytes"));
        if version != VERSION {
            anyhow::bail!("version mismatch: {} != {}", version, VERSION);
        }
        let id_len = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes"));
        // Sanity bound: any embedder_id longer than 256 chars is a
        // corrupt or attacker-supplied marker; bail rather than allocate
        // arbitrary memory.
        if id_len > MAX_ID_LEN {
            anyhow::bail!("embedder_id len absurd: {}", id_len);
        }

        let id_bytes = reader
            .take_header(id_len as usize)
            .context("read embedder_id")?
            .to_vec();
        let stored_id = std::str::from_utf8(&id_bytes).context("embedder_id utf8")?;
        if stored_id != embedder_id {
            anyhow::bail!("embedder_id mismatch: {} != {}", stored_id, embedder_id);
        }

        let tail = reader.take_header(8).context("read dim and entry count")?;
        let stored_dim = u32::from_le_bytes(tail[0..4].try_into().expect("4 bytes"));
        if stored_dim != dim {
            anyhow::bail!("dim mismatch: {} != {}", stored_dim, dim);
        }
        let entry_count = u32::from_le_bytes(tail[4..8].try_into().expect("4 bytes"));
        // Cap to a sensible upper bound — 10M entries × ~1.5 KiB each
        // = 15 GB, which is past anything legitimate. Reject so an
        // adversarial entry_count can't OOM us on read.
        if entry_count > MAX_ENTRIES {
            anyhow::bail!("entry_count absurd: {}", entry_count);
        }

        let vector_bytes = (dim as usize) * std::mem::size_of::<f32>();
        let header_bytes = 20 + id_len as u64;
        let bytes = reader.finish(entry_count as u64 * (8 + vector_bytes as u64))?;
        let mut file = std::io::Cursor::new(bytes.as_slice());
        file.set_position(header_bytes);

        // `entry_count` is only bounded by the absurdity check above; size the
        // map against the bytes the file actually holds.
        let remaining = bytes.len().saturating_sub(header_bytes as usize);
        let capacity = (entry_count as usize).min(remaining / (8 + vector_bytes.max(1)) + 1);
        let mut entries: HashMap<u64, Vec<f32>> = HashMap::with_capacity(capacity);
        let mut buf = vec![0u8; vector_bytes];
        for _ in 0..entry_count {
            let hash = read_u64(&mut file).context("read entry hash")?;
            file.read_exact(&mut buf).context("read entry vector")?;
            let mut vec = Vec::with_capacity(dim as usize);
            for chunk in buf.chunks_exact(4) {
                vec.push(f32::from_le_bytes(chunk.try_into().unwrap()));
            }
            entries.insert(hash, vec);
        }

        Ok(Self {
            embedder_id: embedder_id.to_string(),
            dim,
            entries,
        })
    }

    /// Lookup. `None` on miss.
    pub fn get(&self, hash: u64) -> Option<&[f32]> {
        self.entries.get(&hash).map(|v| v.as_slice())
    }

    /// Insert. Caller's responsibility to ensure `vec.len() == self.dim`;
    /// vectors of the wrong dim corrupt the on-disk format on the next
    /// save. Returns `false` and skips the insert on mismatch.
    pub fn insert(&mut self, hash: u64, vec: Vec<f32>) -> bool {
        if vec.len() != self.dim as usize {
            tracing::warn!(
                got = vec.len(),
                expected = self.dim,
                "embed cache: refused insert with wrong-dim vector"
            );
            return false;
        }
        self.entries.insert(hash, vec);
        true
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// v1.14 E3 — drop every cache entry whose hash is not in
    /// `live_hashes`. Returns the number of entries removed. The pipeline
    /// calls this once per `vex index` / `vex update` build, right after
    /// the miss-insertion step, with the full set of currently-indexed
    /// context hashes. Reclaims storage for symbols that got deleted or
    /// renamed since the last build — without this the cache grew
    /// monotonically and orphaned entries persisted forever.
    ///
    /// Safe to call with an empty slice (clears the cache). O(N + M)
    /// where N = `live_hashes.len()` and M = `self.entries.len()`.
    pub fn sweep_to(&mut self, live_hashes: &[u64]) -> usize {
        let live: std::collections::HashSet<u64> = live_hashes.iter().copied().collect();
        let before = self.entries.len();
        self.entries.retain(|hash, _| live.contains(hash));
        before - self.entries.len()
    }

    /// Atomic save: write to `.tmp` then rename. Matches the
    /// `index.vex` write convention so a crash mid-save can't corrupt
    /// the cache. `path`'s parent must exist (caller's job).
    pub fn save(&self, path: &Path) -> Result<()> {
        // Serialize into one buffer and write it once. Writing straight to the
        // file cost one `write_all` syscall **per `f32`** — with a 384-dim
        // embedder that is 385 syscalls per cached vector. Same bytes as before.
        let id_bytes = self.embedder_id.as_bytes();
        let vector_bytes = (self.dim as usize) * std::mem::size_of::<f32>();
        let mut out: Vec<u8> =
            Vec::with_capacity(20 + id_bytes.len() + self.entries.len() * (8 + vector_bytes));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(id_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(id_bytes);
        out.extend_from_slice(&self.dim.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());

        // Sort by hash so the file is byte-deterministic across
        // saves with the same entry set — helps when comparing
        // caches across machines and avoids HashMap iteration
        // order leaking into on-disk bytes.
        let mut sorted: Vec<(u64, &Vec<f32>)> = self.entries.iter().map(|(k, v)| (*k, v)).collect();
        sorted.sort_unstable_by_key(|(k, _)| *k);
        for (hash, vec) in sorted {
            out.extend_from_slice(&hash.to_le_bytes());
            for x in vec {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }

        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, &out).with_context(|| format!("write {}", tmp.display()))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e)
                .with_context(|| format!("rename {} → {}", tmp.display(), path.display()));
        }
        Ok(())
    }
}

/// Stable cache key. `embedder_id` is mixed in so the cache file
/// format's `embedder_id` field is belt + suspenders — a stray bin
/// blob from a different embedder would still produce non-matching
/// hashes on lookup.
pub fn context_hash(embedder_id: &str, context: &str) -> u64 {
    let mut buf = Vec::with_capacity(embedder_id.len() + 1 + context.len());
    buf.extend_from_slice(embedder_id.as_bytes());
    buf.push(0);
    buf.extend_from_slice(context.as_bytes());
    xxh3_64(&buf)
}

fn read_u64<R: Read>(r: &mut R) -> Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn vec_of(n: usize, fill: f32) -> Vec<f32> {
        vec![fill; n]
    }

    /// Pins the on-disk layout against hand-written bytes rather than against
    /// this module's own reader — a symmetric change to `save` and `load` would
    /// pass every round-trip test while silently discarding every cache already
    /// on a user's disk.
    #[test]
    fn save_emits_the_documented_byte_layout() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("c.bin");
        let mut cache = EmbedCache::empty("e", 2);
        cache.insert(1, vec![1.0, 2.0]);
        cache.save(&p).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(b"VEXE"); // magic
        expected.extend_from_slice(&1u32.to_le_bytes()); // version
        expected.extend_from_slice(&1u32.to_le_bytes()); // embedder_id len
        expected.extend_from_slice(b"e"); //                embedder_id
        expected.extend_from_slice(&2u32.to_le_bytes()); // dim
        expected.extend_from_slice(&1u32.to_le_bytes()); // entry count
        expected.extend_from_slice(&1u64.to_le_bytes()); // entry hash
        expected.extend_from_slice(&1.0f32.to_le_bytes()); // vector[0]
        expected.extend_from_slice(&2.0f32.to_le_bytes()); // vector[1]

        assert_eq!(std::fs::read(&p).unwrap(), expected);
    }

    /// Header fields whose bounds `try_load` checks. `load` swallows every
    /// error into an empty cache, so these are asserted through `try_load` —
    /// otherwise a lost check would look exactly like a cold start.
    #[test]
    fn rejects_absurd_header_fields() {
        let tmp = TempDir::new().unwrap();

        let mut over_id = Vec::new();
        over_id.extend_from_slice(MAGIC);
        over_id.extend_from_slice(&1u32.to_le_bytes());
        over_id.extend_from_slice(&(MAX_ID_LEN + 1).to_le_bytes());
        let p = tmp.path().join("id.bin");
        std::fs::write(&p, &over_id).unwrap();
        let err = match EmbedCache::try_load(&p, "e", 2) {
            Ok(_) => panic!("an absurd embedder_id len must not load"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("embedder_id len absurd"), "{err}");

        let mut over_entries = Vec::new();
        over_entries.extend_from_slice(MAGIC);
        over_entries.extend_from_slice(&1u32.to_le_bytes());
        over_entries.extend_from_slice(&1u32.to_le_bytes());
        over_entries.extend_from_slice(b"e");
        over_entries.extend_from_slice(&2u32.to_le_bytes());
        over_entries.extend_from_slice(&(MAX_ENTRIES + 1).to_le_bytes());
        let p = tmp.path().join("entries.bin");
        std::fs::write(&p, &over_entries).unwrap();
        let err = match EmbedCache::try_load(&p, "e", 2) {
            Ok(_) => panic!("an absurd entry_count must not load"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("entry_count absurd"), "{err}");

        // And the same claim made by a file too small to back it: the capacity
        // bound must not turn a 30-byte file into a huge pre-allocation.
        let cache = EmbedCache::load(&p, "e", 2);
        assert!(cache.is_empty());
    }

    /// The variable-length header comes off in three reads; a file that ends
    /// inside any of them must fail there rather than misread what follows.
    #[test]
    fn rejects_header_truncated_at_each_stage() {
        let tmp = TempDir::new().unwrap();
        let mut full = Vec::new();
        full.extend_from_slice(MAGIC);
        full.extend_from_slice(&VERSION.to_le_bytes());
        full.extend_from_slice(&1u32.to_le_bytes()); // id_len
        full.extend_from_slice(b"e");
        full.extend_from_slice(&2u32.to_le_bytes()); // dim
        full.extend_from_slice(&0u32.to_le_bytes()); // entry_count

        // 6: mid-version, 10: mid-id_len, 13: mid-dim, 17: mid-entry_count.
        for cut in [6usize, 10, 13, 17] {
            let p = tmp.path().join(format!("cut{cut}.bin"));
            std::fs::write(&p, &full[..cut]).unwrap();
            assert!(
                EmbedCache::try_load(&p, "e", 2).is_err(),
                "a {cut}-byte cache must not load as valid"
            );
        }
        // The untruncated file is a valid, empty cache — proof the cuts above
        // fail for the truncation and not for some other reason.
        let p = tmp.path().join("whole.bin");
        std::fs::write(&p, &full).unwrap();
        assert!(EmbedCache::try_load(&p, "e", 2).unwrap().is_empty());
    }

    /// Vectors past what `entry_count` claims are never read.
    #[test]
    fn trailing_junk_past_the_headers_claim_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("junk.bin");
        let mut cache = EmbedCache::empty("e", 2);
        cache.insert(1, vec![1.0, 2.0]);
        cache.save(&p).unwrap();

        let mut bytes = std::fs::read(&p).unwrap();
        bytes.extend(std::iter::repeat_n(0xFFu8, 100_000));
        std::fs::write(&p, &bytes).unwrap();

        let back = EmbedCache::try_load(&p, "e", 2).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.get(1).unwrap(), &[1.0, 2.0]);
    }

    /// A cache whose last vector is cut short must be rejected, not
    /// half-loaded.
    #[test]
    fn rejects_entry_truncated_mid_vector() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("cut.bin");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(b"e");
        bytes.extend_from_slice(&2u32.to_le_bytes()); // dim = 2 → 8 vector bytes
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one entry
        bytes.extend_from_slice(&7u64.to_le_bytes()); // hash
        bytes.extend_from_slice(&1.0f32.to_le_bytes()); // only half the vector
        std::fs::write(&p, &bytes).unwrap();

        assert!(EmbedCache::try_load(&p, "e", 2).is_err());
        assert!(EmbedCache::load(&p, "e", 2).is_empty());
    }

    #[test]
    fn roundtrip_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("c.bin");
        let cache = EmbedCache::empty("minilm-l6-v2", 384);
        cache.save(&p).unwrap();
        let reloaded = EmbedCache::load(&p, "minilm-l6-v2", 384);
        assert_eq!(reloaded.len(), 0);
    }

    #[test]
    fn roundtrip_two_entries() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("c.bin");
        let mut cache = EmbedCache::empty("minilm-l6-v2", 4);
        cache.insert(1, vec![0.1, 0.2, 0.3, 0.4]);
        cache.insert(2, vec![1.0, 1.0, 1.0, 1.0]);
        cache.save(&p).unwrap();

        let reloaded = EmbedCache::load(&p, "minilm-l6-v2", 4);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get(1).unwrap(), &[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(reloaded.get(2).unwrap(), &[1.0, 1.0, 1.0, 1.0]);
        assert!(reloaded.get(999).is_none());
    }

    #[test]
    fn missing_file_yields_empty_cache() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("never_existed.bin");
        let c = EmbedCache::load(&p, "minilm-l6-v2", 384);
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn embedder_id_mismatch_starts_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("c.bin");
        let mut cache = EmbedCache::empty("minilm-l6-v2", 4);
        cache.insert(42, vec![1.0, 2.0, 3.0, 4.0]);
        cache.save(&p).unwrap();

        // Same path, different embedder id — load must NOT return the
        // entry; otherwise the writer would mix incompatible vectors.
        let reloaded = EmbedCache::load(&p, "other-embedder", 4);
        assert_eq!(reloaded.len(), 0);
    }

    #[test]
    fn dim_mismatch_starts_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("c.bin");
        let mut cache = EmbedCache::empty("minilm-l6-v2", 4);
        cache.insert(42, vec![1.0, 2.0, 3.0, 4.0]);
        cache.save(&p).unwrap();

        let reloaded = EmbedCache::load(&p, "minilm-l6-v2", 8);
        assert_eq!(reloaded.len(), 0);
    }

    #[test]
    fn magic_mismatch_starts_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("c.bin");
        // Write valid-shape bytes but with a wrong magic prefix; the
        // loader must reject rather than read into the rest.
        std::fs::write(&p, b"NOPE\x01\x00\x00\x00").unwrap();
        let c = EmbedCache::load(&p, "minilm-l6-v2", 384);
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn truncated_file_starts_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("c.bin");
        std::fs::write(&p, b"VEXE\x01\x00\x00").unwrap(); // mid-version
        let c = EmbedCache::load(&p, "minilm-l6-v2", 384);
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn insert_wrong_dim_is_refused() {
        let mut cache = EmbedCache::empty("minilm-l6-v2", 4);
        // 3 elements instead of 4 → refused, no entry added.
        assert!(!cache.insert(1, vec_of(3, 0.5)));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn context_hash_is_deterministic() {
        let a = context_hash("minilm-l6-v2", "fn foo() {}");
        let b = context_hash("minilm-l6-v2", "fn foo() {}");
        assert_eq!(a, b);
    }

    #[test]
    fn context_hash_differs_by_embedder() {
        // Cache key MUST include embedder_id, otherwise two embedders
        // sharing the same context_string would silently reuse each
        // other's vectors (different spaces, garbage results).
        let a = context_hash("minilm-l6-v2", "fn foo() {}");
        let b = context_hash("other-embedder", "fn foo() {}");
        assert_ne!(a, b);
    }

    #[test]
    fn context_hash_differs_by_context() {
        let a = context_hash("minilm-l6-v2", "fn foo() {}");
        let b = context_hash("minilm-l6-v2", "fn bar() {}");
        assert_ne!(a, b);
    }

    #[test]
    fn save_is_atomic_via_tmp_rename() {
        // The tmp file must NOT exist after a successful save (rename
        // moved it). This pins the atomic-write contract that protects
        // a partially-written cache from being mistaken for a real one.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("c.bin");
        let cache = EmbedCache::empty("minilm-l6-v2", 4);
        cache.save(&p).unwrap();
        let leftover = p.with_extension("bin.tmp");
        assert!(!leftover.exists(), "tmp file should be renamed away");
        assert!(p.exists(), "final cache file should exist");
    }

    #[test]
    fn sweep_keeps_only_live_hashes() {
        // The headline E3 contract: orphaned entries (hashes that left
        // the index after a rename or deletion) are reclaimed; live
        // entries survive untouched. Without this, the cache grew
        // monotonically across every `vex index` invocation.
        let mut cache = EmbedCache::empty("minilm-l6-v2", 4);
        cache.insert(1, vec_of(4, 1.0));
        cache.insert(2, vec_of(4, 2.0));
        cache.insert(3, vec_of(4, 3.0));
        assert_eq!(cache.len(), 3);

        let removed = cache.sweep_to(&[1, 3]);
        assert_eq!(removed, 1, "hash 2 should have been swept");
        assert_eq!(cache.len(), 2);
        assert!(cache.get(1).is_some(), "hash 1 should survive");
        assert!(cache.get(2).is_none(), "hash 2 should be gone");
        assert!(cache.get(3).is_some(), "hash 3 should survive");
    }

    #[test]
    fn sweep_with_empty_live_set_clears_cache() {
        // Edge case: `live_hashes = []` should drop every entry. The
        // pipeline never produces this in practice (every indexed file
        // contributes at least one symbol), but the predicate must hold
        // — `sweep_to(&[])` is the simplest possible test of the loop's
        // termination behaviour.
        let mut cache = EmbedCache::empty("minilm-l6-v2", 4);
        cache.insert(1, vec_of(4, 1.0));
        cache.insert(2, vec_of(4, 2.0));
        let removed = cache.sweep_to(&[]);
        assert_eq!(removed, 2);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn sweep_no_op_when_all_live() {
        // Steady-state: every cached hash is in the live set → zero
        // removals, identical content. The pipeline's tracing logs
        // gate the "reclaimed N orphans" line on this returning > 0,
        // so the no-op path matters for log noise as much as correctness.
        let mut cache = EmbedCache::empty("minilm-l6-v2", 4);
        cache.insert(10, vec_of(4, 7.0));
        cache.insert(20, vec_of(4, 8.0));
        let removed = cache.sweep_to(&[10, 20, 30]);
        // 30 wasn't in the cache, so no harm; nothing to remove.
        assert_eq!(removed, 0);
        assert_eq!(cache.len(), 2);
    }
}
