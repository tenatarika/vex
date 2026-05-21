//! CLI integration tests for the v1.7/11.6 symbol metadata filters
//! (`--visibility`, `--async-only`, `--no-async`, `--static-only`,
//! `--sealed-only`). These are post-filter checks against the
//! `signature` field — pure lexical matching, no format bump.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write_mixed_visibility_project(dir: &Path) {
    // All four functions share the `proc_` prefix so the FST's exact
    // → prefix fallback returns every one of them for `vex search proc_`.
    // Each one carries a distinct signature so the metadata post-filter
    // has something to narrow.
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        dir.join("lib.rs"),
        "pub fn proc_pub_sync() {}\n\
         pub async fn proc_pub_async() {}\n\
         fn proc_priv_sync() {}\n\
         async fn proc_priv_async() {}\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

fn names_from_compact(stdout: &str) -> Vec<String> {
    // compact lines look like: "F proc_pub_sync lib.rs:1"
    stdout
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(String::from))
        .collect()
}

#[test]
fn visibility_public_keeps_only_pub_functions() {
    let tmp = TempDir::new().unwrap();
    write_mixed_visibility_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args([
            "search",
            "proc_",
            "--visibility",
            "public",
            "--format",
            "compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let names = names_from_compact(&stdout);
    assert!(
        names.contains(&"proc_pub_sync".to_string()),
        "expected pub function: {stdout}"
    );
    assert!(
        !names.contains(&"proc_priv_sync".to_string()),
        "private function should be filtered: {stdout}"
    );
}

#[test]
fn async_only_drops_synchronous_results() {
    let tmp = TempDir::new().unwrap();
    write_mixed_visibility_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "proc_", "--async-only", "--format", "compact"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let names = names_from_compact(&stdout);
    assert!(
        names.iter().all(|n| n.contains("async")),
        "expected only async-named matches: {names:?}"
    );
    assert!(
        !names.contains(&"proc_pub_sync".to_string()),
        "non-async should be filtered: {stdout}"
    );
}

#[test]
fn no_async_excludes_async_results() {
    let tmp = TempDir::new().unwrap();
    write_mixed_visibility_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "proc_", "--no-async", "--format", "compact"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let names = names_from_compact(&stdout);
    assert!(
        names.iter().all(|n| !n.contains("async")),
        "async matches should be excluded: {names:?}"
    );
}

#[test]
fn unknown_visibility_value_is_rejected() {
    let tmp = TempDir::new().unwrap();
    write_mixed_visibility_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "proc_", "--visibility", "banana"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("banana") && stderr.contains("unknown --visibility"),
        "expected helpful error for unknown visibility: {stderr}"
    );
}

#[test]
fn combined_visibility_and_async_filters_with_and() {
    let tmp = TempDir::new().unwrap();
    write_mixed_visibility_project(tmp.path());

    // Only `proc_pub_async` is both pub AND async.
    let assert = vex_in(tmp.path())
        .args([
            "search",
            "proc_",
            "--visibility",
            "public",
            "--async-only",
            "--format",
            "compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let names = names_from_compact(&stdout);
    assert_eq!(
        names,
        vec!["proc_pub_async".to_string()],
        "expected only proc_pub_async: {stdout}"
    );
}
