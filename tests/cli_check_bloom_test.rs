//! T4 — `vex check` bloom-sidecar integration coverage.
//!
//! Verifies the v1.12.0 bloom wire-up: `vex index` writes
//! `index.bloom` next to `index.vex`, and `vex check` reads it as a
//! pre-filter without breaking the existing semantics. The bloom
//! pre-filter is a pure optimisation — its presence, absence, or
//! corruption must never change the answer that `check` returns,
//! only the path taken to compute it. Tests pin both axes.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env_remove("VEX_CACHE_DIR");
    cmd
}

/// Lay out a minimal Rust project + index it. Returns the path to the
/// per-project cache directory so tests can poke at sidecar files.
fn make_indexed_project(dir: &Path) -> std::path::PathBuf {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn payment_processor() {}\n\
         pub fn billing_service() {}\n\
         pub fn charge_card() {}\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
    dir.join(".vex_cache")
}

#[test]
fn index_writes_bloom_sidecar_next_to_index() {
    let tmp = TempDir::new().unwrap();
    let cache = make_indexed_project(tmp.path());
    let bloom = cache.join("index.bloom");
    assert!(
        bloom.exists(),
        "expected bloom sidecar at {}, cache contents: {:?}",
        bloom.display(),
        std::fs::read_dir(&cache)
            .map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
            .unwrap_or_default()
    );
    let bytes = std::fs::read(&bloom).unwrap();
    assert!(
        bytes.len() >= 64,
        "bloom file must contain at least the 64-byte header, got {}",
        bytes.len()
    );
    assert_eq!(
        &bytes[0..4],
        b"VEXB",
        "bloom file must start with the VEXB magic"
    );
}

#[test]
fn check_reports_present_and_absent_symbols_correctly() {
    let tmp = TempDir::new().unwrap();
    make_indexed_project(tmp.path());
    let assert = vex_in(tmp.path())
        .args([
            "check",
            "payment_processor",
            "definitely_not_a_symbol",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env: serde_json::Value = serde_json::from_str(&stdout).expect("envelope JSON");
    let results = env["results"].as_array().expect("results array");
    let map: std::collections::HashMap<String, bool> = results
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["exists"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        map.get("payment_processor"),
        Some(&true),
        "known symbol must exist: {map:?}"
    );
    assert_eq!(
        map.get("definitely_not_a_symbol"),
        Some(&false),
        "absent symbol must not exist: {map:?}"
    );
}

#[test]
fn check_works_when_bloom_sidecar_is_deleted() {
    // Bloom is an optimisation; deleting the sidecar must not change
    // the answer `vex check` returns, only the code path taken.
    let tmp = TempDir::new().unwrap();
    let cache = make_indexed_project(tmp.path());
    std::fs::remove_file(cache.join("index.bloom")).expect("bloom file should exist");

    let assert = vex_in(tmp.path())
        .args([
            "check",
            "payment_processor",
            "definitely_not_a_symbol",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = env["results"].as_array().unwrap();
    let map: std::collections::HashMap<String, bool> = results
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["exists"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(map.get("payment_processor"), Some(&true));
    assert_eq!(map.get("definitely_not_a_symbol"), Some(&false));
}

#[test]
fn check_falls_through_on_corrupt_bloom_sidecar() {
    // A corrupt sidecar must be ignored, not propagated as an error.
    let tmp = TempDir::new().unwrap();
    let cache = make_indexed_project(tmp.path());
    // Overwrite the sidecar with garbage — no VEXB magic.
    std::fs::write(cache.join("index.bloom"), vec![0xAB_u8; 128]).unwrap();

    let assert = vex_in(tmp.path())
        .args([
            "check",
            "payment_processor",
            "definitely_not_a_symbol",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = env["results"].as_array().unwrap();
    let map: std::collections::HashMap<String, bool> = results
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["exists"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(map.get("payment_processor"), Some(&true));
    assert_eq!(map.get("definitely_not_a_symbol"), Some(&false));
}

#[test]
fn vex_update_rebuilds_bloom_with_newly_added_symbol() {
    // `vex update` must rebuild the bloom from the merged symbol set
    // so a name added in a later file is reachable via `vex check`.
    // Without this, a stale bloom from the original `vex index` would
    // false-negative the new symbol and `cmd_check` would short-
    // circuit to `(name, false)` before consulting the FST.
    let tmp = TempDir::new().unwrap();
    make_indexed_project(tmp.path());
    // Add a brand-new file with a symbol that wasn't in the original
    // index. `freshly_added_symbol` is a deliberate distinctive name
    // that cannot collide with anything in `make_indexed_project`'s
    // fixture.
    std::fs::write(
        tmp.path().join("src").join("extra.rs"),
        "pub fn freshly_added_symbol() {}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["update"]).assert().success();

    let assert = vex_in(tmp.path())
        .args(["check", "freshly_added_symbol", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = env["results"].as_array().unwrap();
    assert_eq!(
        results[0]["exists"].as_bool(),
        Some(true),
        "bloom must be rebuilt by `vex update`; newly-added symbol \
         must not be false-negatived: {results:?}"
    );
}

#[test]
fn check_is_case_insensitive_with_bloom_loaded() {
    let tmp = TempDir::new().unwrap();
    make_indexed_project(tmp.path());
    // Query with a different case than the source.
    let assert = vex_in(tmp.path())
        .args(["check", "PAYMENT_PROCESSOR", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = env["results"].as_array().unwrap();
    assert_eq!(
        results[0]["exists"].as_bool(),
        Some(true),
        "uppercase query must match lowercase symbol: {results:?}"
    );
}
