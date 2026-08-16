//! v1.15.0 B1.2 — body_tokens sidecar.
//!
//! Persists `Vec<Option<String>>` of per-symbol `body_tokens` strings in
//! sym_idx order so `parse_files::reconstruct_unchanged` can restore them
//! during `vex update`. The `body_tokens` field participates in
//! `embed::build_context`, which feeds `context_hash`; without
//! persistence, reconstructed symbols produce body-less hashes and the
//! HNSW `index.hashes` sidecar drifts from a fresh `vex index` baseline.
//! That drift is what blocks B1.2 incremental HNSW update — every
//! "unchanged" symbol would appear as `removed → re-added` when diffing
//! the old hash set against the recomputed one.
//!
//! Lives at `<index_dir>/index.bodytokens`. Absence is a valid cold-start
//! state for pre-v1.15 indexes — the loader returns an empty `Vec` and
//! `reconstruct_unchanged` falls back to `body_tokens: None` (the
//! pre-existing behaviour). Format mismatch on load → bail; caller
//! decides whether to ignore or surface.
//!
//! ## Format (binary, little-endian)
//!
//! ```text
//! magic:        4 bytes "VEXT"
//! version:      u32 = 1
//! count:        u32 (number of records)
//! records:      count × { u32 byte_len, byte_len bytes UTF-8 }
//!               where byte_len == u32::MAX encodes `None`
//! ```
//!
//! `extract_body_tokens` truncates payloads at 400 bytes; the loader
//! enforces a 1024-byte cap per record (headroom for future widening
//! without invalidating existing sidecars). Bound on `count` mirrors
//! `hash_index::MAX_COUNT` so a crafted sidecar can't trigger a huge
//! allocation before the body-read fails.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

const MAGIC: &[u8; 4] = b"VEXT";
const VERSION: u32 = 1;

/// Bound on the count field. Mirrors `hash_index::MAX_COUNT` — past
/// 10M symbols a body-tokens sidecar would be ~10 MB even at the lower
/// 1KB/symbol ceiling, well within reason; the bound is the same
/// crafted-input guard, not a capacity limit.
const MAX_COUNT: u32 = 10_000_000;

/// Per-record byte-length cap. `extract_body_tokens` truncates at 400
/// bytes; 1024 leaves headroom for a future bump without invalidating
/// existing sidecars. The cap exists to prevent a crafted sidecar from
/// claiming a huge per-record length and exhausting memory before the
/// truncated body-read surfaces the error.
const MAX_BYTE_LEN: u32 = 1024;

/// Sentinel for `None`. `u32::MAX` is well past `MAX_BYTE_LEN` so it
/// can never collide with a legitimate byte length.
const NONE_SENTINEL: u32 = u32::MAX;

/// Magic + version + count.
const HEADER_BYTES: u64 = 12;

/// Largest a single record can be: its length prefix plus a payload at the cap.
/// `count × MAX_RECORD_BYTES` is what bounds the body read once the header has
/// been validated.
const MAX_RECORD_BYTES: u64 = 4 + MAX_BYTE_LEN as u64;

