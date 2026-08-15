//! v1.18 (audit C1) — incremental-state sidecar `<index_dir>/index.state`.
//!
//! ## Why
//!
//! `Manifest` accreted ten or so fields whose semantics are "writer
//! provenance / phase state / reverse-index cache" — distinct from the
//! core fingerprint table (`files`) and the sticky user-toggle opt-outs.
//! `imported_by` in particular scales O(cross-file-edges) in bytes (3–5
//! MB JSON at typical-repo scale, multi-GB at monorepo scale) and is
//! fully reserialised on every `vex update`, so the JSON path is a
//! measurable cost on watch-mode hot paths.
//!
//! This module relocates those fields to a binary sidecar in bincode
//! form. v1.18 first moved the *storage* (the fields stayed flat on
//! `Manifest`, shuttled via a hand-written capture/apply pair); v1.21
//! nested them into `Manifest::state: IncrementalState`, deleting the
//! shuttle. The on-disk wire format (this sidecar + the JSON manifest)
//! is unchanged by the v1.21 nesting.
//!
//! - `Manifest::save` writes JSON without these fields
//!   (`Manifest::state` is `#[serde(skip_serializing)]`) and writes the
//!   sidecar from `self.state`.
//! - `Manifest::load` reads JSON, then overlays the sidecar into
//!   `manifest.state` (sidecar wins). The sidecar is the SOLE store:
//!   pre-v1.18 indexes have no sidecar and carried these fields inline
//!   in JSON, but post-v1.21 those inline keys are unknown and silently
//!   ignored — `state` stays default and the next `vex update`
//!   re-derives `imported_by` (the re-bootstrap contract).
//!
//! ## Format (binary, little-endian)
//!
//! ```text
//! magic:        4 bytes "VEXS"
//! version:      u32 = 1
//! payload_len:  u32  (bincode-encoded `IncrementalState` length in bytes)
//! payload:      payload_len bytes — bincode of `IncrementalState`
//! ```
//!
//! ## Threat model
//!
//! Same surface as `manifest.json` — `index.state` is user-owned, the
//! reader cannot reach across the trust boundary. Defense-in-depth: a
//! 256 MiB byte cap rejects pathologically large sidecars before
//! bincode allocates the payload `Vec`. Fuzzed via `fuzz_state_load`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::index::manifest::HistoryStats;

const MAGIC: &[u8; 4] = b"VEXS";
const VERSION: u32 = 2;

/// Hard ceiling on payload size. 256 MiB covers a dense `imported_by`
/// map across hundreds of thousands of files plus every other state
/// field; anything larger is almost certainly hostile (bincode would
/// otherwise allocate the payload `Vec` before failing).
const MAX_PAYLOAD_BYTES: u32 = 256 * 1024 * 1024;

/// The incremental-rebuild state persisted out-of-band, nested into
/// `Manifest` as the `state` field (v1.21). Field names mirror the
/// historical flat `Manifest` fields so call sites read naturally as
/// `manifest.state.imported_by` etc.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IncrementalState {
    pub imported_by: BTreeMap<String, BTreeSet<String>>,
    /// v1.25.6 — per-file `(len, mtime)` fingerprints from the last run,
    /// keyed by the same POSIX-relative path as `Manifest::files`.
    ///
    /// Lets `hash_files` skip **reading** a file whose stat is byte-identical
    /// to last time and reuse the recorded content hash instead. On a
    /// one-file edit that turns "read and hash every tracked file" into "stat
    /// every tracked file", which on a 3.6 GB / 6083-file repo was 38 ms of
    /// the 551 ms `vex update`.
    ///
    /// Empty on any index written before this field existed, which simply
    /// means the next run hashes everything and repopulates it.
    pub file_stats: BTreeMap<String, FileStat>,
    pub imported_by_built: Option<bool>,
    pub cpp_includes_processed: Option<bool>,
    pub body_tokens_persisted: Option<bool>,
    pub history_indexed_at: Option<String>,
    pub history_tip_sha: Option<String>,
    pub history_depth: Option<usize>,
    /// `depth_capped` travels inside this nested struct — no separate
    /// top-level field on `IncrementalState`.
    pub history: Option<HistoryStats>,
}

/// Stat fingerprint of a file as of the last index write.
///
/// `mtime_ns` is nanoseconds since the Unix epoch. Both fields must match for
/// the cached content hash to be reused — see
/// `pipeline::parse_files::hash_files` for the third condition (the
/// racily-clean guard) that makes the reuse safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub len: u64,
    pub mtime_ns: u64,
}

/// Atomic save: write to `.tmp`, then rename. Same convention as the
/// other binary sidecars (`hash_index`, `embed_cache`, `body_tokens`).
pub fn save(path: &Path, state: &IncrementalState) -> Result<()> {
    let tmp = path.with_extension("state.tmp");
    save_to_tmp(&tmp, state)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename {} → {}", tmp.display(), path.display()));
    }
    Ok(())
}

fn save_to_tmp(tmp_path: &Path, state: &IncrementalState) -> Result<()> {
    let payload = bincode::serialize(state).context("bincode encode IncrementalState")?;
    if payload.len() > MAX_PAYLOAD_BYTES as usize {
        bail!(
            "incremental-state payload {} bytes exceeds {}-byte limit",
            payload.len(),
            MAX_PAYLOAD_BYTES,
        );
    }
    let mut file = std::fs::File::create(tmp_path)
        .with_context(|| format!("create {}", tmp_path.display()))?;
    file.write_all(MAGIC).context("write magic")?;
    file.write_all(&VERSION.to_le_bytes())
        .context("write version")?;
    file.write_all(&(payload.len() as u32).to_le_bytes())
        .context("write payload_len")?;
    file.write_all(&payload).context("write payload")?;
    file.sync_all().context("fsync state tmp")?;
    Ok(())
}

