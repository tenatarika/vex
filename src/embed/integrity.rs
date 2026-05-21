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
pub fn verify_file_sha256(path: &Path, expected_hex: &str) -> Result<()> {
    if skip_check() {
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
    let entries = std::fs::read_dir(&snapshots_root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("model.onnx");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // Env vars are process-global — `set_var` in one test races with
    // every other test that reads `VEX_EMBEDDER_SKIP_CHECK`. The std
    // test harness runs tests in parallel by default, so we serialise
    // every test in this module behind one mutex. `clear` always runs
    // (even on panic) via the `_guard` scope.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn known_hash(content: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(content);
        format!("{:x}", h.finalize())
    }

    #[test]
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
}
