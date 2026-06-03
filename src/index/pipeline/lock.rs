//! RAII build-lock guard for the indexing pipeline.
//!
//! `IndexLock` wraps the `<index>.lock` sentinel file with `fs2`'s
//! advisory file lock so concurrent `vex` instances on the same project
//! serialize through one parser. Acquisition has a blocking variant
//! (`acquire`) and a non-blocking try-lock (`try_acquire`) that backs
//! the `--no-wait` CLI flag and `run_or_busy` / `update_or_busy`.
//!
//! Isolated from `mod.rs` so the locking concerns and the indexing
//! orchestration concerns can be read independently. The
//! `is_lock_contended` helper distinguishes platform-specific
//! contention errors (Linux EWOULDBLOCK vs Windows ERROR_LOCK_VIOLATION
//! — see `docs/CONCURRENCY.md`).

use std::path::Path;

use anyhow::{Context, Result};

use crate::util::config;

/// RAII guard over the per-index build lock (`<index>.lock`). Acquiring it
/// blocks until no other vex instance is building the same index, so only one
/// process runs the expensive parse + embed + write at a time. The lock is
/// released on drop (including on early return).
///
/// The lock file is created once and never deleted. Deleting it on release is
/// the classic `flock` + unlink race: a queued waiter keeps its handle on the
/// now-unlinked inode while a new instance creates a *fresh* inode under the
/// same name and locks it immediately — so both run concurrently. A stable,
/// never-deleted sentinel keeps every instance contending on one lock object.
pub(super) struct IndexLock {
    file: std::fs::File,
}

impl IndexLock {
    /// Opens (or creates) the persistent lock sentinel for the index at
    /// `root`. Returned alongside the underlying [`File`] so the caller can
    /// decide whether to block or to bail out under contention.
    pub(super) fn open(root: &Path) -> Result<(std::path::PathBuf, std::fs::File)> {
        let index_path = config::index_path(root);
        let cache_dir = index_path.parent().context("index path has no parent")?;
        std::fs::create_dir_all(cache_dir).context("create cache directory")?;
        let path = index_path.with_extension("lock");
        // Open-or-create the persistent sentinel; never truncated, never removed.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .context("open index lock file")?;
        Ok((path, file))
    }

    /// Blocking variant. Tries without waiting first so a contention event
    /// can be logged at `info` level before settling into the blocking wait
    /// — otherwise users staring at a "stuck" CLI (or an agent harness
    /// watching for output) get no signal for the duration of the peer's
    /// parse + embed + write.
    pub(super) fn acquire(root: &Path) -> Result<Self> {
        let (path, file) = Self::open(root)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(e) if is_lock_contended(&e) => {
                tracing::info!(
                    lock = %path.display(),
                    "waiting for index lock (another vex instance is indexing)"
                );
                fs2::FileExt::lock_exclusive(&file)
                    .context("acquire index lock (another vex instance may be indexing)")?;
            }
            Err(e) => {
                return Err(anyhow::Error::from(e).context("try-lock index lock"));
            }
        }
        Ok(Self { file })
    }

    /// v1.12.0 — non-blocking variant for `opts.no_wait`. Returns
    /// `Ok(None)` when a peer is currently holding the lock; the caller is
    /// expected to bail out with a "busy" outcome. Real I/O errors still
    /// propagate as `Err`.
    pub(super) fn try_acquire(root: &Path) -> Result<Option<Self>> {
        let (path, file) = Self::open(root)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(e) if is_lock_contended(&e) => {
                // Symmetric with `acquire`'s `tracing::info!` on contention,
                // but at `debug` because the no-wait caller has explicitly
                // opted into "skip on contention" — every contention event
                // is already surfaced via `Ok(None)` to the caller.
                tracing::debug!(
                    lock = %path.display(),
                    "try_acquire observed lock held; returning Busy"
                );
                Ok(None)
            }
            Err(e) => Err(anyhow::Error::from(e).context("try-lock index lock")),
        }
    }
}

/// True when `e` describes a lock-contention failure from `try_lock_exclusive`.
/// POSIX returns `ErrorKind::WouldBlock`; Windows returns `ERROR_LOCK_VIOLATION`
/// (raw OS error 33) which historically maps to `ErrorKind::Other` (the Rust
/// stdlib has not always normalized this). Matching both shapes keeps the
/// try-then-block diagnostic path working across platforms — without the
/// raw-code check, every Windows contention would bypass the "waiting for
/// index lock" log and return an outright error, breaking the
/// thundering-herd serialization the lock exists to provide.
fn is_lock_contended(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    // ERROR_LOCK_VIOLATION on Windows. The constant is exposed via
    // `winapi`/`windows-sys`, but we want to avoid adding a Windows-only
    // dep for one number — and the value is stable platform ABI.
    matches!(e.raw_os_error(), Some(33))
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        // Release the advisory lock; intentionally leave the lock file in place
        // (see the struct doc — unlinking it would break mutual exclusion under
        // contention).
        if let Err(e) = fs2::FileExt::unlock(&self.file) {
            tracing::warn!(error = %e, "failed to unlock index lock");
        }
    }
}
