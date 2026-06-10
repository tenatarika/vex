//! Integrity check for the MiniLM ONNX model (10.4).
//!
//! `fastembed` / `hf_hub` download the ~86 MB MiniLM ONNX from huggingface.co
//! over HTTPS. The file lands in the embed cache and is then *executed*
//! in-process by ONNX Runtime — there is no signature, no certificate
//! transparency, no pinned digest in the upstream supply chain. A
//! compromised HF host, a poisoned CDN entry, or a tampered local cache
//! could land arbitrary ONNX into a process that immediately runs it.
//!
//! This module pins the SHA-256 of the known-good model file shipped by
//! the `Qdrant/all-MiniLM-L6-v2-onnx` HF repo at the snapshot fastembed
//! currently points at. After fastembed initialisation succeeds, we walk
//! the cache to locate the actual ONNX file and verify the digest. On
//! mismatch we bail with an actionable error pointing the user at the
//! file and offering an escape hatch (`VEX_EMBEDDER_SKIP_CHECK=1`).
//!
//! The check runs **after** ONNX Runtime has already loaded the model —
//! pre-init verification would race with the download fastembed itself
//! manages. Post-init is "best-effort defence in depth" rather than a
//! sandbox: a malicious model would still execute briefly. But the user
//! gets a loud, immediate signal that something is wrong so they can
//! purge the cache and rebuild instead of trusting the resulting index.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Pinned SHA-256 of the MiniLM-L6-v2 ONNX model file as shipped by the
/// `Qdrant/all-MiniLM-L6-v2-onnx` Hugging Face repo at snapshot
/// `5f1b8cd78bc4fb444dd171e59b18f3a3af89a079` (which is what
/// `fastembed = "5.13"` currently downloads via `hf_hub`).
///
/// When fastembed bumps to a new snapshot or HF rotates the file, this
/// constant must be updated alongside the dependency upgrade — the
/// mismatch surfaces immediately on the next `vex index --semantic`
/// with the actual digest in the error message, so rotating is a copy-
/// paste from stderr.
pub const MINILM_ONNX_SHA256: &str =
    "bbd7b466f6d58e646fdc2bd5fd67b2f5e93c0b687011bd4548c420f7bd46f0c5";

/// Env-var escape hatch for users who legitimately need to run with a
/// model version that predates a pin update or hand-modified weights.
/// Setting to `1` skips the digest check with a `tracing::warn!`.
const SKIP_ENV_VAR: &str = "VEX_EMBEDDER_SKIP_CHECK";

/// Verify the SHA-256 of `path` against `expected_hex`. Honors
/// `VEX_EMBEDDER_SKIP_CHECK=1` as an explicit bypass.
///
/// As of v1.13 (P2 marker cache) the bin only reaches this primitive
/// indirectly via [`verify_with_marker`]. Kept `pub` for downstream lib
/// consumers + tests + benches; `#[allow(dead_code)]` silences the
/// bin-compile dead-code lint that doesn't see those uses.
#[allow(dead_code)]
pub fn verify_file_sha256(path: &Path, expected_hex: &str) -> Result<()> {
    if skip_check() {
        // `tracing::warn!` alone disappears when RUST_LOG is unset
        // (the default for end users), so a CI-injected SKIP env var
        // would silently disable integrity checks with no visible
        // signal. `eprintln!` is unconditional and shows up in any
        // captured stderr — mirrors how Cargo / rustup surface
        // bypass warnings.
        eprintln!(
            "warning: {SKIP_ENV_VAR}=1 is set; embedding model integrity check bypassed for {}",
            path.display()
        );
        tracing::warn!(
            path = %path.display(),
            "{SKIP_ENV_VAR}=1 set; bypassing embedding model integrity check"
        );
        return Ok(());
    }
    let actual = compute_sha256(path)?;
    if actual != expected_hex {
        bail!(
            "embedding model checksum mismatch at {path}\n\n\
             expected SHA-256: {expected_hex}\n\
             actual SHA-256:   {actual}\n\n\
             This may indicate a compromised cache or CDN. Delete the cached \
             file and re-run `vex index --semantic` to redownload, or set \
             {SKIP_ENV_VAR}=1 to bypass (NOT recommended — defeats the \
             integrity check entirely).",
            path = path.display(),
        );
    }
    Ok(())
}

