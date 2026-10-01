//! P4a (`docs/V9-FORMAT.md` §4.1, §13) — `vex status` additive JSON keys
//! for the symbol-cluster section: `clusters`, `clusters_stale`,
//! `clusters_new_since_build`. Mirrors `cli_status_coverage_test.rs`'s
//! isolated-cache-dir pattern.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn read_status_json(dir: &Path) -> serde_json::Value {
    let assert = vex_in(dir)
        .args(["status", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim()).expect("status --format json must be valid JSON envelope")
}

fn results(envelope: &serde_json::Value) -> &serde_json::Value {
    envelope.get("results").expect("envelope has a results key")
}

/// A tiny but real clique so the full `vex index` pipeline actually
/// computes a non-trivial (k >= 1) cluster section, not just an empty one.
fn write_clique_fixture(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn f0() { f1(); f2(); }\n\
         pub fn f1() { f2(); }\n\
         pub fn f2() { f3(); }\n\
         pub fn f3() { f0(); }\n",
    )
    .unwrap();
}

#[test]
fn status_json_exposes_cluster_keys_after_full_index() {
    let tmp = TempDir::new().unwrap();
    write_clique_fixture(tmp.path());

    vex_in(tmp.path()).args(["index"]).assert().success();

    let envelope = read_status_json(tmp.path());
    let results = results(&envelope);
    let clusters = results
        .get("clusters")
        .and_then(|v| v.as_u64())
        .expect("results.clusters must be a number");
    assert!(
        clusters >= 1,
        "a real 4-function clique must form >= 1 cluster, got {clusters}"
    );
    assert_eq!(
        results.get("clusters_stale").and_then(|v| v.as_bool()),
        Some(false),
        "P4a never sets STALE"
    );
    assert_eq!(
        results
            .get("clusters_new_since_build")
            .and_then(|v| v.as_u64()),
        Some(0),
        "P4a never emits the NEW sentinel"
    );
}

#[test]
fn status_json_reports_zero_clusters_with_no_clusters_flag() {
    let tmp = TempDir::new().unwrap();
    write_clique_fixture(tmp.path());

    vex_in(tmp.path())
        .args(["index", "--no-clusters"])
        .assert()
        .success();

    let envelope = read_status_json(tmp.path());
    let results = results(&envelope);
    assert_eq!(
        results.get("clusters").and_then(|v| v.as_u64()),
        Some(0),
        "--no-clusters must leave the section not COMPUTED"
    );
    assert_eq!(
        results.get("clusters_stale").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        results
            .get("clusters_new_since_build")
            .and_then(|v| v.as_u64()),
        Some(0)
    );
}

#[test]
fn status_text_shows_a_clusters_line() {
    let tmp = TempDir::new().unwrap();
    write_clique_fixture(tmp.path());
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = vex_in(tmp.path()).args(["status"]).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Clusters:"),
        "text output must surface a Clusters: line, got:\n{stdout}"
    );
}
