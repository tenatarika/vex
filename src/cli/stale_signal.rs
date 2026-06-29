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

/// Read **and clear** the slot in one lock. The `--workspace` fanout calls
/// this after each member so the next member starts clean and a stale
/// reason is attributed to the member that produced it, not the whole run.
pub(crate) fn take() -> Option<String> {
    STALE_REASON.lock().ok().and_then(|mut s| s.take())
}

/// Clear the slot (production sibling of `reset_for_test`). The workspace
/// fanout clears once before its member loop so a signal set during
/// dispatch can't leak onto the first member.
pub(crate) fn reset() {
    if let Ok(mut slot) = STALE_REASON.lock() {
        *slot = None;
    }
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

    /// The slot is process-wide by design (one CLI invocation = one MCP
    /// request) — `LazyLock<Mutex<Option<String>>>` is correct in
    /// production but creates a parallelism hazard in tests, since
    /// `cargo test` runs `#[test]` functions in the same binary on
    /// shared threads. Splitting `reset`, `set`, and `current`
    /// assertions across multiple tests lets a second test mutate the
    /// slot between this one's reset and its assert.
    ///
    /// Solution: one test holds the slot for its full body, exercising
    /// every behavior (empty → set → first-wins → reset → empty)
    /// sequentially. No serialization primitive needed.
    #[test]
    fn slot_lifecycle_empty_set_first_wins_reset() {
        reset_for_test();
        assert!(
            current().is_none(),
            "after reset_for_test, current() must be None"
        );

        set("first failure");
        set("second failure");
        assert_eq!(
            current().as_deref(),
            Some("first failure"),
            "first set wins (idempotent-first-wins contract)"
        );

        // `take` returns the reason AND clears the slot (per-member capture).
        assert_eq!(
            take().as_deref(),
            Some("first failure"),
            "take returns the reason"
        );
        assert!(current().is_none(), "take must clear the slot");

        // `reset` clears the slot (production sibling of reset_for_test).
        set("another");
        reset();
        assert!(current().is_none(), "reset must clear the slot");

        reset_for_test();
        assert!(
            current().is_none(),
            "reset_for_test must clear the slot back to None"
        );
    }
}