/// Load and validate. Returns `Err` on any failure (file missing, magic
/// mismatch, version mismatch, oversized payload, truncated body, bincode
/// parse error) so the caller — [`crate::index::manifest::Manifest::load`]
/// — can decide whether to bail or fall back to the legacy JSON path.
pub fn load(path: &Path) -> Result<IncrementalState> {
    let meta = std::fs::metadata(path).context("stat state sidecar")?;
    // Header is 12 bytes; payload is bounded by MAX_PAYLOAD_BYTES. The
    // header check protects against pathologically large `payload_len`
    // before the read_to_end allocation.
    if meta.len() > (MAX_PAYLOAD_BYTES as u64) + 12 {
        bail!(
            "state sidecar {} bytes exceeds {} byte ceiling (refusing to load)",
            meta.len(),
            (MAX_PAYLOAD_BYTES as u64) + 12,
        );
    }

    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;

    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).context("read magic")?;
    if &magic != MAGIC {
        bail!("state sidecar magic mismatch (got {:?})", magic);
    }

    let version = read_u32(&mut file).context("read version")?;
    if version != VERSION {
        bail!("state sidecar version mismatch: {} != {}", version, VERSION);
    }

    let payload_len = read_u32(&mut file).context("read payload_len")?;
    if payload_len > MAX_PAYLOAD_BYTES {
        bail!(
            "state sidecar payload_len absurd: {} > {}",
            payload_len,
            MAX_PAYLOAD_BYTES,
        );
    }

    let mut payload = vec![0u8; payload_len as usize];
    file.read_exact(&mut payload).context("read payload")?;
    bincode::deserialize::<IncrementalState>(&payload).context("bincode decode IncrementalState")
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Fuzzing shim — exposed only to `vex-fuzz`. Writes `data` as a state
/// sidecar to a process-temp path, then drives [`load`]. Mirrors the
/// other `__fuzz_*_bytes` shims; libfuzzer's success metric is process
/// survival, so the return value is discarded.
#[doc(hidden)]
pub fn __fuzz_state_bytes(data: &[u8]) {
    let path = std::env::temp_dir().join("__vex_fuzz_state.bin");
    if std::fs::write(&path, data).is_err() {
        return;
    }
    let _ = load(&path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> IncrementalState {
        let mut imported_by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut importers = BTreeSet::new();
        importers.insert("src/b.rs".to_string());
        importers.insert("src/c.rs".to_string());
        imported_by.insert("src/a.rs".to_string(), importers);

        IncrementalState {
            imported_by,
            file_stats: BTreeMap::from([(
                "src/a.rs".to_string(),
                FileStat {
                    len: 1234,
                    mtime_ns: 1_700_000_000_000_000_000,
                },
            )]),
            imported_by_built: Some(true),
            cpp_includes_processed: Some(true),
            body_tokens_persisted: Some(true),
            history_indexed_at: Some("2026-06-17".to_string()),
            history_tip_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            history_depth: Some(500),
            history: Some(HistoryStats {
                commit_count: 42,
                blob_count: 100,
                entry_count: 555,
                depth_capped: Some(false),
            }),
        }
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.state");
        let original = sample_state();
        save(&path, &original).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.imported_by, original.imported_by);
        assert_eq!(loaded.imported_by_built, original.imported_by_built);
        assert_eq!(
            loaded.cpp_includes_processed,
            original.cpp_includes_processed
        );
        assert_eq!(loaded.body_tokens_persisted, original.body_tokens_persisted);
        assert_eq!(loaded.history_indexed_at, original.history_indexed_at);
        assert_eq!(loaded.history_tip_sha, original.history_tip_sha);
        assert_eq!(loaded.history_depth, original.history_depth);
        let l = loaded.history.unwrap();
        let o = original.history.unwrap();
        assert_eq!(l.commit_count, o.commit_count);
        assert_eq!(l.blob_count, o.blob_count);
        assert_eq!(l.entry_count, o.entry_count);
        assert_eq!(l.depth_capped, o.depth_capped);
    }

    #[test]
    fn default_state_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.state");
        save(&path, &IncrementalState::default()).unwrap();
        let loaded = load(&path).unwrap();
        assert!(loaded.imported_by.is_empty());
        assert!(loaded.imported_by_built.is_none());
    }

    #[test]
    fn load_rejects_bad_magic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("garbage.state");
        std::fs::write(&path, b"NOPE\x01\x00\x00\x00\x00\x00\x00\x00").unwrap();
        let err = load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("magic mismatch"));
    }

    #[test]
    fn load_rejects_oversized_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("huge.state");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len((MAX_PAYLOAD_BYTES as u64) + 13).unwrap();
        drop(f);
        let err = load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("exceeds"));
    }

    #[test]
    fn load_rejects_oversized_payload_len_field() {
        // A crafted header that claims a payload larger than the cap must
        // be rejected BEFORE the body read allocates `Vec<u8>` of that size.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("crafted.state");
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&(MAX_PAYLOAD_BYTES + 1).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let err = load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("absurd"));
    }
}
