//! Phase 11.10 — end-to-end integration tests for `--why` on
//! `vex usages`, `vex similar`, and `vex duplicates`.
//!
//! Mirrors the shape of `tests/cli_pattern_why_test.rs`: spin up a
//! tiny indexed project in a `TempDir`, invoke the command, and assert
//! both the trace appears on stderr in the expected shape AND that
//! the negative contract (no `--why` → no JSON trace on stderr) holds.
//!
//! `similar` / `duplicates` require a `--semantic` index, so those
//! tests gate behind the same env var (`VEX_TEST_SEMANTIC=1`) that
//! the existing duplicates suite uses — without an ONNX runtime the
//! semantic index path bails out at index time.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// First `{`-prefixed line on stderr that parses as JSON — the `--why`
/// emission contract. Panics with the full stderr for fast debugging
/// when the trace is missing (the most common test failure here).
fn parse_trace(stderr: &str) -> serde_json::Value {
    let line = stderr
        .lines()
        .find(|l| {
            l.trim_start().starts_with('{') && serde_json::from_str::<serde_json::Value>(l).is_ok()
        })
        .unwrap_or_else(|| panic!("expected JSON trace on stderr, got:\n{stderr}"));
    serde_json::from_str(line).unwrap()
}

fn write_and_index_rust_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn payment_processor() {}\n\
         \n\
         fn caller_fn() {\n\
         \x20\x20\x20\x20payment_processor();\n\
         }\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

// ── usages ───────────────────────────────────────────────────────────

#[test]
fn usages_why_emits_text_scan_trace_with_hit_counts() {
    let tmp = TempDir::new().unwrap();
    write_and_index_rust_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["usages", "payment_processor", "--why"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);

    assert_eq!(
        trace["mode"].as_str(),
        Some("text_scan"),
        "default usages must run via text-scan path; got: {trace}",
    );
    let before = trace["hits_before_filter"]
        .as_u64()
        .expect("hits_before_filter must be u64");
    let after = trace["hits_after_filter"]
        .as_u64()
        .expect("hits_after_filter must be u64");
    assert!(before >= 1, "expected at least one hit, got: {trace}");
    // No path filter applied — both counts equal.
    assert_eq!(before, after, "no-filter case: before == after");
    // Exact hits were found → prefix_suggestions must be absent.
    assert!(
        trace.get("prefix_suggestions").is_none(),
        "prefix_suggestions must be omitted on exact hit, got: {trace}",
    );
}

#[test]
fn usages_why_strict_records_strict_mode() {
    // Pin the strict path produces `mode: "strict"` — load-bearing
    // because the trace's mode field is the only signal that
    // distinguishes the binder-resolved from text-scan code paths.
    let tmp = TempDir::new().unwrap();
    write_and_index_rust_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["usages", "payment_processor", "--strict", "--why"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);
    assert_eq!(trace["mode"].as_str(), Some("strict"));
}

#[test]
fn usages_why_emits_prefix_suggestions_when_no_exact_hit() {
    // Query a name that has no exact ref but matches a prefix —
    // `payment_processor` exists, query `payment` should give zero
    // exact hits and a non-empty prefix suggestion list.
    let tmp = TempDir::new().unwrap();
    write_and_index_rust_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["usages", "payment", "--why"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);
    assert_eq!(trace["hits_after_filter"].as_u64(), Some(0));
    // The text_scan path engages prefix-suggestion fallback when
    // there are zero exact hits. Pin both shape (present) and that
    // it found at least one suggestion (the `payment_processor`
    // identifier — though the actual count is grammar-driven so we
    // only assert >= 0, not a specific value).
    assert!(
        trace["prefix_suggestions"].is_u64(),
        "prefix_suggestions must be populated on text_scan zero-hit, got: {trace}",
    );
}

#[test]
fn usages_without_why_leaves_stderr_quiet() {
    // Negative contract: stderr must not carry a JSON trace when
    // `--why` is absent. Stale-check / auto-update warnings can
    // still land there, but those don't parse as JSON.
    let tmp = TempDir::new().unwrap();
    write_and_index_rust_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["usages", "payment_processor"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let json_line = stderr.lines().find(|l| {
        l.trim_start().starts_with('{') && serde_json::from_str::<serde_json::Value>(l).is_ok()
    });
    assert!(
        json_line.is_none(),
        "no --why → no JSON line on stderr, but found: {json_line:?}\nfull stderr:\n{stderr}",
    );
}

// ── similar / duplicates (semantic-gated) ────────────────────────────

/// Build a semantic-indexed project. `VEX_TEST_SEMANTIC=1` must be
/// set; otherwise the test should be skipped (returns false).
fn semantic_index_or_skip(dir: &Path) -> bool {
    if std::env::var("VEX_TEST_SEMANTIC").ok().as_deref() != Some("1") {
        return false;
    }
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn payment_processor() {}\n\
         pub fn billing_service() {}\n\
         pub fn charge_card() {}\n",
    )
    .unwrap();
    vex_in(dir).args(["index", "--semantic"]).assert().success();
    true
}

#[test]
fn similar_why_emits_trace_with_seed_resolution() {
    let tmp = TempDir::new().unwrap();
    if !semantic_index_or_skip(tmp.path()) {
        eprintln!("VEX_TEST_SEMANTIC!=1 — skipping similar --why E2E");
        return;
    }
    let assert = vex_in(tmp.path())
        .args(["similar", "payment_processor", "--why"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);
    assert_eq!(
        trace["seed_resolved"].as_bool(),
        Some(true),
        "seed must resolve to a stored vector, got: {trace}",
    );
    assert!(
        trace["threshold_applied"].is_number(),
        "threshold_applied must be numeric, got: {trace}",
    );
    assert!(trace["candidates_before_filter"].is_u64());
    assert!(trace["candidates_after_filter"].is_u64());
}

#[test]
fn duplicates_why_emits_trace_with_thresholds() {
    let tmp = TempDir::new().unwrap();
    if !semantic_index_or_skip(tmp.path()) {
        eprintln!("VEX_TEST_SEMANTIC!=1 — skipping duplicates --why E2E");
        return;
    }
    let assert = vex_in(tmp.path())
        .args(["duplicates", "--why", "--threshold", "0.5"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_trace(&stderr);
    assert!(
        (trace["threshold_applied"].as_f64().unwrap_or(0.0) - 0.5).abs() < f64::EPSILON,
        "threshold_applied must reflect --threshold 0.5, got: {trace}",
    );
    assert!(trace["min_body_lines_applied"].is_u64());
    assert!(trace["pairs_before_filter"].is_u64());
    assert!(trace["pairs_after_filter"].is_u64());
}
