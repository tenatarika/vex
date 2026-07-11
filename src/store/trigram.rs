//! grep trigram skip-index sidecar (STORAGE-RESEARCH §2, P2).
//!
//! Persists one record per indexed code file: its rel-path, a fixed-size
//! trigram presence bloom (built from raw file bytes — see
//! `crate::grep::trigram`), and the `(len, mtime)` the file had when the
//! bloom was computed. `vex grep` (P3) loads this sidecar and skips a
//! file only when BOTH (a) its bloom lacks a trigram the pattern requires
//! AND (b) the stored `(len, mtime)` still matches the file on disk.
//!
//! **The `(len, mtime)` guard is correctness-critical.** grep runs
//! WITHOUT a reindex, so between the last index and a grep the user may
//! have edited a file. If the stored bloom were trusted unconditionally,
//! an edit that introduced the pattern into a file whose old bloom lacked
//! it would be silently skipped — a false negative. Storing `(len,
//! mtime)` lets grep detect the drift (it already stats every file) and
//! fall back to a full read. A mismatch, a missing record, a malformed
//! sidecar, or a bloom-width change all degrade to "read the file" —
//! never to a skipped match.
//!
//! Lives at `<index_dir>/index.trigram`. Absence is a valid state
//! (pre-P2 index, or trigram build failed): the loader bails and grep
//! walks every file as it did before the sidecar existed.
//!
//! ## Format (binary, little-endian)
//!
//! ```text
//! magic:        4 bytes "VXTG"
//! version:      u32 = 1
//! bloom_bytes:  u32  (= grep::trigram::BLOOM_BYTES; a mismatch on load
//!                     rejects the whole sidecar — a P4 bloom-width tune
//!                     must not read old records at the wrong stride)
//! count:        u32  (number of records; ≤ MAX_COUNT)
//! records:      count × {
//!     path_len:    u16  (UTF-8 byte length; ≤ MAX_PATH_LEN)
//!     path:        path_len bytes  (POSIX-separator rel path)
//!     bloom:       bloom_bytes bytes
//!     len:         u64  (file byte length when indexed)
//!     mtime_secs:  i64  (signed offset from the Unix epoch)
//!     mtime_nanos: u32  (sub-second part, 0..1e9)
//! }
//! ```
//!
//! The `count` bound mirrors `body_tokens::MAX_COUNT`; `MAX_PATH_LEN`
//! and the `bloom_bytes` echo are the crafted-input guards that keep a
//! corrupt sidecar from triggering a huge allocation before the read
//! fails.

use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::grep::trigram::BLOOM_BYTES;

const MAGIC: &[u8; 4] = b"VXTG";
const VERSION: u32 = 1;

/// Bound on the record count. Mirrors `body_tokens::MAX_COUNT` — the same
/// crafted-input guard, not a capacity limit. At 10M files a sidecar
/// would already be ~2.7 GB at 278 B/record, well past any real repo.
const MAX_COUNT: u32 = 10_000_000;

/// Per-record path-length cap. Comfortably past `PATH_MAX` on every
/// mainstream platform (Linux 4096, macOS 1024, Windows 260/32767) so a
/// legitimate rel path never trips it, while bounding the allocation a
/// crafted `path_len` can request before the body read fails.
const MAX_PATH_LEN: u16 = 8192;

/// One persisted file's trigram record. `bloom` is the raw bloom bytes
/// (`grep::trigram::TrigramBloom::as_bytes`); the grep path wraps it back
/// via `TrigramBloom::from_raw`. `len` + `mtime` are the staleness guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrigramRecord {
    /// POSIX-separator path relative to the project root (see
    /// `crate::util::paths::to_rel_posix`). The grep query key must be
    /// derived the same way or lookups no-op on Windows.
    pub rel_path: String,
    /// Raw trigram presence bloom, `BLOOM_BYTES` long.
    pub bloom: [u8; BLOOM_BYTES],
    /// File byte length when the bloom was built.
    pub len: u64,
    /// File mtime when the bloom was built, as `(secs, nanos)` from the
    /// Unix epoch. Compared verbatim against the current mtime at grep
    /// time — any difference forces a full read (conservative).
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
}

/// Split a [`SystemTime`] into the `(secs, nanos)` pair we persist.
///
/// Both the writer (recording a file's mtime) and the grep reader
/// (recomputing the current file's mtime to compare) MUST funnel through
/// this so the comparison is apples-to-apples. Only determinism matters
/// for the staleness guard — the same `SystemTime` always maps to the
/// same pair — so the (vanishingly rare) pre-epoch branch just needs to
/// be stable, not perfectly reversible.
pub fn mtime_parts(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        // Pre-1970 mtime: represent as a negative second count. The
        // nanos of the "how far before the epoch" duration are kept so
        // two distinct pre-epoch times stay distinguishable.
        Err(e) => {
            let d = e.duration();
            (-(d.as_secs() as i64), d.subsec_nanos())
        }
    }
}

