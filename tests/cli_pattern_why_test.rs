//! CLI integration tests for `vex pattern --why` (Phase 11.4 Inc 5).
//!
//! Verifies that the skeleton-based prefilter is engaged when an index
//! is present, that the live-scan fallback fires correctly when the
//! index is absent or empty, and that root-kind inference is reflected
//! in the trace.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

/// v1.12.0 S8.2 — `vex pattern` exits 1 when no matches. These tests
/// probe the `--why` trace on stderr regardless of result count, so
/// accept exit 0 or 1.
fn assert_ran(cmd: &mut Command) -> assert_cmd::assert::Assert {
    let assert = cmd.assert();
    let code = assert.get_output().status.code();
    assert!(
        matches!(code, Some(0) | Some(1)),
        "expected exit 0 or 1, got: {code:?}"
    );
    assert
}

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// Write a minimal Rust project with a few function definitions and index it.
fn write_and_index_rust_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        dir.join("lib.rs"),
        "pub fn alpha() {}\npub fn beta(x: u32) -> u32 { x }\nstruct Gamma;\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

/// Extract the `--why` trace from stderr. v1.10.1 tags the trace line
/// with `VEX_WHY:` (review S8.1); legacy first-`{` line behaviour kept
/// as fallback.
fn parse_trace(stderr: &str) -> serde_json::Value {
    const PREFIX: &str = "VEX_WHY:";
    if let Some(rest) = stderr
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(PREFIX))
    {
        return serde_json::from_str(rest.trim())
            .unwrap_or_else(|e| panic!("VEX_WHY trace did not parse as JSON ({e}):\n{stderr}"));
    }
    let line = stderr
        .lines()
        .find(|l| {
            l.trim_start().starts_with('{') && serde_json::from_str::<serde_json::Value>(l).is_ok()
        })
        .unwrap_or_else(|| panic!("expected JSON trace on stderr, got:\n{stderr}"));
    serde_json::from_str(line).unwrap()
}

// ── Test 1: indexed mode with keyword pattern ─────────────────────────────────

#[test]
fn why_indexed_mode_with_fn_keyword() {
    let tmp = TempDir::new().unwrap();
    write_and_index_rust_project(tmp.path());

    let mut cmd = vex_in(tmp.path());
    let assert = assert_ran(cmd.args([
        "pattern",
        "fn $NAME()",
        "--lang",
        "rust",
        "--why",
        "--format",
        "json",
    ]));

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);

    assert_eq!(
        trace["mode"].as_str().unwrap(),
        "indexed",
        "expected indexed mode after `vex index`; trace={trace}"
    );
    assert_eq!(
        trace["root_kind_inferred"].as_str().unwrap(),
        "function_item",
        "fn keyword must infer function_item; trace={trace}"
    );
    // fallback_reason must be absent (null) in indexed mode.
    assert!(
        trace["fallback_reason"].is_null(),
        "no fallback reason expected in indexed mode; trace={trace}"
    );
}

// ── Test 2: live-scan fallback when `--no-pattern-index` was used ─────────────

#[test]
fn why_live_scan_when_skeleton_section_empty() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(tmp.path().join("lib.rs"), "fn alpha() {}\n").unwrap();

    // Build an index that deliberately skips the skeleton section.
    vex_in(tmp.path())
        .args(["index", "--no-pattern-index"])
        .assert()
        .success();

    let assert = vex_in(tmp.path())
        .args([
            "pattern",
            "fn $NAME()",
            "--lang",
            "rust",
            "--why",
            "--format",
            "json",
        ])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);

    assert_eq!(
        trace["mode"].as_str().unwrap(),
        "live_scan",
        "expected live_scan when skeleton section is empty; trace={trace}"
    );
    assert_eq!(
        trace["fallback_reason"].as_str().unwrap(),
        "empty-section",
        "fallback_reason must be empty-section; trace={trace}"
    );
}

