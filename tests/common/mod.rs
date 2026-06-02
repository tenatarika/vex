//! Shared helpers for `tests/cli_*_test.rs` integration tests.
//!
//! Each `tests/<name>.rs` file is compiled as a separate binary, so
//! Rust's normal `mod` sharing doesn't apply. The Cargo convention is
//! to put shared code under `tests/common/mod.rs` and pull it in with
//! `mod common;` at the top of each test file — the `mod.rs` form (vs
//! `tests/common.rs`) keeps Cargo from treating it as its own test
//! binary.
//!
//! Only put genuinely cross-cutting helpers here. Per-suite fixtures
//! belong next to their consumer.

use assert_cmd::Command;

/// Assert that `cmd` ran without erroring AND returned an exit code
/// that's part of the S8.2 contract (`0` found results, `1` empty
/// result set). v1.12.0 made every query subcommand distinguish those
/// two states; bare `.assert().success()` rejects `1`, so tests that
/// query empty/populated states in the same suite must use this
/// helper instead. Use plain `.success()` for action commands like
/// `vex index` / `vex update` — they only ever exit `0`.
///
/// Returns the underlying [`assert_cmd::assert::Assert`] so callers
/// can chain further expectations (`.stdout(...)`, `.get_output()`,
/// etc.) exactly as they would after `.assert()`.
pub fn assert_ran(cmd: &mut Command) -> assert_cmd::assert::Assert {
    let assert = cmd.assert();
    let code = assert.get_output().status.code();
    assert!(
        matches!(code, Some(0) | Some(1)),
        "expected exit code 0 (found) or 1 (no results), got: {code:?}"
    );
    assert
}
