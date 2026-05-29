//! FU-5 integration tests (v1.10.1): `vex status --coverage` surfaces a
//! per-language breakdown plus a `discovered_not_indexed` bucket with
//! actionable reasons for every file the walker found but the indexer
//! filtered out.
//!
//! Two scenarios:
//!   1. Mixed-language project with an unsupported extension and a
//!      brand-new not-yet-indexed source file — assert by-language
//!      counts and that the unindexed file lands in the right bucket
//!      with reason `unsupported_extension` / `not_yet_indexed`.
//!   2. Deletion: index a file then `rm` it — the missing path must
//!      surface under `missing_from_disk`.

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
        .args(["status", "--coverage", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim()).expect("status --coverage --format json must be valid JSON")
}

#[test]
fn coverage_surfaces_by_language_and_unindexed_buckets() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("scripts")).unwrap();

    // Two indexable files: one Rust, one Python.
    std::fs::write(tmp.path().join("src").join("lib.rs"), "pub fn alpha() {}\n").unwrap();
    std::fs::write(
        tmp.path().join("scripts").join("util.py"),
        "def beta():\n    return 1\n",
    )
    .unwrap();
    // Unsupported extension — must surface as `unsupported_extension`.
    std::fs::write(
        tmp.path().join("config.unknownext"),
        "this is not a source file\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    // Add a brand-new source file AFTER indexing — it should appear
    // under `not_yet_indexed`.
    std::fs::write(
        tmp.path().join("src").join("fresh.rs"),
        "pub fn gamma() {}\n",
    )
    .unwrap();

    let out = read_status_json(tmp.path());
    let cov = &out["coverage"];
    assert!(
        cov.is_object(),
        "coverage block must be present when --coverage is set; got: {out}"
    );

    assert_eq!(cov["indexed_files"].as_u64(), Some(2));
    let by_lang = &cov["by_language"];
    assert_eq!(
        by_lang["rust"].as_u64(),
        Some(1),
        "expected 1 indexed rust file, got: {by_lang}"
    );
    assert_eq!(
        by_lang["python"].as_u64(),
        Some(1),
        "expected 1 indexed python file, got: {by_lang}"
    );

    let bucket = &cov["discovered_not_indexed"];
    let samples = bucket["samples"]
        .as_array()
        .expect("samples must be an array");
    let reasons: Vec<&str> = samples
        .iter()
        .filter_map(|s| s["reason"].as_str())
        .collect();
    assert!(
        reasons.contains(&"unsupported_extension"),
        "expected an unsupported_extension reason in samples; got: {samples:?}"
    );
    assert!(
        reasons.contains(&"not_yet_indexed"),
        "fresh.rs should be reported as not_yet_indexed; got: {samples:?}"
    );

    // The bucket count must be at least the number of unique reasons we
    // saw — usually equals samples.len() unless we exceeded the cap.
    assert!(
        bucket["count"].as_u64().unwrap() >= 2,
        "bucket count should reflect both unindexed files; got: {bucket}"
    );

    // Negative contract: no missing files in this scenario.
    assert_eq!(cov["missing_from_disk"]["count"].as_u64(), Some(0));
}

#[test]
fn coverage_reports_files_missing_from_disk_after_delete() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src").join("a.rs"), "pub fn keep() {}\n").unwrap();
    std::fs::write(tmp.path().join("src").join("b.rs"), "pub fn drop_me() {}\n").unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    // Remove b.rs after indexing. We deliberately do NOT re-index — the
    // diagnostic is meant to surface exactly this drift.
    std::fs::remove_file(tmp.path().join("src").join("b.rs")).unwrap();

    let out = read_status_json(tmp.path());
    let cov = &out["coverage"];
    let missing = &cov["missing_from_disk"];
    assert_eq!(missing["count"].as_u64(), Some(1));
    let samples = missing["samples"].as_array().expect("samples array");
    assert_eq!(samples.len(), 1);
    let path = samples[0]["path"].as_str().expect("path str");
    assert!(
        path.ends_with("b.rs"),
        "missing_from_disk sample must point at the removed file; got: {path}"
    );
}

#[test]
fn coverage_block_absent_without_flag() {
    // Negative regression: omitting `--coverage` must leave the existing
    // `vex status --format json` envelope unchanged. Tools that scrape
    // `status` should not see new fields surface implicitly.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src").join("a.rs"), "pub fn alpha() {}\n").unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = vex_in(tmp.path())
        .args(["status", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let out: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        out.get("coverage").is_none(),
        "coverage block must NOT appear without --coverage; got: {out}"
    );
}