/// Atomic save: write to `.tmp`, then rename. Same convention as
/// `body_tokens::save`.
pub fn save(path: &Path, records: &[TrigramRecord]) -> Result<()> {
    anyhow::ensure!(
        records.len() <= MAX_COUNT as usize,
        "trigram record count exceeds MAX_COUNT: {} > {}",
        records.len(),
        MAX_COUNT,
    );
    for (i, r) in records.iter().enumerate() {
        anyhow::ensure!(
            r.rel_path.len() <= MAX_PATH_LEN as usize,
            "trigram[{}] rel_path exceeds MAX_PATH_LEN: {} > {}",
            i,
            r.rel_path.len(),
            MAX_PATH_LEN,
        );
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(MAGIC).context("write magic")?;
        file.write_all(&VERSION.to_le_bytes())
            .context("write version")?;
        file.write_all(&(BLOOM_BYTES as u32).to_le_bytes())
            .context("write bloom_bytes")?;
        file.write_all(&(records.len() as u32).to_le_bytes())
            .context("write count")?;
        for r in records {
            let path_bytes = r.rel_path.as_bytes();
            file.write_all(&(path_bytes.len() as u16).to_le_bytes())
                .context("write path_len")?;
            file.write_all(path_bytes).context("write path")?;
            file.write_all(&r.bloom).context("write bloom")?;
            file.write_all(&r.len.to_le_bytes()).context("write len")?;
            file.write_all(&r.mtime_secs.to_le_bytes())
                .context("write mtime_secs")?;
            file.write_all(&r.mtime_nanos.to_le_bytes())
                .context("write mtime_nanos")?;
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename {} → {}", tmp.display(), path.display()));
    }
    Ok(())
}

/// Load and validate. Returns an error on any malformed sidecar — bloom
/// width mismatch included — so the caller can fall back to a full walk.
/// Callers that want "absence ≡ no sidecar" semantics should check
/// `path.exists()` first (or just treat the error as "walk everything").
pub fn load(path: &Path) -> Result<Vec<TrigramRecord>> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;

    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).context("read magic")?;
    if &magic != MAGIC {
        anyhow::bail!("trigram magic mismatch (got {:?})", magic);
    }

    let version = read_u32(&mut file).context("read version")?;
    if version != VERSION {
        anyhow::bail!("trigram version mismatch: {} != {}", version, VERSION);
    }

    let bloom_bytes = read_u32(&mut file).context("read bloom_bytes")?;
    if bloom_bytes as usize != BLOOM_BYTES {
        anyhow::bail!(
            "trigram bloom width mismatch: {} != {} (bloom size changed since this sidecar was written)",
            bloom_bytes,
            BLOOM_BYTES
        );
    }

    let count = read_u32(&mut file).context("read count")?;
    if count > MAX_COUNT {
        anyhow::bail!("trigram count absurd: {} > {}", count, MAX_COUNT);
    }

    let mut records = Vec::with_capacity(count as usize);
    for i in 0..count {
        let path_len =
            read_u16(&mut file).with_context(|| format!("read path_len at record {i}"))?;
        if path_len > MAX_PATH_LEN {
            anyhow::bail!(
                "trigram[{i}] path_len exceeds MAX_PATH_LEN: {path_len} > {MAX_PATH_LEN}"
            );
        }
        let mut path_buf = vec![0u8; path_len as usize];
        file.read_exact(&mut path_buf)
            .with_context(|| format!("read path bytes at record {i}"))?;
        let rel_path = String::from_utf8(path_buf)
            .with_context(|| format!("trigram[{i}] path is not valid UTF-8"))?;

        let mut bloom = [0u8; BLOOM_BYTES];
        file.read_exact(&mut bloom)
            .with_context(|| format!("read bloom at record {i}"))?;

        let len = read_u64(&mut file).with_context(|| format!("read len at record {i}"))?;
        let mtime_secs =
            read_i64(&mut file).with_context(|| format!("read mtime_secs at record {i}"))?;
        let mtime_nanos =
            read_u32(&mut file).with_context(|| format!("read mtime_nanos at record {i}"))?;

        records.push(TrigramRecord {
            rel_path,
            bloom,
            len,
            mtime_secs,
            mtime_nanos,
        });
    }
    Ok(records)
}

fn read_u16<R: Read>(r: &mut R) -> Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
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

