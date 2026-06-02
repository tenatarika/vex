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
