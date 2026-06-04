//! Bloom filter for fast "definitely not in index" symbol-name
//! pre-filtering.
//!
//! v1.12.0 T4 — wired into `pipeline::run`/`update` (build + persist as
//! sidecar `index.bloom`) and `cmd_check` (pre-filter to skip FST
//! lookups for definitely-missing names). The format is a small custom
//! binary header followed by the raw bitmap; reconstruction goes
//! through `Bloom::from_existing`. A fixed sip-key seed keeps rebuilds
//! deterministic, which matters for test fixtures and reproducible
//! cache keys.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use bloomfilter::Bloom;

use crate::index::symbols::ParsedFile;

/// Sidecar file magic. Distinct from the main `index.vex` magic so a
/// crossed-up file is rejected immediately.
const MAGIC: &[u8; 4] = b"VEXB";

/// Sidecar format version. Bump on any layout change.
const VERSION: u32 = 1;

/// Bloom false-positive rate target. 1% is the established default
/// from the previous unit tests and balances bitmap size against FP
/// noise for the search hot path.
const FP_RATE: f64 = 0.01;

/// Fixed sip-key seed for deterministic builds. Two consecutive `vex
/// index` runs over the same input must produce byte-identical bloom
/// sidecars so reproducible-build checks pass; non-deterministic seeds
/// would also confuse content-hash-based cache keys.
const SEED: [u8; 32] = *b"vex.bloom.seed.v1...............";

/// Sanity cap on the on-disk bitmap length. 256 MiB is well above the
/// largest plausible vex index bloom (~10 MB for a million symbols at
/// 1% FP). A crafted sidecar declaring more is treated as malformed
/// rather than allocated.
const MAX_BITMAP_LEN: usize = 256 * 1024 * 1024;

pub(crate) struct SymbolBloom {
    filter: Bloom<str>,
}

impl SymbolBloom {
    /// Allocate a bloom sized for `expected_items` at the configured
    /// FP rate, with the fixed `SEED` so builds are deterministic.
    /// `expected_items` is clamped to at least 1 because `bloomfilter`
    /// panics on a zero-item sizing.
    pub(crate) fn new(expected_items: usize) -> Self {
        let items = expected_items.max(1);
        let filter = Bloom::new_for_fp_rate_with_seed(items, FP_RATE, &SEED);
        Self { filter }
    }

    /// Build a bloom directly from the parsed-file output of the
    /// indexing pipeline. Inserts each symbol's name and, when the
    /// lowercased form differs, the lowercased name as well — case
    /// folding mirrors the pre-existing `from_reader` behaviour so
    /// callers can do case-insensitive `may_contain` checks.
    pub(crate) fn from_parsed_files(parsed: &[ParsedFile]) -> Self {
        let total: usize = parsed.iter().map(|f| f.symbols.len()).sum();
        // Size for 2x: original + lowercased variant per symbol.
        let mut bloom = Self::new(total * 2);
        for file in parsed {
            for sym in &file.symbols {
                if sym.name.is_empty() {
                    continue;
                }
                bloom.insert(&sym.name);
                let lower = sym.name.to_lowercase();
                if lower != sym.name {
                    bloom.insert(&lower);
                }
            }
        }
        bloom
    }

    pub(crate) fn insert(&mut self, name: &str) {
        self.filter.set(name);
    }

    /// Returns `false` if the name is definitely NOT in the bloom set
    /// (and therefore not in the index). Returns `true` if the name
    /// MIGHT be present — caller must consult the real index.
    pub(crate) fn may_contain(&self, name: &str) -> bool {
        self.filter.check(name)
    }

    /// Serialize the bloom to the sidecar binary format. The layout is
    /// magic + version + bloom params + bitmap. Build failures are
    /// non-fatal upstream, so the caller is expected to log and
    /// continue rather than abort the index run.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let bitmap = self.filter.bitmap();
        let n_bits = self.filter.number_of_bits();
        let k_num = self.filter.number_of_hash_functions();
        let keys = self.filter.sip_keys();

        let mut buf: Vec<u8> = Vec::with_capacity(64 + bitmap.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&n_bits.to_le_bytes());
        buf.extend_from_slice(&k_num.to_le_bytes());
        buf.extend_from_slice(&[0_u8; 4]); // pad to 8-byte alignment
        buf.extend_from_slice(&keys[0].0.to_le_bytes());
        buf.extend_from_slice(&keys[0].1.to_le_bytes());
        buf.extend_from_slice(&keys[1].0.to_le_bytes());
        buf.extend_from_slice(&keys[1].1.to_le_bytes());
        buf.extend_from_slice(&(bitmap.len() as u64).to_le_bytes());
        buf.extend_from_slice(&bitmap);

