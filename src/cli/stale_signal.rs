//! v1.15.1 HIGH — stale-index signal for `_meta.stale` / `_meta.stale_reason`.
//!
//! Pre-v1.15.1, a failed `pipeline::update` in `handle_staleness` bubbled
//! up as `Err` → the CLI exited non-zero → the MCP wrapper surfaced
//! `exit code 2` to the caller (or, in one observed case for `vex usages`,
//! a successful-looking `{results: []}` envelope wrapped *inside* the
//! MCP error text — a correctness trap where an agent reads "0 usages"
//! and trusts it).
//!
//! v1.15.1 changes the contract: rebuild failure is no longer fatal.
//! `handle_staleness` records the reason here and returns `Ok(())`,
//! the command proceeds against the existing (stale) index, and the
//! response envelope advertises the degradation via `_meta.vex.dev/stale`
//! + `_meta.vex.dev/stale_reason` so the caller can see what happened.
//!
//! ## Scope and lifetime
//!
//! One slot per CLI invocation. Each `vex` subprocess is a single
//! MCP request, so a `LazyLock<Mutex<Option<String>>>` is naturally
//! request-scoped without any thread-local plumbing. The signal is
//! set at most once during the staleness check and read at most once
//! during envelope construction; it is not cleared on read (callers
//! that build multiple envelopes in one CLI run — currently none —
//! would all see the same reason).

use std::sync::{LazyLock, Mutex};

static STALE_REASON: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

/// Record that the index is stale and an auto-update attempt failed.
/// `reason` should be a short user-facing string (the formatted error
/// from `pipeline::update`); it appears verbatim in the response
/// envelope so prefer concise wording.
///
/// Idempotent within a CLI invocation: the first reason set wins (we
/// never overwrite — a later success doesn't unset a prior failure,
/// because the on-disk index didn't actually refresh).
pub(crate) fn set(reason: impl Into<String>) {
    if let Ok(mut slot) = STALE_REASON.lock() {
        slot.get_or_insert_with(|| reason.into());
    }
}

/// Read the current stale reason if any. Used by `output::build_search_meta`
/// / `output::default_meta_for` when stamping the response envelope.
pub(crate) fn current() -> Option<String> {
    STALE_REASON.lock().ok().and_then(|s| s.clone())
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    if let Ok(mut slot) = STALE_REASON.lock() {
        *slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests in this module share the process-wide `STALE_REASON` slot.
    /// `cargo test` runs tests in the same binary in parallel by default,
    /// so each test must explicitly reset and the assertions must tolerate
    /// the small chance another test inserts between the reset and the
    /// read. We serialize via a per-test `reset` to keep them deterministic
    /// when run in isolation (`--test-threads=1` or `nextest`'s default
    /// process-per-binary model).

    #[test]
    fn first_set_wins() {
        reset_for_test();
        set("first failure");
        set("second failure");
        assert_eq!(current().as_deref(), Some("first failure"));
    }

    #[test]
    fn current_is_none_when_unset() {
        reset_for_test();
        assert!(current().is_none());
    }
}