// ── Test 4 (post-review): partial-section after `vex update` ──────────────────
//
// After `vex update`, the skeleton section only carries records for re-parsed
// files. The indexed prefilter would silently drop unchanged files, so
// scan_with_mode must degrade to live-scan with reason `"partial-section"`.

#[test]
fn why_partial_section_after_update_falls_back_to_live_scan() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(dir.join("a.rs"), "fn alpha() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn beta() {}\n").unwrap();
    vex_in(dir).args(["index"]).assert().success();

    // Sanity: full-rebuild path uses indexed mode.
    let pre = vex_in(dir)
        .args([
            "pattern",
            "fn $NAME()",
            "--lang",
            "rust",
            "--why",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let trace_pre = parse_trace(&String::from_utf8_lossy(&pre.get_output().stderr));
    assert_eq!(trace_pre["mode"].as_str().unwrap(), "indexed");

    // Touch only one file, run update — partial section state.
    std::fs::write(dir.join("a.rs"), "fn alpha() {}\nfn gamma() {}\n").unwrap();
    vex_in(dir).args(["update"]).assert().success();

    let post = vex_in(dir)
        .args([
            "pattern",
            "fn $NAME()",
            "--lang",
            "rust",
            "--why",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&post.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);
    assert_eq!(
        trace["mode"].as_str().unwrap(),
        "live_scan",
        "after vex update the section is partial — must fall back; trace={trace}"
    );
    assert_eq!(
        trace["fallback_reason"].as_str().unwrap(),
        "partial-section",
        "fallback_reason must be partial-section; trace={trace}"
    );

    // Correctness check: matches must include `beta` (the unchanged file).
    // Without the partial-section fallback the indexed prefilter would skip b.rs.
    let stdout = String::from_utf8_lossy(&post.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let matches = envelope
        .get("results")
        .cloned()
        .unwrap_or(serde_json::json!([]));
    let paths: Vec<&str> = matches
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap_or(""))
        .collect();
    assert!(
        paths.iter().any(|p| p.contains("b.rs")),
        "unchanged file b.rs must still appear in matches after update; got {paths:?}"
    );
}

// ── Test 4b (review H): OR pattern → root_kind_inferred = null ────────────────
//
// `scan_with_mode` sets `root_kind = None` when `composite.has_or()`
// because OR disjuncts can target distinct kinds (`fn $N || struct $N`)
// and a single root-kind narrowing would silently drop one side. This
// test pins that contract end-to-end via the JSON trace.

#[test]
fn why_or_pattern_has_null_root_kind() {
    let tmp = TempDir::new().unwrap();
    write_and_index_rust_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args([
            "pattern",
            "fn $N() || struct $N",
            "--lang",
            "rust",
            "--why",
            "--format",
            "json",
        ])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);

    assert_eq!(
        trace["mode"].as_str().unwrap(),
        "indexed",
        "OR pattern still uses indexed mode; trace={trace}"
    );
    assert!(
        trace["root_kind_inferred"].is_null(),
        "OR composition must skip kind narrowing — root_kind_inferred = null; trace={trace}"
    );
}

// ── Test 5: no-keyword pattern still uses indexed mode, null root_kind ────────

#[test]
fn why_indexed_mode_no_keyword_pattern() {
    let tmp = TempDir::new().unwrap();
    write_and_index_rust_project(tmp.path());

    let mut cmd = vex_in(tmp.path());
    let assert = assert_ran(cmd.args([
        "pattern",
        "$X.then($Y)",
        "--lang",
        "rust",
        "--why",
        "--format",
        "json",
    ]));

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);

    assert_eq!(
        trace["mode"].as_str().unwrap(),
        "indexed",
        "indexed mode expected even without a leading keyword; trace={trace}"
    );
    assert!(
        trace["root_kind_inferred"].is_null(),
        "root_kind_inferred must be null for no-keyword pattern; trace={trace}"
    );
}
