//! CLI integration tests for the v1.7/11.5 multi-hop call-graph
//! commands (`vex paths` and `vex reachable`). Both walk the
//! persistent v4 call graph, so the tests build an index over a tiny
//! Rust project with a known chain and assert the structure of the
//! emitted JSON.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

mod common;
use common::assert_ran;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// A→X→B chain: `a` calls `mid`, `mid` calls `target`.
fn write_chain_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        dir.join("lib.rs"),
        "fn target() {}\n\
         fn mid() { target(); }\n\
         fn a() { mid(); }\n\
         fn unrelated() {}\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn paths_finds_two_hop_chain() {
    let tmp = TempDir::new().unwrap();
    write_chain_project(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args(["paths", "a", "target", "--format", "json"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("paths emits envelope");
    let json = envelope
        .get("results")
        .cloned()
        .unwrap_or(serde_json::json!([]));
    let paths = json.as_array().expect("array");
    assert!(
        !paths.is_empty(),
        "expected at least one path, got: {stdout}"
    );
    let steps = paths[0]["steps"].as_array().expect("steps array");
    let names: Vec<&str> = steps
        .iter()
        .map(|s| s["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        names,
        vec!["a", "mid", "target"],
        "expected A→X→B chain, got: {names:?}"
    );
}

#[test]
fn paths_respects_max_hops() {
    let tmp = TempDir::new().unwrap();
    write_chain_project(tmp.path());

    // 2-hop chain `a → mid → target`. max-hops=1 should miss it.
    let assert = assert_ran(vex_in(tmp.path()).args([
        "paths",
        "a",
        "target",
        "--max-hops",
        "1",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let json = envelope
        .get("results")
        .cloned()
        .unwrap_or(serde_json::json!([]));
    assert_eq!(
        json.as_array().map(|a| a.len()).unwrap_or(1),
        0,
        "max-hops=1 should hide the 2-hop chain: {stdout}"
    );
}

#[test]
fn reachable_lists_all_indirect_callers() {
    let tmp = TempDir::new().unwrap();
    write_chain_project(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args(["reachable", "target", "--format", "json"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let json = envelope
        .get("results")
        .cloned()
        .unwrap_or(serde_json::json!([]));
    let entries = json.as_array().expect("array");
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        names.contains(&"mid"),
        "expected direct caller `mid`, got: {names:?}"
    );
    assert!(
        names.contains(&"a"),
        "expected transitive caller `a`, got: {names:?}"
    );
    assert!(
        !names.contains(&"unrelated"),
        "unrelated must not appear: {names:?}"
    );

    // Depth: `mid` is direct (1), `a` is transitive (2).
    let by_name: std::collections::HashMap<&str, u64> = entries
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap_or(""),
                e["depth"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    assert_eq!(by_name.get("mid"), Some(&1));
    assert_eq!(by_name.get("a"), Some(&2));
}

#[test]
fn paths_requires_call_graph_in_index() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(tmp.path().join("lib.rs"), "fn a() {}\nfn b() { a(); }\n").unwrap();
    // Index without call graph.
    vex_in(tmp.path())
        .args(["index", "--no-call-graph"])
        .assert()
        .success();

    let assert = vex_in(tmp.path())
        .args(["paths", "b", "a"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("no call graph"),
        "expected helpful error when call graph missing, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Phase 11.5 hardening (G2): stress + scope edges
// ---------------------------------------------------------------------------

/// Three distinct chains from `a` to `target`. `--max-paths 2` must cap
/// the enumeration to 2 returned paths even though the BFS could reach 3.
fn write_fanout_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        dir.join("lib.rs"),
        "fn target() {}\n\
         fn mid_a() { target(); }\n\
         fn mid_b() { target(); }\n\
         fn mid_c() { target(); }\n\
         fn a() { mid_a(); mid_b(); mid_c(); }\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn paths_max_paths_caps_output_on_fanout() {
    let tmp = TempDir::new().unwrap();
    write_fanout_project(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args([
        "paths",
        "a",
        "target",
        "--max-paths",
        "2",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = envelope
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        results.len(),
        2,
        "--max-paths 2 must cap at 2 returned paths; got {} in: {stdout}",
        results.len()
    );

    // Sanity-check: without the cap, all 3 chains surface — pins that
    // the fan-out project really has 3 distinct paths and the cap above
    // is not a false-positive on a degenerate graph.
    let uncapped =
        assert_ran(vex_in(tmp.path()).args(["paths", "a", "target", "--format", "json"]));
    let stdout = String::from_utf8_lossy(&uncapped.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let all = envelope
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        all.len(),
        3,
        "default cap must surface all 3 chains: {stdout}"
    );
}

#[test]
fn reachable_honours_include_exclude_scope() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn target() {}\npub fn src_caller() { target(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tests").join("it.rs"),
        "fn test_caller() { super::target(); }\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();

    // --include src/** keeps src_caller, drops test_caller.
    let assert = assert_ran(vex_in(dir).args([
        "reachable",
        "target",
        "--include",
        "src/**",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let names: Vec<String> = envelope
        .get("results")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        names.iter().any(|n| n == "src_caller"),
        "--include src/** must keep src_caller; got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "test_caller"),
        "--include src/** must drop test_caller; got: {names:?}"
    );

    // --exclude src/** drops src_caller.
    let assert = vex_in(dir)
        .args([
            "reachable",
            "target",
            "--exclude",
            "src/**",
            "--format",
            "json",
        ])
        .assert();
    let code = assert.get_output().status.code();
    assert!(
        matches!(code, Some(0) | Some(1)),
        "reachable with exclude must exit 0 or 1; got {code:?}"
    );
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let names: Vec<String> = envelope
        .get("results")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !names.iter().any(|n| n == "src_caller"),
        "--exclude src/** must drop src_caller; got: {names:?}"
    );
}

#[test]
fn paths_max_paths_zero_yields_empty_envelope_exit_one() {
    let tmp = TempDir::new().unwrap();
    write_chain_project(tmp.path());

    let output = vex_in(tmp.path())
        .args([
            "paths",
            "a",
            "target",
            "--max-paths",
            "0",
            "--format",
            "json",
        ])
        .output()
        .expect("spawn vex paths");

    assert_eq!(
        output.status.code(),
        Some(1),
        "--max-paths 0 must exit 1 (empty result); got: {:?}",
        output.status.code()
    );

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        !stdout.trim().is_empty(),
        "vex paths --format json must always emit an envelope; got empty stdout"
    );
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("envelope parses");
    let results = envelope
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        results.is_empty(),
        "--max-paths 0 must produce empty results[]; got: {results:?}"
    );
}
