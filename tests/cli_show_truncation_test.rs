//! Phase 13.3 — `vex show` smart-truncation flags
//!
//! Drives the actual `vex` binary via `assert_cmd`. Each test sets up
//! a tiny project, runs `vex index --auto-update`, then invokes
//! `vex show <name> [truncation-flag]` and asserts on the truncated
//! output shape. We pin the cache to the test's tempdir via
//! `VEX_CACHE_DIR` so the user's real cache is never touched.

use std::path::Path;

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;

/// Spawn the vex binary configured to use a project-local cache.
fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// Set up a Rust-only project containing a single multi-line function
/// `widget_renderer` with `LINES` body lines plus a 1-line signature.
/// The function body lines are `let v_i = i;` for i in 1..=LINES, so
/// counting them is trivial. Returns the tempdir guard.
fn make_rust_project(body_lines: usize) -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join(".vex.toml"),
        "auto_update = true\nlocal_cache = true\n",
    )
    .unwrap();
    let mut src = String::from("/// Doc comment for widget_renderer.\n");
    src.push_str("fn widget_renderer(input: i32) -> i32 {\n");
    for i in 1..=body_lines {
        src.push_str(&format!("    let v_{i} = {i};\n"));
    }
    src.push_str("    input\n");
    src.push_str("}\n");
    std::fs::write(tmp.path().join("src.rs"), src).unwrap();
    tmp
}

