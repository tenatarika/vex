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
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use xxhash_rust::xxh3::xxh3_64;

const MAGIC: &[u8; 4] = b"VEXE";
const VERSION: u32 = 1;

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
        let mut file = std::fs::File::open(path).context("open cache file")?;
        let mut header = [0u8; 4];
        file.read_exact(&mut header).context("read magic")?;
        if &header != MAGIC {
            anyhow::bail!("magic mismatch");
        }

        let version = read_u32(&mut file).context("read version")?;
        if version != VERSION {
            anyhow::bail!("version mismatch: {} != {}", version, VERSION);
        }

        let id_len = read_u32(&mut file).context("read embedder_id len")?;
        // Sanity bound: any embedder_id longer than 256 chars is a
        // corrupt or attacker-supplied marker; bail rather than allocate
        // arbitrary memory.
        if id_len > 256 {
            anyhow::bail!("embedder_id len absurd: {}", id_len);
        }
        let mut id_bytes = vec![0u8; id_len as usize];
        file.read_exact(&mut id_bytes).context("read embedder_id")?;
        let stored_id = std::str::from_utf8(&id_bytes).context("embedder_id utf8")?;
        if stored_id != embedder_id {
            anyhow::bail!("embedder_id mismatch: {} != {}", stored_id, embedder_id);
        }

        let stored_dim = read_u32(&mut file).context("read dim")?;
        if stored_dim != dim {
            anyhow::bail!("dim mismatch: {} != {}", stored_dim, dim);
        }

        let entry_count = read_u32(&mut file).context("read entry count")?;
        // Cap to a sensible upper bound — 10M entries × ~1.5 KiB each
        // = 15 GB, which is past anything legitimate. Reject so an
        // adversarial entry_count can't OOM us on read.
        if entry_count > 10_000_000 {
            anyhow::bail!("entry_count absurd: {}", entry_count);
        }

        let mut entries: HashMap<u64, Vec<f32>> = HashMap::with_capacity(entry_count as usize);
        let vector_bytes = (dim as usize) * std::mem::size_of::<f32>();
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
        let tmp = path.with_extension("bin.tmp");
        {
            let mut file =
                std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            file.write_all(MAGIC).context("write magic")?;
            file.write_all(&VERSION.to_le_bytes())
                .context("write version")?;

            let id_bytes = self.embedder_id.as_bytes();
            file.write_all(&(id_bytes.len() as u32).to_le_bytes())
                .context("write id len")?;
            file.write_all(id_bytes).context("write embedder_id")?;

            file.write_all(&self.dim.to_le_bytes())
                .context("write dim")?;
            file.write_all(&(self.entries.len() as u32).to_le_bytes())
                .context("write entry count")?;

            // Sort by hash so the file is byte-deterministic across
            // saves with the same entry set — helps when comparing
            // caches across machines and avoids HashMap iteration
            // order leaking into on-disk bytes.
            let mut sorted: Vec<(u64, &Vec<f32>)> =
                self.entries.iter().map(|(k, v)| (*k, v)).collect();
            sorted.sort_unstable_by_key(|(k, _)| *k);
            for (hash, vec) in sorted {
                file.write_all(&hash.to_le_bytes())
                    .context("write entry hash")?;
                for x in vec {
                    file.write_all(&x.to_le_bytes())
                        .context("write entry vector")?;
                }
            }
        }
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

fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
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