/// Atomic save: write to `.tmp`, then rename. Same convention as
/// `hash_index::save` and `embed::cache::EmbedCache::save`.
pub fn save(path: &Path, records: &[Option<String>]) -> Result<()> {
    anyhow::ensure!(
        records.len() <= MAX_COUNT as usize,
        "body_tokens count exceeds MAX_COUNT: {} > {}",
        records.len(),
        MAX_COUNT,
    );
    // Guard against an oversized record. `extract_body_tokens` enforces
    // the 400-byte cap on the write side, but a caller that bypasses
    // it (e.g. a future change to the extractor) would otherwise
    // silently produce a sidecar the loader rejects. Surface the bug
    // at write time, not first read time.
    for (i, r) in records.iter().enumerate() {
        if let Some(s) = r {
            anyhow::ensure!(
                s.len() <= MAX_BYTE_LEN as usize,
                "body_tokens[{}] exceeds MAX_BYTE_LEN: {} > {}",
                i,
                s.len(),
                MAX_BYTE_LEN,
            );
        }
    }
    // Serialize into one buffer and write it once. Writing directly to the file
    // issued two `write_all` syscalls per record — two per indexed symbol, for
    // a payload that is a few hundred bytes each. Byte-for-byte the same file.
    let mut out: Vec<u8> = Vec::with_capacity(12 + records.len() * 24);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for r in records {
        match r {
            None => out.extend_from_slice(&NONE_SENTINEL.to_le_bytes()),
            Some(s) => {
                let bytes = s.as_bytes();
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &out).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename {} → {}", tmp.display(), path.display()));
    }
    Ok(())
}

/// Load and validate. Returns an error on any malformed sidecar so the
/// caller can decide whether to bail or fall back to "no body_tokens".
/// Callers that want "absence ≡ legacy index" semantics should check
/// `path.exists()` first.
pub fn load(path: &Path) -> Result<Vec<Option<String>>> {
    // Header first: magic, version, count — validated before a single record
    // byte is read. The body read is then bounded by what `count` claims, so a
    // corrupt file costs what it says it is, not what it weighs. Records are
    // parsed from that buffer through a `Cursor`, which is why the loop below
    // is the original streaming reader with its `read_exact` calls intact —
    // they are memory copies now, not syscalls.
    let mut reader = crate::util::sidecar::SidecarReader::open(path, MAGIC)
        .with_context(|| format!("body_tokens sidecar at {}", path.display()))?;
    let header = reader.take_header(8).context("read header")?;
    let version = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
    if version != VERSION {
        anyhow::bail!("body_tokens version mismatch: {} != {}", version, VERSION);
    }
    let count = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes"));
    if count > MAX_COUNT {
        anyhow::bail!("body_tokens count absurd: {} > {}", count, MAX_COUNT);
    }

    let bytes = reader.finish(count as u64 * MAX_RECORD_BYTES)?;
    let mut file = std::io::Cursor::new(bytes.as_slice());
    file.set_position(HEADER_BYTES);

    // `count` is only bounded by MAX_COUNT, far above what any real file holds.
    // Size the allocation against the bytes actually present — a record cannot
    // be shorter than its length prefix.
    let remaining = bytes.len().saturating_sub(HEADER_BYTES as usize);
    let mut records: Vec<Option<String>> =
        Vec::with_capacity((count as usize).min(remaining / 4 + 1));
    for i in 0..count {
        let byte_len =
            read_u32(&mut file).with_context(|| format!("read byte_len at record {i}"))?;
        if byte_len == NONE_SENTINEL {
            records.push(None);
            continue;
        }
        if byte_len > MAX_BYTE_LEN {
            anyhow::bail!(
                "body_tokens[{i}] byte_len exceeds MAX_BYTE_LEN: {byte_len} > {MAX_BYTE_LEN}"
            );
        }
        let mut buf = vec![0u8; byte_len as usize];
        file.read_exact(&mut buf)
            .with_context(|| format!("read body_tokens bytes at record {i}"))?;
        let s = String::from_utf8(buf)
            .with_context(|| format!("body_tokens[{i}] is not valid UTF-8"))?;
        records.push(Some(s));
    }
    Ok(records)
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Pins the on-disk layout against hand-written bytes rather than against
    /// this module's own reader. A round-trip test cannot catch a change that
    /// alters `save` and `load` symmetrically — which would leave every sidecar
    /// already on disk silently misparsed, with no version bump to warn anyone.
    #[test]
    fn save_emits_the_documented_byte_layout() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        save(&p, &[Some("ab".to_string()), None, Some(String::new())]).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(b"VEXT"); // magic
        expected.extend_from_slice(&1u32.to_le_bytes()); // version
        expected.extend_from_slice(&3u32.to_le_bytes()); // count
        expected.extend_from_slice(&2u32.to_le_bytes()); // record 0: byte_len
        expected.extend_from_slice(b"ab"); //            record 0: payload
        expected.extend_from_slice(&u32::MAX.to_le_bytes()); // record 1: None
        expected.extend_from_slice(&0u32.to_le_bytes()); // record 2: empty string

        assert_eq!(std::fs::read(&p).unwrap(), expected);
    }

    /// The header is where the reader changed, and a file truncated inside it
    /// is the case the record-level tests never reach.
    #[test]
    fn rejects_header_truncated_mid_field() {
        let tmp = TempDir::new().unwrap();
        for cut in [0usize, 2, 5, 9] {
            let p = tmp.path().join(format!("cut{cut}.bin"));
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"VEXT");
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.truncate(cut);
            std::fs::write(&p, &bytes).unwrap();
            assert!(
                load(&p).is_err(),
                "a {cut}-byte sidecar must not load as valid"
            );
        }
    }

    /// An absurd `count` is rejected from the header alone — the body it
    /// claims is never read, which is the whole point of validating before
    /// sizing the read.
    #[test]
    fn absurd_count_is_rejected_from_the_header() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("absurd.bin");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&(MAX_COUNT + 1).to_le_bytes());
        std::fs::write(&p, &bytes).unwrap();

        let err = match load(&p) {
            Ok(_) => panic!("an absurd count must not load"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("count absurd"), "{err}");
    }

    /// Trailing bytes past what the header claims are ignored, not read into
    /// memory. A sidecar that was truncated and regrown — or simply corrupt —
    /// costs what its header says it is.
    #[test]
    fn trailing_junk_past_the_headers_claim_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("junk.bin");
        save(&p, &[Some("ab".to_string())]).unwrap();

        let mut bytes = std::fs::read(&p).unwrap();
        let honest_len = bytes.len();
        bytes.extend(std::iter::repeat_n(0xFFu8, 200_000));
        std::fs::write(&p, &bytes).unwrap();

        // One record of at most MAX_RECORD_BYTES is all the reader may take
        // beyond the header, so the junk cannot be paid for.
        assert!(honest_len < HEADER_BYTES as usize + MAX_RECORD_BYTES as usize);
        assert_eq!(load(&p).unwrap(), vec![Some("ab".to_string())]);
    }

    #[test]
    fn round_trip_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        save(&p, &[]).unwrap();
        let back = load(&p).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn round_trip_mixed_some_and_none() {
        // Mixed payload pins the Option<String> roundtrip — None must
        // come back as None (not as Some("")), Some(s) must preserve
        // the exact bytes. The reconstruct path relies on this to
        // distinguish "no body extracted" from "empty body extracted".
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        let records = vec![
            Some("fn foo bar".to_string()),
            None,
            Some(String::new()),
            Some("baz".to_string()),
            None,
        ];
        save(&p, &records).unwrap();
        let back = load(&p).unwrap();
        assert_eq!(back, records);
    }

    #[test]
    fn save_is_atomic_via_tmp_rename() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        save(&p, &[Some("a".into()), None]).unwrap();
        let leftover = p.with_extension("tmp");
        assert!(!leftover.exists(), "tmp must be renamed away");
        assert!(p.exists(), "final file must exist");
    }

    #[test]
    fn rejects_bad_magic() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
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
        let p = tmp.path().join("bt.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&999u32.to_le_bytes());
        bad.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_absurd_count() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&(MAX_COUNT + 1).to_le_bytes());
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_byte_len_past_cap() {
        // A crafted record claiming MAX_BYTE_LEN + 1 bytes would
        // either allocate a huge Vec or pass through and bloat the
        // sidecar. Both bad: bail at the length field.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        bad.extend_from_slice(&(MAX_BYTE_LEN + 1).to_le_bytes()); // bogus length
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_truncated_body() {
        // Header claims 2 records, body has 1 — read_exact bails on
        // EOF and the loader surfaces the error.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&2u32.to_le_bytes());
        bad.extend_from_slice(&3u32.to_le_bytes());
        bad.extend_from_slice(b"abc");
        // Missing record 2.
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_non_utf8() {
        // Crafted record with a body_len that matches but bytes that
        // aren't valid UTF-8 — String::from_utf8 must surface the
        // error rather than producing a lossy String.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&VERSION.to_le_bytes());
        bad.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        bad.extend_from_slice(&2u32.to_le_bytes()); // byte_len = 2
        bad.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8
        std::fs::write(&p, bad).unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn missing_file_returns_err() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("nope.bin");
        assert!(load(&p).is_err());
    }

    #[test]
    fn rejects_oversized_record_at_save_time() {
        // Surface a payload-cap violation at the write boundary so a
        // bug in `extract_body_tokens` truncation surfaces here, not
        // first read. `MAX_BYTE_LEN + 1` to clear the boundary unambiguously.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        let oversized = Some("a".repeat((MAX_BYTE_LEN + 1) as usize));
        assert!(save(&p, &[oversized]).is_err());
    }

    #[test]
    fn sym_idx_position_preserved() {
        // The whole point of the sidecar is that
        // `records[sym_idx]` is the body_tokens for that symbol.
        // A re-ordering bug in `save`/`load` would silently mis-attribute
        // every reconstructed symbol's body_tokens — pin it.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bt.bin");
        let records: Vec<Option<String>> = (0u32..100)
            .map(|i| {
                if i.is_multiple_of(3) {
                    None
                } else {
                    Some(format!("sym{i}"))
                }
            })
            .collect();
        save(&p, &records).unwrap();
        let back = load(&p).unwrap();
        assert_eq!(back, records);
        for (i, r) in back.iter().enumerate() {
            let i_u32 = i as u32;
            match r {
                None => assert!(i_u32.is_multiple_of(3)),
                Some(s) => assert_eq!(s, &format!("sym{i_u32}")),
            }
        }
    }
}