#[test]
fn show_signature_only_keeps_only_declaration_line() {
    let tmp = make_rust_project(5);
    let assert = vex_in(tmp.path())
        .args([
            "show",
            "widget_renderer",
            "--signature-only",
            "--format",
            "compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // The signature must be present...
    assert!(
        stdout.contains("fn widget_renderer(input: i32) -> i32"),
        "expected signature in stdout, got: {stdout}"
    );
    // ...and the body lines must NOT be.
    assert!(
        !stdout.contains("let v_1 = 1"),
        "body line leaked into signature-only output: {stdout}"
    );
    assert!(
        !stdout.contains("let v_5 = 5"),
        "body line leaked into signature-only output: {stdout}"
    );
}

#[test]
fn show_head_n_truncates_to_first_n_lines() {
    let tmp = make_rust_project(10);
    let assert = vex_in(tmp.path())
        .args([
            "show",
            "widget_renderer",
            "--head",
            "3",
            "--format",
            "compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // First three lines of the body should be present.
    assert!(stdout.contains("fn widget_renderer"), "signature missing");
    assert!(stdout.contains("let v_1 = 1"), "first body line missing");
    assert!(stdout.contains("let v_2 = 2"), "second body line missing");
    // The 5th content line must NOT be present (we capped at 3).
    assert!(
        !stdout.contains("let v_5 = 5"),
        "head truncation kept too many lines: {stdout}"
    );
    // Trailer indicator must mention the remaining count.
    assert!(
        stdout.contains("more lines"),
        "expected `... (N more lines)` trailer, got: {stdout}"
    );
}

#[test]
fn show_head_n_does_not_truncate_if_body_shorter_than_n() {
    let tmp = make_rust_project(2);
    let assert = vex_in(tmp.path())
        .args([
            "show",
            "widget_renderer",
            "--head",
            "100",
            "--format",
            "compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // Full body present, no trailer.
    assert!(stdout.contains("let v_1 = 1"));
    assert!(stdout.contains("let v_2 = 2"));
    assert!(
        !stdout.contains("more lines"),
        "should not emit trailer when body fits: {stdout}"
    );
}

#[test]
fn show_no_body_keeps_signature_and_docstring() {
    // For Rust, `vex show` extracts only the function body (the `fn`
    // declaration through the closing brace) — leading `///` doc
    // comments live ABOVE the body and are not part of the extracted
    // text. So `--no-body` for a Rust function ends up looking like
    // `--signature-only`: signature line, no body.
    //
    // Where `--no-body` differs is when leading docs live *inside* the
    // body (Python `def foo():\n    """docstring"""\n`). We exercise
    // that path in the unit-test suite (`no_body_keeps_python_docstring`).
    let tmp = make_rust_project(5);
    let assert = vex_in(tmp.path())
        .args([
            "show",
            "widget_renderer",
            "--no-body",
            "--format",
            "compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(stdout.contains("fn widget_renderer(input: i32) -> i32"));
    assert!(
        !stdout.contains("let v_1 = 1"),
        "body line leaked into --no-body output: {stdout}"
    );
    assert!(
        !stdout.contains("let v_5 = 5"),
        "body line leaked into --no-body output: {stdout}"
    );
}

#[test]
fn show_collapsed_currently_emits_full_body_with_warning() {
    // v1.9 NO-OP: --collapsed should emit the full body and surface a
    // "pending language-aware implementation" warning on stderr.
    let tmp = make_rust_project(3);
    let assert = vex_in(tmp.path())
        .args([
            "show",
            "widget_renderer",
            "--collapsed",
            "--format",
            "compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    // Body remains intact (NO-OP).
    assert!(stdout.contains("let v_1 = 1"));
    assert!(stdout.contains("let v_3 = 3"));
    // Warning surfaces on stderr.
    assert!(
        stderr.contains("pending"),
        "expected `pending` warning on stderr, got: {stderr}"
    );
}

#[test]
fn show_flags_are_mutually_exclusive() {
    let tmp = make_rust_project(3);
    let assert = vex_in(tmp.path())
        .args(["show", "widget_renderer", "--signature-only", "--head", "5"])
        .assert()
        .failure();
    // clap emits the conflict on stderr.
    assert.stderr(contains("cannot be used with"));
}

#[test]
fn show_json_truncation_metadata_present() {
    let tmp = make_rust_project(10);
    let assert = vex_in(tmp.path())
        .args(["show", "widget_renderer", "--head", "3", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout was:\n{stdout}"));
    // `vex show --format json` emits a bare array (no envelope) —
    // confirmed by inspection of `cli::mod.rs::Commands::Show`. The
    // per-item shape carries the truncation block.
    let arr = parsed.as_array().expect("expected JSON array");
    assert!(!arr.is_empty(), "expected at least one result");
    let first = &arr[0];
    let truncation = first
        .get("truncation")
        .unwrap_or_else(|| panic!("missing `truncation` block in: {first:#}"));
    assert_eq!(
        truncation.get("mode").and_then(|m| m.as_str()),
        Some("head"),
        "truncation.mode mismatch: {truncation:#}"
    );
    assert_eq!(
        truncation.get("kept_lines").and_then(|n| n.as_u64()),
        Some(3),
        "truncation.kept_lines mismatch: {truncation:#}"
    );
    let original = truncation
        .get("original_lines")
        .and_then(|n| n.as_u64())
        .expect("missing original_lines");
    assert!(
        original >= 10,
        "expected original_lines >= 10 (got {original}): {truncation:#}"
    );
}

#[test]
fn show_no_truncation_flag_is_backwards_compatible() {
    // Backwards-compat guarantee: `vex show Foo` with no truncation
    // flag must produce output that DOES contain the full body — i.e.
    // we did not accidentally apply any transform by default.
    let tmp = make_rust_project(5);
    let assert = vex_in(tmp.path())
        .args(["show", "widget_renderer", "--format", "compact"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(stdout.contains("let v_1 = 1"));
    assert!(stdout.contains("let v_5 = 5"));
    assert!(!stdout.contains("more lines"));
    // No truncation block in JSON when no flag set.
    let assert = vex_in(tmp.path())
        .args(["show", "widget_renderer", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let first = &parsed.as_array().unwrap()[0];
    assert!(
        first.get("truncation").is_none(),
        "unexpected truncation block in default JSON: {first:#}"
    );
}