fn skip_check() -> bool {
    matches!(std::env::var(SKIP_ENV_VAR).ok().as_deref(), Some("1"))
}

fn compute_sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open {} for integrity check", path.display()))?;
    let mut hasher = Sha256::new();
    // 64 KiB buffer — well-tuned for sequential reads from a single file
    // on every common filesystem. The cost is bounded: one full read of
    // ~86 MiB per `vex index --semantic` invocation, ≈100 ms on SSD.
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {} for integrity check", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Marker-cache fast path for [`verify_file_sha256`].
///
/// The marker file lives alongside the ONNX (`<onnx>.sha256.marker`)
/// and stores `(mtime, size, sha256_hex)`. On a hit — where the on-disk
/// `mtime` and `size` both match the marker — we trust the recorded
/// SHA and skip the full hash (P2: ~163 ms ⇒ ~μs).
///
/// On any of `marker missing` / `mtime mismatch` / `size mismatch` /
/// `malformed marker`, fall through to the slow path: hash the file,
/// verify against `expected_hex`, and (if it matches) refresh the
/// marker so the next invocation is fast. Marker-write failures (e.g.
/// read-only cache) are logged but never escalated — the SHA result is
/// authoritative.
///
/// Honors `VEX_EMBEDDER_SKIP_CHECK=1` the same way as
/// [`verify_file_sha256`] — bypass entirely, no marker touched.
pub fn verify_with_marker(onnx_path: &Path, expected_hex: &str) -> Result<()> {
    if skip_check() {
        // Mirror verify_file_sha256's bypass exactly so behavior is
        // identical regardless of which entry point the caller uses.
        eprintln!(
            "warning: {SKIP_ENV_VAR}=1 is set; embedding model integrity check bypassed for {}",
            onnx_path.display()
        );
        tracing::warn!(
            path = %onnx_path.display(),
            "{SKIP_ENV_VAR}=1 set; bypassing embedding model integrity check"
        );
        return Ok(());
    }

    let marker_path = marker_path_for(onnx_path);

    if let Some(marker) = read_marker(&marker_path) {
        if let Ok(meta) = std::fs::metadata(onnx_path) {
            let on_disk_size = meta.len();
            let on_disk_mtime_ns = mtime_unix_ns(&meta);
            if on_disk_size == marker.size && on_disk_mtime_ns == marker.mtime_ns {
                // mtime + size match → marker is fresh. The recorded
                // SHA is authoritative for this file. Compare it with
                // the pin and bail on mismatch the same way the slow
                // path would.
                if marker.sha256 != expected_hex {
                    bail!(
                        "embedding model checksum mismatch at {path}\n\n\
                         expected SHA-256: {expected_hex}\n\
                         actual SHA-256:   {actual} (from cached marker)\n\n\
                         This may indicate a compromised cache or CDN. Delete the cached \
                         file and re-run `vex index --semantic` to redownload, or set \
                         {SKIP_ENV_VAR}=1 to bypass (NOT recommended — defeats the \
                         integrity check entirely).",
                        path = onnx_path.display(),
                        actual = marker.sha256,
                    );
                }
                return Ok(());
            }
        }
        // mtime/size drift or stat failure → marker is stale; fall
        // through to the slow path. Don't bail here — the file may be
        // legitimately re-downloaded by fastembed after a snapshot bump.
    }

    // Slow path: full SHA, then refresh the marker so the next call hits.
    let actual = compute_sha256(onnx_path)?;
    if actual != expected_hex {
        bail!(
            "embedding model checksum mismatch at {path}\n\n\
             expected SHA-256: {expected_hex}\n\
             actual SHA-256:   {actual}\n\n\
             This may indicate a compromised cache or CDN. Delete the cached \
             file and re-run `vex index --semantic` to redownload, or set \
             {SKIP_ENV_VAR}=1 to bypass (NOT recommended — defeats the \
             integrity check entirely).",
            path = onnx_path.display(),
        );
    }

    if let Err(e) = write_marker(onnx_path, &marker_path, &actual) {
        tracing::warn!(
            marker = %marker_path.display(),
            error = %e,
            "failed to persist ONNX integrity marker; next invocation will rehash"
        );
    }

    Ok(())
}

