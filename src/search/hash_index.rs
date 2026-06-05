//! v1.14.1 B1.1 — HNSW hash-index sidecar.
//!
//! Persists `Vec<u64>` of `context_hash` values in sym_idx order so the
//! semantic-search query path can map HNSW results (keyed by content
//! hash, not by `sym_idx`) back to `SymbolRecord` positions. The hash
//! keying is the prerequisite for B1.2 incremental update: a symbol's
//! key is stable across `vex update` runs (content-based), while the
//! old `sym_idx`-as-key scheme broke whenever any earlier file's
//! symbol count changed.
//!
//! Lives at `<index_dir>/index.hashes` next to `index.hnsw` and is
//! always co-written + co-removed with it. Stale or missing sidecar
//! makes `HnswHandle::open` bail to brute-force search, same
//! degradation path as a stale HNSW file itself.
//!
//! ## Format (binary, little-endian)
//!
//! ```text
//! magic:        4 bytes "VEXH"
//! version:      u32 = 1
//! count:        u32 (number of hashes)
//! data:         count × u64 (sym_idx-ordered)
//! ```
//!
//! No `embedder_id` field — the embed cache (`embed/cache.rs`) already
//! gates that boundary, and the sidecar is paired with `index.hnsw`
//! which carries the embedder's dimension in its own header. Magic /
//! version mismatch on load → discard, fall back to brute force.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};

const MAGIC: &[u8; 4] = b"VEXH";
const VERSION: u32 = 1;

/// Bound on the count field. ~10M symbols is past anything legitimate
/// (a 384-dim f32 vector blob alone would be 15 GB at that scale). The
/// reader bails before allocating against a crafted count.
const MAX_COUNT: u32 = 10_000_000;

/// Atomic save: write to `.tmp`, then rename. Matches the convention
/// used by `embed::cache::EmbedCache::save` and `store::writer` —
/// half-written sidecars can't be mistaken for valid ones.
pub fn save(path: &Path, hashes: &[u64]) -> Result<()> {
    // Match the read-side `MAX_COUNT` guard so a truncating `as u32` cast
    // can't silently write a wrong count for an index past 10M symbols —
    // the cast would otherwise produce a sidecar that `load` accepts as
    // valid but is short by `hashes.len() % (MAX_COUNT + 1)` entries.
    anyhow::ensure!(
        hashes.len() <= MAX_COUNT as usize,
        "hash count exceeds MAX_COUNT: {} > {}",
        hashes.len(),
        MAX_COUNT,
    );
    let tmp = path.with_extension("hashes.tmp");
    {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(MAGIC).context("write magic")?;
        file.write_all(&VERSION.to_le_bytes())
            .context("write version")?;
        file.write_all(&(hashes.len() as u32).to_le_bytes())
            .context("write count")?;
        for h in hashes {
            file.write_all(&h.to_le_bytes()).context("write hash")?;
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Best-effort cleanup if the rename failed (partial cross-FS
        // moves on Linux can leave the tmp behind).
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename {} → {}", tmp.display(), path.display()));
    }
    Ok(())
}

/// Load and validate. Returns an error on any failure (file missing,
/// magic mismatch, version mismatch, absurd count, truncated body) so
/// the caller can decide whether to bail or fall back. Callers that
/// want "absence ≡ no sidecar" semantics should check `path.exists()`
/// first.
pub fn load(path: &Path) -> Result<Vec<u64>> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;

    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).context("read magic")?;
    if &magic != MAGIC {
        anyhow::bail!("hash-index magic mismatch (got {:?})", magic);
    }

    let version = read_u32(&mut file).context("read version")?;
    if version != VERSION {
        anyhow::bail!("hash-index version mismatch: {} != {}", version, VERSION);
    }

    let count = read_u32(&mut file).context("read count")?;
    if count > MAX_COUNT {
        anyhow::bail!("hash-index count absurd: {} > {}", count, MAX_COUNT);
    }

    let mut hashes: Vec<u64> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        hashes.push(read_u64(&mut file).context("read hash")?);
    }
    Ok(hashes)
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

    #[test]
    fn round_trip_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("h.bin");
        save(&p, &[]).unwrap();
        let back = load(&p).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn round_trip_preserves_order_and_content() {
        // Sym_idx order MUST be preserved — the `HnswHandle::open`
        // builder relies on `hashes[i] -> sym_idx == i` to materialise
        // the hash→sym_idx map. A re-ordering bug here would silently
        // mis-attribute every semantic-search result.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("h.bin");
        let hashes = vec![1, 2, 3, 0xDEADBEEF, u64::MAX];
        save(&p, &hashes).unwrap();
        let back = load(&p).unwrap();
        assert_eq!(back, hashes);
    }

    #[test]
    fn save_is_atomic_via_tmp_rename() {
        // Same atomic-write contract `embed::cache::EmbedCache::save`
        // follows — a crash mid-save can't produce a partially-written
        // sidecar that load() would treat as valid. Pin it.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("h.bin");
        save(&p, &[1, 2, 3]).unwrap();
        let leftover = p.with_extension("hashes.tmp");
        assert!(!leftover.exists(), "tmp must be renamed away");
        assert!(p.exists(), "final file must exist");
    }

    #[test]
    fn rejects_bad_magic() {
        // Crafted sidecar with garbage magic — load must reject rather
        // than parse the rest as if it were our format.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("h.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(b"NOPE");
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_bad_version() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("h.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&999u32.to_le_bytes());
        bad.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_absurd_count() {
        // A crafted count past `MAX_COUNT` would allocate a huge Vec
        // before reading the body fails — bail at the header instead.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("h.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&(MAX_COUNT + 1).to_le_bytes());
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_truncated_body() {
        // Header claims 3 hashes, body has 1 — the read_u64 loop bails
        // on EOF and the function surfaces the error rather than
        // silently returning a short Vec.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("h.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&3u32.to_le_bytes());
        bad.extend_from_slice(&42u64.to_le_bytes());
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn missing_file_returns_err() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("nope.bin");
        // Caller is expected to check `exists()` first if it wants
        // "absence ≡ no sidecar" semantics; the loader doesn't decide
        // policy for them.
        assert!(load(&p).is_err());
    }
}