        // Atomic write: write to `index.bloom.tmp` then rename. Mirrors
        // the pattern the main index writer uses so a crashed `vex
        // index` can never leave half-written bloom bytes that a
        // subsequent reader would mmap and trust. `sync_data` is best-
        // effort — failure there means the durable-on-disk guarantee
        // weakens but content is still consistent, so we don't abort
        // the index run for it.
        let tmp = path.with_extension("bloom.tmp");
        let write_result = (|| -> Result<()> {
            let mut f =
                std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(&buf)
                .with_context(|| format!("write {}", tmp.display()))?;
            f.sync_data().ok();
            Ok(())
        })();
        if let Err(e) = write_result {
            // Best-effort cleanup so a failed write doesn't leave the
            // tmp on disk; the next successful save would overwrite it
            // anyway, but cleaning up keeps `ls .vex_cache` tidy.
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Load a bloom from disk. Returns `Ok(None)` when the file is
    /// absent (a valid "no bloom built yet" state — callers fall back
    /// to direct FST lookups). `Err` on malformed content, version
    /// mismatch, or truncation; the caller treats that the same as
    /// absent (log + skip) so a corrupt sidecar can't wedge `vex check`.
    pub(crate) fn load(path: &Path) -> Result<Option<Self>> {
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;

        if buf.len() < 64 {
            bail!("bloom sidecar truncated: {} bytes", buf.len());
        }
        if &buf[0..4] != MAGIC {
            bail!("bloom sidecar magic mismatch");
        }
        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        if version != VERSION {
            bail!("bloom sidecar version {version} != expected {VERSION}");
        }
        let n_bits = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let k_num = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        // bytes 20..24 = pad
        let k00 = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        let k01 = u64::from_le_bytes(buf[32..40].try_into().unwrap());
        let k10 = u64::from_le_bytes(buf[40..48].try_into().unwrap());
        let k11 = u64::from_le_bytes(buf[48..56].try_into().unwrap());
        let raw_bitmap_len = u64::from_le_bytes(buf[56..64].try_into().unwrap());
        // Reject implausibly-large sizes before any allocation or
        // arithmetic. The cap guards against (a) `as usize` truncation
        // on 32-bit targets and (b) a `64 + bitmap_len` overflow in
        // the truncation check below.
        let bitmap_len: usize = usize::try_from(raw_bitmap_len)
            .ok()
            .filter(|&n| n <= MAX_BITMAP_LEN)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "bloom sidecar bitmap_len implausible: {raw_bitmap_len} (cap {MAX_BITMAP_LEN})"
                )
            })?;
        // Consistency: `bloomfilter` panics inside `check()` if `n_bits`
        // exceeds the bitmap's actual capacity (it indexes `BitVec` past
        // the end). A vex-written sidecar always has these in sync;
        // reject mismatched files explicitly so a tampered or truncated
        // sidecar can never propagate to a panic deep in the crate.
        if n_bits != bitmap_len as u64 * 8 {
            bail!(
                "bloom sidecar inconsistent: n_bits={n_bits} != bitmap_len_bytes={bitmap_len} * 8"
            );
        }
        if buf.len() < 64 + bitmap_len {
            bail!(
                "bloom sidecar truncated: header claims bitmap_len={bitmap_len}, file has {} bytes",
                buf.len() - 64
            );
        }
        let bitmap = &buf[64..64 + bitmap_len];
        let filter = Bloom::from_existing(bitmap, n_bits, k_num, [(k00, k01), (k10, k11)]);
        Ok(Some(Self { filter }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserted_items_found() {
        let mut bloom = SymbolBloom::new(1000);
        bloom.insert("PaymentService");
        bloom.insert("UserRepository");

        assert!(bloom.may_contain("PaymentService"));
        assert!(bloom.may_contain("UserRepository"));
    }

    #[test]
    fn missing_items_usually_rejected() {
        let mut bloom = SymbolBloom::new(1000);
        for i in 0..100 {
            bloom.insert(&format!("Symbol{i}"));
        }
        let mut false_positives = 0;
        for i in 1000..2000 {
            if bloom.may_contain(&format!("Other{i}")) {
                false_positives += 1;
            }
        }
        // With 1% FP rate and 1000 tests, expect ~10 false positives.
        assert!(
            false_positives < 50,
            "too many false positives: {false_positives}"
        );
    }

    #[test]
    fn save_load_roundtrip_preserves_membership() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.bloom");

        let mut original = SymbolBloom::new(500);
        for i in 0..200 {
            original.insert(&format!("Sym{i}"));
        }
        original.save(&path).expect("save");

        let loaded = SymbolBloom::load(&path).expect("load").expect("Some");
        for i in 0..200 {
            assert!(
                loaded.may_contain(&format!("Sym{i}")),
                "Sym{i} should be in loaded bloom"
            );
        }
    }

    #[test]
    fn load_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.bloom");
        let out = SymbolBloom::load(&path).expect("load");
        assert!(out.is_none(), "missing file must return Ok(None)");
    }

    #[test]
    fn load_errors_on_bad_magic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.bloom");
        std::fs::write(&path, vec![0_u8; 128]).unwrap();
        let result = SymbolBloom::load(&path);
        assert!(result.is_err(), "must error on missing/bad magic");
    }

    #[test]
    fn load_rejects_implausibly_large_bitmap_len() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.bloom");
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&0_u64.to_le_bytes()); // n_bits
        buf.extend_from_slice(&0_u32.to_le_bytes()); // k_num
        buf.extend_from_slice(&[0_u8; 4]); // pad
        buf.extend_from_slice(&[0_u8; 32]); // sip_keys
                                            // Claim a bitmap larger than the sanity cap so the load path
                                            // rejects it before allocating / overflowing the bounds check.
        buf.extend_from_slice(&(MAX_BITMAP_LEN as u64 + 1).to_le_bytes());
        std::fs::write(&path, buf).unwrap();
        let result = SymbolBloom::load(&path);
        assert!(result.is_err(), "MAX_BITMAP_LEN cap must reject this");
    }

    #[test]
    fn load_rejects_n_bits_mismatched_with_bitmap_len() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("inconsistent.bloom");
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        // n_bits says 999, bitmap_len says 8 (== 64 bits) → mismatch.
        buf.extend_from_slice(&999_u64.to_le_bytes());
        buf.extend_from_slice(&7_u32.to_le_bytes());
        buf.extend_from_slice(&[0_u8; 4]);
        buf.extend_from_slice(&[0_u8; 32]);
        buf.extend_from_slice(&8_u64.to_le_bytes());
        buf.extend_from_slice(&[0_u8; 8]);
        std::fs::write(&path, buf).unwrap();
        let result = SymbolBloom::load(&path);
        assert!(
            result.is_err(),
            "n_bits != bitmap_len * 8 must be rejected (else bloomfilter::check panics)"
        );
    }

    #[test]
    fn load_rejects_version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("future.bloom");
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(VERSION + 1).to_le_bytes());
        buf.extend_from_slice(&[0_u8; 56]);
        std::fs::write(&path, buf).unwrap();
        let result = SymbolBloom::load(&path);
        assert!(
            result.is_err(),
            "newer version must be rejected, not silently accepted"
        );
    }

    #[test]
    fn load_errors_on_truncated_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trunc.bloom");
        // Only 32 bytes — shorter than the 64-byte header.
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&[0_u8; 24]);
        std::fs::write(&path, buf).unwrap();
        let result = SymbolBloom::load(&path);
        assert!(result.is_err(), "must error on truncated header");
    }

    #[test]
    fn deterministic_seed_produces_stable_bitmap() {
        // Two bloom builds with identical inputs must produce identical
        // bitmaps — required for reproducible builds + content-hashing.
        let mut a = SymbolBloom::new(100);
        let mut b = SymbolBloom::new(100);
        for i in 0..50 {
            a.insert(&format!("Stable{i}"));
            b.insert(&format!("Stable{i}"));
        }
        assert_eq!(a.filter.bitmap(), b.filter.bitmap());
    }

    #[test]
    fn from_parsed_files_inserts_each_symbol_name() {
        use crate::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
        let parsed = vec![ParsedFile {
            path: "a.rs".to_string(),
            symbols: vec![
                ParsedSymbol {
                    name: "Foo".to_string(),
                    kind: SymbolKind::Function,
                    line: 1,
                    signature: None,
                    doc: None,
                    body_tokens: None,
                },
                ParsedSymbol {
                    name: "BarBaz".to_string(),
                    kind: SymbolKind::Function,
                    line: 2,
                    signature: None,
                    doc: None,
                    body_tokens: None,
                },
            ],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
        }];
        let bloom = SymbolBloom::from_parsed_files(&parsed);
        assert!(bloom.may_contain("Foo"));
        assert!(bloom.may_contain("BarBaz"));
        // Lowercase variants inserted as well.
        assert!(bloom.may_contain("foo"));
        assert!(bloom.may_contain("barbaz"));
    }
}