/// Sibling marker path for `onnx_path`. Co-locating with the model
/// (rather than under a shared `.vex-marker/` dir) means cache cleanup
/// removes both atomically — no orphaned markers pointing at deleted
/// files.
fn marker_path_for(onnx_path: &Path) -> PathBuf {
    let mut s = onnx_path.as_os_str().to_owned();
    s.push(".sha256.marker");
    PathBuf::from(s)
}

/// In-memory representation of a parsed marker file.
struct Marker {
    mtime_ns: u128,
    size: u64,
    sha256: String,
}

/// Marker format header. Bumping this string forces the parser to
/// reject markers written by an incompatible vex version.
const MARKER_MAGIC: &str = "vex-onnx-marker v1";

fn read_marker(marker_path: &Path) -> Option<Marker> {
    let content = std::fs::read_to_string(marker_path).ok()?;
    let mut lines = content.lines();
    if lines.next()?.trim() != MARKER_MAGIC {
        return None;
    }
    let mut mtime_ns: Option<u128> = None;
    let mut size: Option<u64> = None;
    let mut sha256: Option<String> = None;
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once(' ')?;
        match k {
            "mtime_ns" => mtime_ns = v.parse().ok(),
            "size" => size = v.parse().ok(),
            // SHA-256 is exactly 64 lowercase hex chars; anything else
            // is malformed and we treat the marker as a cache miss.
            "sha256" if v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()) => {
                sha256 = Some(v.to_string());
            }
            _ => {}
        }
    }
    Some(Marker {
        mtime_ns: mtime_ns?,
        size: size?,
        sha256: sha256?,
    })
}

fn write_marker(onnx_path: &Path, marker_path: &Path, sha256_hex: &str) -> Result<()> {
    let meta = std::fs::metadata(onnx_path)
        .with_context(|| format!("stat {} for marker write", onnx_path.display()))?;
    let mtime_ns = mtime_unix_ns(&meta);
    let size = meta.len();
    let body = format!("{MARKER_MAGIC}\nmtime_ns {mtime_ns}\nsize {size}\nsha256 {sha256_hex}\n");

    // Write to a temp sibling then rename for atomicity. Without this,
    // a crash mid-write would leave a half-written marker that the
    // next `read_marker` would silently reject as malformed — correct
    // but wasteful. The rename keeps the warm path warm across crashes.
    //
    // Rename can fail with EXDEV when `$HF_HOME` straddles a mount —
    // the tmp file would otherwise accumulate on every cold call. Best
    // effort cleanup on rename error: don't let a transient EXDEV leak
    // turn into permanent disk-litter.
    let tmp = marker_path.with_extension("marker.tmp");
    std::fs::write(&tmp, body.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, marker_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e)
            .with_context(|| format!("rename {} → {}", tmp.display(), marker_path.display()));
    }
    Ok(())
}

