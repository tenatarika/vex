//! CLI integration tests for `vex capabilities` (Phase 13.0).
//!
//! These tests will fail at Stage 2 because the `Capabilities` match arm
//! in `src/cli/mod.rs` is a `todo!()` that panics and produces a non-zero
//! exit code. They become GREEN in Stage 3 when the body is implemented.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write_minimal_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
}

/// Run `vex capabilities` and parse stdout as JSON, panicking on any parse
/// error or non-zero exit. Used by multiple tests.
fn run_capabilities(dir: &Path) -> serde_json::Value {
    let assert = vex_in(dir).args(["capabilities"]).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("capabilities stdout is not valid JSON: {e}\n---\n{stdout}"))
}

#[test]
fn capabilities_command_exits_zero() {
    let tmp = TempDir::new().unwrap();
    write_minimal_project(tmp.path());
    vex_in(tmp.path()).args(["capabilities"]).assert().success();
}

#[test]
fn capabilities_command_prints_protocol_version_v1() {
    let tmp = TempDir::new().unwrap();
    write_minimal_project(tmp.path());
    let out = run_capabilities(tmp.path());
    assert_eq!(
        out["protocol_version"].as_str(),
        Some("v1"),
        "expected protocol_version == \"v1\", got: {}",
        out
    );
}

#[test]
fn capabilities_command_lists_signals_as_supported() {
    let tmp = TempDir::new().unwrap();
    write_minimal_project(tmp.path());
    let out = run_capabilities(tmp.path());
    assert_eq!(
        out["capabilities"]["signals"].as_bool(),
        Some(true),
        "expected capabilities.signals == true, got: {}",
        out
    );
}

#[test]
fn capabilities_command_includes_empty_reason_false() {
    // empty_reason ships as false until Phase 13.9 — this test locks in the
    // current state and will need updating when 13.9 lands.
    let tmp = TempDir::new().unwrap();
    write_minimal_project(tmp.path());
    let out = run_capabilities(tmp.path());
    assert_eq!(
        out["capabilities"]["empty_reason"].as_bool(),
        Some(false),
        "expected capabilities.empty_reason == false (locks Phase 13.9 pre-state), got: {}",
        out
    );
}

#[test]
fn capabilities_command_advertises_structured_result_kind() {
    // PROTOCOL-EVOLUTION §4 (v1.24.0) — the `def`/`neighbor` per-result
    // marker ships in this release, so its capability flag flips true in the
    // same release that first emits the field (§2 step-2).
    let tmp = TempDir::new().unwrap();
    write_minimal_project(tmp.path());
    let out = run_capabilities(tmp.path());
    assert_eq!(
        out["capabilities"]["structured_result_kind"].as_bool(),
        Some(true),
        "expected capabilities.structured_result_kind == true, got: {}",
        out
    );
}

#[test]
fn capabilities_command_bundle_modes_lists_phase_13_2_modes() {
    // Phase 13.2 — `bundle_modes` advertises the three modes shipped by
    // the `vex bundle` subcommand. Order is locked to mirror
    // `BundleModeFlag::ALL`; downstream MCP clients may rely on it.
    let tmp = TempDir::new().unwrap();
    write_minimal_project(tmp.path());
    let out = run_capabilities(tmp.path());
    let modes: Vec<&str> = out["capabilities"]["bundle_modes"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "expected capabilities.bundle_modes to be an array, got: {}",
                out
            )
        })
        .iter()
        .map(|v| v.as_str().expect("bundle_modes entry must be a string"))
        .collect();
    assert_eq!(
        modes,
        vec!["symbol", "pr-impact", "project"],
        "capabilities.bundle_modes (Phase 13.2)"
    );
}

#[test]
fn capabilities_command_why_is_supported() {
    // Validates that the 11.10 --why capability is correctly advertised.
    let tmp = TempDir::new().unwrap();
    write_minimal_project(tmp.path());
    let out = run_capabilities(tmp.path());
    assert_eq!(
        out["capabilities"]["why"].as_bool(),
        Some(true),
        "expected capabilities.why == true (11.10 capability), got: {}",
        out
    );
}

/// v1.19.1 D3 — the dedicated `capabilities` tool must mirror its
/// capability matrix into `results`, not leave it `null`. Per the MCP
/// spec, only `structuredContent.results` (the lifted payload) is
/// guaranteed to reach the LLM; a `null` payload made the dedicated
/// capability-negotiation tool report `{"results":null}` to agents in
/// field testing even though the matrix was right there at the
/// envelope's top level. Locks the post-fix shape so future edits
/// don't silently re-null `results`.
#[test]
fn capabilities_command_results_carries_capability_matrix() {
    let tmp = TempDir::new().unwrap();
    write_minimal_project(tmp.path());
    let out = run_capabilities(tmp.path());

    // results must be a populated object, not null/empty.
    let results = out["results"]
        .as_object()
        .unwrap_or_else(|| panic!("expected `results` to be a JSON object, got: {out}"));
    assert!(
        !results.is_empty(),
        "expected `results` to carry the capability matrix; got empty object: {out}"
    );

    // The matrix in `results` must match `capabilities` exactly — same
    // shape, same booleans, same `bundle_modes` order.
    assert_eq!(
        out["results"], out["capabilities"],
        "post-fix `results` must mirror `capabilities` so the dedicated tool \
         is visible to MCP clients reading `structuredContent.results`; got: {out}"
    );

    // Spot-check load-bearing keys so a future bug that returns a
    // truncated/partial matrix in `results` still trips this test.
    assert_eq!(
        out["results"]["signals"].as_bool(),
        Some(true),
        "results.signals must mirror capabilities.signals, got: {out}"
    );
    assert!(
        out["results"]["bundle_modes"].is_array(),
        "results.bundle_modes must be an array, got: {out}"
    );
}
