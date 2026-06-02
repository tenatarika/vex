//! v1.12.0 S8.2 — explicit exit-code contract for `vex`.
//!
//! ## Contract
//!
//! - `0` — success. The command produced results (`vex search Foo` →
//!   found at least one symbol) or an action completed (`vex index`,
//!   `vex update`, `vex self-update`).
//! - `1` — expected empty outcome. The command ran without error but
//!   the answer is "nothing": `vex search Foo` matched zero symbols,
//!   `vex callers Bar` returned no edges, `vex find_symbol X` could not
//!   locate the symbol. CI / scripts gate on this distinct from real
//!   errors.
//! - `2` — actual error. Bad regex, corrupted index, I/O failure,
//!   invalid args past clap's own validation. Surfaced by `main`'s
//!   `Err(e)` arm with `Error: <message>` on stderr.
//!
//! ## Why a side-channel instead of `Result<ExitCode>` everywhere
//!
//! `vex` has ~25 CLI subcommands, each in its own `cmd_*.rs`. Changing
//! every handler's signature from `Result<()>` to `Result<ExitCode>`
//! plus every dispatch arm would be a wide-blast-radius refactor for a
//! property that is inherently process-global ("what code does this
//! process exit with?"). A static `AtomicBool` captures the only
//! distinction we need ("did the handler produce results?") with
//! near-zero call-site cost: handlers that find no results call
//! `signal_no_results()` once before returning their normal `Ok(())`.
//! The CLI binary runs exactly one subcommand per process, so the
//! global state has no concurrency footgun.
//!
//! Errors stay on the `Err` arm of `Result<()>` and are mapped to
//! exit code 2 by `main`. The atomic only encodes the success-vs-empty
//! distinction.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

/// `true` once a handler signalled "no results found"; read once by
/// [`finish`] before the process exits.
static NO_RESULTS: AtomicBool = AtomicBool::new(false);

/// Called by query handlers that found no matches. Idempotent — multiple
/// calls do not change behaviour (the first sets the flag, subsequent
/// calls re-set it to the same value). Handlers may also call this
/// conditionally based on whichever empty-detection logic suits their
/// query shape.
pub fn signal_no_results() {
    NO_RESULTS.store(true, Ordering::Relaxed);
}

/// Maps a handler's `Result<()>` into the final process exit code:
///
/// - `Ok(())` + no `signal_no_results()` call → [`ExitCode::SUCCESS`] (0)
/// - `Ok(())` + at least one `signal_no_results()` call → exit code 1
/// - `Err(_)` → propagates the error; `main` will print + exit with 2
pub fn finish(handler_result: anyhow::Result<()>) -> anyhow::Result<ExitCode> {
    handler_result?;
    Ok(if NO_RESULTS.load(Ordering::Relaxed) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Test-only helper that resets the flag. Production code never resets;
/// the CLI binary executes one subcommand and exits.
#[cfg(test)]
pub fn reset_for_tests() {
    NO_RESULTS.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All three scenarios in one test body because the `NO_RESULTS`
    /// atomic is process-global. `cargo test` runs unit tests in
    /// parallel within the same binary; if these were split across
    /// `#[test]` functions, the `signal_no_results()` from one test
    /// would race against `reset_for_tests()` in another and the
    /// "returns success when no signal" scenario would intermittently
    /// observe `true` and fail. The scenarios are independent enough
    /// to assert sequentially in one body — same approach as the
    /// VEX_BIN tests in `crates/vex-mcp/src/main.rs`.
    #[test]
    fn finish_maps_signal_and_error_to_correct_exit_codes() {
        // Scenario 1: no `signal_no_results()` → ExitCode::SUCCESS.
        reset_for_tests();
        let code = finish(Ok(())).expect("scenario 1 must succeed");
        assert_eq!(
            format!("{code:?}"),
            format!("{:?}", ExitCode::SUCCESS),
            "no signal → SUCCESS"
        );

        // Scenario 2: after `signal_no_results()` → ExitCode::from(1).
        reset_for_tests();
        signal_no_results();
        let code = finish(Ok(())).expect("scenario 2 must succeed");
        assert_eq!(
            format!("{code:?}"),
            format!("{:?}", ExitCode::from(1)),
            "signal → ExitCode(1)"
        );

        // Scenario 3: Err propagates unchanged regardless of signal state.
        reset_for_tests();
        let err: anyhow::Result<()> = Err(anyhow::anyhow!("oops"));
        let result = finish(err);
        assert!(result.is_err(), "scenario 3: Err must propagate");

        reset_for_tests();
    }
}