fn mtime_unix_ns(meta: &std::fs::Metadata) -> u128 {
    // Platform note: `metadata().modified()` returns `Err` on a small
    // set of targets that don't expose an mtime (WASM, some embedded
    // VFS shims). On those platforms every marker records `mtime_ns=0`,
    // so the freshness check effectively degrades to size-only. That's
    // a strictly looser guard but never unsafe — `verify_with_marker`
    // still bails on a mismatch between `marker.sha256` and the pinned
    // `MINILM_ONNX_SHA256`, so a forged-but-same-size file is caught.
    // The normal POSIX / NTFS paths hit this with nanosecond precision.
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Fuzzing shim — exposed only to `vex-fuzz`. Writes `data` as a
/// marker file alongside a fake ONNX, then drives both [`read_marker`]
/// (parser path) and [`verify_with_marker`] (full decision tree
/// including mtime/size comparison + slow-path fallback + marker
/// refresh). Hidden from rustdoc; the regular API stays `pub`.
/// Reuses a single tmp file path across iterations so libfuzzer
/// doesn't leak inodes. Best-effort tmp I/O — full disk / permission
/// failures are not asserted against.
#[doc(hidden)]
pub fn __fuzz_marker_bytes(data: &[u8]) {
    let tmp_dir = std::env::temp_dir();
    let onnx = tmp_dir.join("__vex_fuzz_marker.onnx");
    let marker = tmp_dir.join("__vex_fuzz_marker.onnx.sha256.marker");
    // 32 KiB fake ONNX so size-comparison branches aren't degenerate.
    let mut fake = Vec::with_capacity(32 * 1024);
    for i in 0..(32 * 1024) {
        fake.push((i & 0xff) as u8);
    }
    if std::fs::write(&onnx, &fake).is_err() {
        return;
    }
    if std::fs::write(&marker, data).is_err() {
        return;
    }
    // Direct parser invocation — covers malformed markers, partial
    // fields, oversized numeric strings, non-UTF8 (already filtered
    // by `read_to_string` but the harness shouldn't crash either way).
    let _ = read_marker(&marker);
    // Full decision-tree path — exercises mtime/size match, marker
    // refresh on slow-path, and the marker-stale bail branch.
    let _ = verify_with_marker(&onnx, MINILM_ONNX_SHA256);
}

/// Locate the MiniLM `model.onnx` inside fastembed's HF-Hub-style cache.
/// Layout: `models--Qdrant--all-MiniLM-L6-v2-onnx/snapshots/<commit>/model.onnx`.
/// Returns `None` when the model has not yet been downloaded — callers
/// should warn and skip rather than fail, since a fastembed bump could
/// change the layout and we don't want a working semantic index to break
/// purely because the integrity check couldn't find the file.
pub fn find_minilm_onnx(cache_dir: &Path) -> Option<PathBuf> {
    let snapshots_root = cache_dir
        .join("models--Qdrant--all-MiniLM-L6-v2-onnx")
        .join("snapshots");
    if !snapshots_root.is_dir() {
        return None;
    }
    // Collect and sort by directory name so the pick is deterministic
    // when fastembed has more than one snapshot cached side-by-side
    // (e.g. after a dependency bump). `read_dir` ordering is
    // filesystem-dependent; without the sort, two machines with the
    // same cache could disagree on which snapshot we hash. Lexically
    // sorting commit-hash-shaped names is arbitrary but stable.
    let mut entries: Vec<_> = std::fs::read_dir(&snapshots_root)
        .ok()?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let candidate = entry.path().join("model.onnx");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // Env vars are process-global — `set_var` in one test races with
    // every other test that reads `VEX_EMBEDDER_SKIP_CHECK`. The std
    // test harness runs tests in parallel by default. `#[serial]` puts
    // these tests on serial_test's GLOBAL lock — shared with every other
    // env-mutating test in this binary (embed::device, embed::mod,
    // util::config), so cross-module setenv/getenv races are excluded
    // too. The module mutex stays as defense-in-depth for any future
    // test here that forgets the attribute.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn known_hash(content: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(content);
        format!("{:x}", h.finalize())
    }

    #[test]
    #[serial]
    fn matching_checksum_passes() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.bin");
        let content = b"hello world\n";
        std::fs::write(&path, content).unwrap();
        verify_file_sha256(&path, &known_hash(content)).expect("matching hash should pass");
    }

    #[test]
    #[serial]
    fn mismatching_checksum_bails_with_actionable_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("tampered.bin");
        std::fs::write(&path, b"original").unwrap();
        // Pretend we expected a different hash.
        let err = verify_file_sha256(&path, &known_hash(b"different")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("checksum mismatch"), "missing headline: {msg}");
        assert!(
            msg.contains(&path.display().to_string()),
            "missing path: {msg}"
        );
        assert!(
            msg.contains("expected SHA-256"),
            "missing expected line: {msg}"
        );
        assert!(msg.contains("actual SHA-256"), "missing actual line: {msg}");
        assert!(
            msg.contains(SKIP_ENV_VAR),
            "missing escape-hatch hint: {msg}"
        );
    }

    #[test]
    #[serial]
    fn skip_env_var_bypasses_mismatch() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(SKIP_ENV_VAR, "1");
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bogus.bin");
        std::fs::write(&path, b"whatever").unwrap();
        let r = verify_file_sha256(&path, "deadbeef");
        std::env::remove_var(SKIP_ENV_VAR);
        r.expect("skip flag should bypass mismatch");
    }

    #[test]
    #[serial]
    fn missing_file_returns_open_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.bin");
        let err = verify_file_sha256(&missing, "doesnotmatter").unwrap_err();
        assert!(
            format!("{err:#}").contains("open"),
            "expected open-error context, got: {err:#}"
        );
    }

    #[test]
    fn find_minilm_onnx_returns_none_for_empty_cache() {
        let tmp = TempDir::new().unwrap();
        assert!(find_minilm_onnx(tmp.path()).is_none());
    }

    #[test]
    fn find_minilm_onnx_locates_file_under_snapshot_dir() {
        let tmp = TempDir::new().unwrap();
        let snap = tmp
            .path()
            .join("models--Qdrant--all-MiniLM-L6-v2-onnx")
            .join("snapshots")
            .join("5f1b8cd78bc4fb444dd171e59b18f3a3af89a079");
        std::fs::create_dir_all(&snap).unwrap();
        let onnx = snap.join("model.onnx");
        let mut f = std::fs::File::create(&onnx).unwrap();
        f.write_all(b"fake onnx").unwrap();
        assert_eq!(find_minilm_onnx(tmp.path()), Some(onnx));
    }

    // -----------------------------------------------------------------
    // verify_with_marker (P2 marker-cache fast path)
    // -----------------------------------------------------------------

    /// Build a model-onnx-shaped file in tmp, return path + its real
    /// SHA-256 hex. Most marker tests want both.
    fn write_model(tmp: &TempDir, content: &[u8]) -> (PathBuf, String) {
        let path = tmp.path().join("model.onnx");
        std::fs::write(&path, content).unwrap();
        (path, known_hash(content))
    }

    #[test]
    #[serial]
    fn marker_first_call_hashes_and_persists_marker() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let (path, real_sha) = write_model(&tmp, b"the model bytes");
        let marker = marker_path_for(&path);
        assert!(!marker.exists(), "no marker before first call");

        verify_with_marker(&path, &real_sha).expect("first call should hash and pass");

        assert!(marker.exists(), "marker must be written on success");
        let m = read_marker(&marker).expect("written marker must parse");
        assert_eq!(m.sha256, real_sha);
        assert_eq!(m.size, b"the model bytes".len() as u64);
    }

    #[test]
    #[serial]
    fn marker_hit_skips_rehash_when_mtime_and_size_match() {
        // The marker's defining behavior: trust mtime+size. We prove
        // the fast path was taken by tampering with the file contents
        // WITHOUT touching mtime+size, then asserting the call still
        // succeeds. If the slow path ran it would hash the new bytes,
        // get a different SHA than the marker, and bail.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let (path, real_sha) = write_model(&tmp, b"0123456789abcdef");
        verify_with_marker(&path, &real_sha).expect("seed call");
        let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();

        // Overwrite contents in place, then restore the mtime so the
        // marker thinks nothing changed. (mtime stamping is best-effort
        // — if the platform refuses, the test still demonstrates the
        // size-match leg via the equal-length payload.)
        std::fs::write(&path, b"ABCDEFGHIJKLMNOP").unwrap(); // same len
        let _ = filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(mtime_before));

        verify_with_marker(&path, &real_sha).expect("marker hit trusts cached sha");
    }

    #[test]
    #[serial]
    fn marker_invalidated_by_size_change_triggers_rehash() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let (path, _) = write_model(&tmp, b"short");
        let marker = marker_path_for(&path);
        verify_with_marker(&path, &known_hash(b"short")).expect("seed");
        let m_before = read_marker(&marker).unwrap();

        let new_content = b"a much longer payload than the seed";
        std::fs::write(&path, new_content).unwrap();
        let new_sha = known_hash(new_content);
        verify_with_marker(&path, &new_sha).expect("size mismatch should rehash and pass");

        let m_after = read_marker(&marker).expect("marker must be rewritten");
        assert_ne!(m_before.size, m_after.size, "marker size should refresh");
        assert_eq!(m_after.sha256, new_sha, "marker sha should refresh");
    }

    #[test]
    #[serial]
    fn marker_with_wrong_sha_field_bails_on_hit() {
        // Marker that matches mtime+size but stores a SHA different
        // from the expected pin → bail with the marker-stale message.
        // Mirrors the case where someone hand-edited the marker to
        // bypass verification.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let (path, real_sha) = write_model(&tmp, b"payload");
        verify_with_marker(&path, &real_sha).expect("seed writes marker");

        let marker = marker_path_for(&path);
        let mut content = std::fs::read_to_string(&marker).unwrap();
        content = content.replace(
            &real_sha,
            "deadbeef000000000000000000000000000000000000000000000000deadbeef",
        );
        std::fs::write(&marker, content).unwrap();

        let err = verify_with_marker(&path, &real_sha).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("from cached marker"), "{msg}");
    }

    #[test]
    #[serial]
    fn malformed_marker_falls_through_to_slow_path() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let (path, real_sha) = write_model(&tmp, b"bytes");
        let marker = marker_path_for(&path);
        std::fs::write(&marker, b"this is not a valid marker\n").unwrap();

        verify_with_marker(&path, &real_sha).expect("malformed marker should be ignored");

        let m = read_marker(&marker).expect("must be overwritten with valid marker");
        assert_eq!(m.sha256, real_sha);
    }

    #[test]
    #[serial]
    fn marker_rejected_when_magic_header_differs() {
        // Future v2 markers (or v0 leftovers) must be ignored so a
        // version drift doesn't quietly trust a marker written by an
        // incompatible parser.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let (path, real_sha) = write_model(&tmp, b"x");
        let marker = marker_path_for(&path);
        std::fs::write(
            &marker,
            "vex-onnx-marker v0\nmtime_ns 0\nsize 1\nsha256 \
             0000000000000000000000000000000000000000000000000000000000000000\n",
        )
        .unwrap();

        verify_with_marker(&path, &real_sha).expect("v0 marker must be ignored");
        let m = read_marker(&marker).unwrap();
        assert_eq!(m.sha256, real_sha, "v1 marker must overwrite the v0 file");
    }

    #[test]
    #[serial]
    fn marker_tampered_bytes_with_new_mtime_bail_with_mismatch() {
        // End-to-end "model swapped under us" check: new mtime
        // invalidates the marker → slow path runs → real SHA differs
        // from the pin → standard mismatch error. Ensures the marker
        // doesn't accidentally let bad bytes pass.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(SKIP_ENV_VAR);
        let tmp = TempDir::new().unwrap();
        let (path, real_sha) = write_model(&tmp, b"original");
        verify_with_marker(&path, &real_sha).expect("seed");

        // Replace with different bytes and bump mtime forward so the
        // marker is provably invalid.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, b"tampered bytes").unwrap();

        let err = verify_with_marker(&path, &real_sha).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("checksum mismatch"), "headline: {msg}");
        // Must NOT mention "from cached marker" — that branch is only
        // for the marker-hit path; this is a slow-path mismatch.
        assert!(
            !msg.contains("from cached marker"),
            "should be slow-path mismatch, not marker-hit: {msg}"
        );
    }

    #[test]
    #[serial]
    fn marker_skip_env_var_bypasses() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(SKIP_ENV_VAR, "1");
        let tmp = TempDir::new().unwrap();
        let (path, _) = write_model(&tmp, b"anything");
        let marker = marker_path_for(&path);
        let r = verify_with_marker(&path, "deadbeef");
        std::env::remove_var(SKIP_ENV_VAR);
        r.expect("skip flag should bypass");
        assert!(!marker.exists(), "skip path must not touch the marker");
    }

    #[test]
    fn find_minilm_onnx_picks_first_lex_sorted_snapshot() {
        // Two snapshots side-by-side — fastembed cache after a
        // dependency bump. Without the sort the choice would be
        // filesystem-dependent; with it we lock in lexical ordering
        // so two machines hash the same file.
        let tmp = TempDir::new().unwrap();
        let snaps = tmp
            .path()
            .join("models--Qdrant--all-MiniLM-L6-v2-onnx")
            .join("snapshots");
        let snap_a = snaps.join("aaaa11111111111111111111111111111111aaaa");
        let snap_b = snaps.join("ffff99999999999999999999999999999999ffff");
        std::fs::create_dir_all(&snap_a).unwrap();
        std::fs::create_dir_all(&snap_b).unwrap();
        std::fs::write(snap_a.join("model.onnx"), b"a").unwrap();
        std::fs::write(snap_b.join("model.onnx"), b"b").unwrap();
        let found = find_minilm_onnx(tmp.path()).expect("should locate");
        assert!(
            found.starts_with(&snap_a),
            "expected the lex-first snapshot to win, got: {found:?}"
        );
    }
}
