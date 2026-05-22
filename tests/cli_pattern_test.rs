//! CLI integration tests for the v1.7/11.4 promoted `vex pattern`
//! command: metavar back-references, scope filters, empty-pattern
//! error.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write_back_ref_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        dir.join("lib.rs"),
        "fn caller() {\n    record(state, state);\n    record(state, other);\n}\n",
    )
    .unwrap();
}

#[test]
fn back_reference_pattern_enforces_same_capture() {
    let tmp = TempDir::new().unwrap();
    write_back_ref_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args([
            "pattern",
            "record($X, $X)",
            "--lang",
            "rust",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("pattern emits JSON array");
    let matches = json.as_array().expect("array");
    // The first call `record(state, state)` matches (both $X = state);
    // the second `record(state, other)` must NOT match because the
    // back-ref would force $X to be both `state` and `other`.
    let texts: Vec<&str> = matches
        .iter()
        .map(|m| m["text"].as_str().unwrap_or(""))
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("state, state")),
        "expected back-ref match for record(state, state): {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("state, other")),
        "record(state, other) must not match: {texts:?}"
    );
}

#[test]
fn empty_pattern_fails_with_helpful_error() {
    let tmp = TempDir::new().unwrap();
    write_back_ref_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["pattern", "   ", "--lang", "rust"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("empty pattern"),
        "expected empty-pattern error: {stderr}"
    );
}

#[test]
fn scope_include_filters_pattern_results() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
    std::fs::write(tmp.path().join("src").join("a.rs"), "fn src_fn() {}\n").unwrap();
    std::fs::write(tmp.path().join("tests").join("t.rs"), "fn test_fn() {}\n").unwrap();

    let assert = vex_in(tmp.path())
        .args([
            "pattern",
            "fn $NAME()",
            "--lang",
            "rust",
            "--include",
            "src/**",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    // Normalise path separators so the substring checks work on Windows
    // where vex emits backslashes (`src\a.rs`).
    let paths: Vec<String> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap_or("").replace('\\', "/"))
        .collect();
    assert!(
        paths.iter().any(|p| p.contains("src/")),
        "expected src/ match: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains("tests/")),
        "tests/ match should be filtered: {paths:?}"
    );
}