fn read_i64<R: Read>(r: &mut R) -> Result<i64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(i64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rec(path: &str, fill: u8, len: u64, secs: i64, nanos: u32) -> TrigramRecord {
        TrigramRecord {
            rel_path: path.to_string(),
            bloom: [fill; BLOOM_BYTES],
            len,
            mtime_secs: secs,
            mtime_nanos: nanos,
        }
    }

    #[test]
    fn round_trip_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        save(&p, &[]).unwrap();
        assert!(load(&p).unwrap().is_empty());
    }

    #[test]
    fn round_trip_records() {
        // Distinct blooms + metadata pin field ordering: a swap of
        // len/mtime or a bloom stride bug would corrupt the roundtrip.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        let records = vec![
            rec("src/main.rs", 0x00, 1234, 1_700_000_000, 42),
            rec("src/with space.rs", 0xFF, 0, 0, 0),
            rec("a/b/c/deep.py", 0xA5, u64::MAX, i64::MAX, 999_999_999),
        ];
        save(&p, &records).unwrap();
        assert_eq!(load(&p).unwrap(), records);
    }

    #[test]
    fn save_is_atomic_via_tmp_rename() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        save(&p, &[rec("x.rs", 1, 1, 1, 1)]).unwrap();
        assert!(
            !p.with_extension("tmp").exists(),
            "tmp must be renamed away"
        );
        assert!(p.exists());
    }

    #[test]
    fn rejects_bad_magic() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        std::fs::write(&p, b"NOPExxxxxxxx").unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_bad_version() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&999u32.to_le_bytes());
        bad.extend_from_slice(&(BLOOM_BYTES as u32).to_le_bytes());
        bad.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_bloom_width_mismatch() {
        // A P4 bloom-size tune must invalidate old sidecars, not read
        // them at the wrong stride (which would desync every field).
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&((BLOOM_BYTES + 1) as u32).to_le_bytes());
        bad.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_absurd_count() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&(BLOOM_BYTES as u32).to_le_bytes());
        bad.extend_from_slice(&(MAX_COUNT + 1).to_le_bytes());
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_path_len_past_cap() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&(BLOOM_BYTES as u32).to_le_bytes());
        bad.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        bad.extend_from_slice(&(MAX_PATH_LEN + 1).to_le_bytes()); // bogus path_len
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_oversized_path_at_save_time() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        let oversized = rec(&"a".repeat((MAX_PATH_LEN + 1) as usize), 0, 0, 0, 0);
        assert!(save(&p, &[oversized]).is_err());
    }

    #[test]
    fn rejects_truncated_body() {
        // Header claims 1 record; body stops mid-bloom → read_exact bails.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&(BLOOM_BYTES as u32).to_le_bytes());
        bad.extend_from_slice(&1u32.to_le_bytes());
        bad.extend_from_slice(&4u16.to_le_bytes()); // path_len = 4
        bad.extend_from_slice(b"x.rs");
        bad.extend_from_slice(&[0u8; 8]); // only 8 of BLOOM_BYTES bloom bytes
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_non_utf8_path() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&(BLOOM_BYTES as u32).to_le_bytes());
        bad.extend_from_slice(&1u32.to_le_bytes());
        bad.extend_from_slice(&2u16.to_le_bytes());
        bad.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8 path
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn missing_file_returns_err() {
        let tmp = TempDir::new().unwrap();
        assert!(load(&tmp.path().join("nope.bin")).is_err());
    }

    #[test]
    fn mtime_parts_matches_duration_since_and_is_deterministic() {
        let t = UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
        // `mtime_parts` must report exactly what `duration_since` yields for
        // the same `SystemTime` — it adds no truncation of its own. We derive
        // the expected pair from `t` rather than hard-coding the nanos:
        // Windows stores `SystemTime` as 100 ns FILETIME ticks, so the
        // sub-second part of `t` is coarser than the 123_456_789 we passed in.
        // That's fine for the staleness guard — the writer and the grep reader
        // both observe the same stored value — so the test pins delegation +
        // determinism, not a platform-specific nanosecond count.
        let d = t.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(mtime_parts(t), (d.as_secs() as i64, d.subsec_nanos()));
        assert_eq!(
            d.as_secs(),
            1_700_000_000,
            "seconds are exact on every platform"
        );
        // Same input → same output is what the guard's compare relies on.
        assert_eq!(mtime_parts(t), mtime_parts(t));
    }

    #[test]
    fn record_order_preserved() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("t.bin");
        let records: Vec<TrigramRecord> = (0u8..50)
            .map(|i| rec(&format!("f{i}.rs"), i, i as u64, i as i64, i as u32))
            .collect();
        save(&p, &records).unwrap();
        assert_eq!(load(&p).unwrap(), records);
    }
}
