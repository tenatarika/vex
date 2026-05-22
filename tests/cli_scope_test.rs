//! CLI-level integration tests for the v1.7 per-query path scope
//! filters (`--include` / `--exclude`).
//!
//! Each test creates a small tempdir with files in two directories
//! (`src/` and `tests/`), indexes it under a project-local cache, and
//! exercises the scope flags across the search-shaped subcommands.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write_two_dir_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::create_dir_all(dir.join("src").join("generated")).unwrap();
    std::fs::write(
        dir.join("src").join("api.rs"),
        "pub fn payment_processor() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("generated").join("proto.rs"),
        "pub fn payment_processor() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tests").join("integration.rs"),
        "fn payment_processor() {}\n",
    )
    .unwrap();
    // Bootstrap the index up front so each individual scope assertion stays
    // focused on the filter, not on the auto-bootstrap warning.
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn include_glob_restricts_search_results() {
    let tmp = TempDir::new().unwrap();
    write_two_dir_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "payment_processor", "--include", "tests/**"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout)
        .replace('\\', "/"); // Normalise Windows path separators for substring checks.

    assert!(
        stdout.contains("tests/integration.rs"),
        "expected tests/ hit, got: {stdout}"
    );
    assert!(
        !stdout.contains("src/api.rs"),
        "src/ result should be filtered out, got: {stdout}"
    );
    assert!(
        !stdout.contains("src/generated/proto.rs"),
        "src/generated/ result should be filtered out, got: {stdout}"
    );
}

#[test]
fn exclude_glob_drops_results_even_when_included() {
    let tmp = TempDir::new().unwrap();
    write_two_dir_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args([
            "search",
            "payment_processor",
            "--include",
            "src/**",
            "--exclude",
            "**/generated/**",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout)
        .replace('\\', "/"); // Normalise Windows path separators for substring checks.

    assert!(
        stdout.contains("src/api.rs"),
        "expected src/api.rs hit, got: {stdout}"
    );
    assert!(
        !stdout.contains("src/generated/proto.rs"),
        "src/generated/ should be excluded, got: {stdout}"
    );
    assert!(
        !stdout.contains("tests/integration.rs"),
        "tests/ should fail the include filter, got: {stdout}"
    );
}

#[test]
fn multiple_include_globs_union() {
    let tmp = TempDir::new().unwrap();
    write_two_dir_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args([
            "search",
            "payment_processor",
            "--include",
            "src/api.rs",
            "--include",
            "tests/**",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout)
        .replace('\\', "/"); // Normalise Windows path separators for substring checks.

    assert!(
        stdout.contains("src/api.rs"),
        "expected src/api.rs hit, got: {stdout}"
    );
    assert!(
        stdout.contains("tests/integration.rs"),
        "expected tests/integration.rs hit, got: {stdout}"
    );
    assert!(
        !stdout.contains("src/generated/proto.rs"),
        "src/generated/ should not match either include, got: {stdout}"
    );
}

#[test]
fn invalid_glob_surfaces_error() {
    let tmp = TempDir::new().unwrap();
    write_two_dir_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "payment_processor", "--include", "src/["])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("--include") && stderr.contains("src/["),
        "expected error naming the --include glob, got: {stderr}"
    );
}

#[test]
fn scope_applies_to_grep() {
    let tmp = TempDir::new().unwrap();
    write_two_dir_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args([
            "grep",
            "payment_processor",
            "--exclude",
            "**/generated/**",
            "--exclude",
            "tests/**",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout)
        .replace('\\', "/"); // Normalise Windows path separators for substring checks.

    assert!(
        stdout.contains("src/api.rs"),
        "expected src/api.rs hit, got: {stdout}"
    );
    assert!(
        !stdout.contains("src/generated/proto.rs"),
        "generated/ should be excluded from grep, got: {stdout}"
    );
    assert!(
        !stdout.contains("tests/integration.rs"),
        "tests/ should be excluded from grep, got: {stdout}"
    );
}

#[test]
fn scope_applies_to_usages() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
    // Definition + two reference sites in different dirs.
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn payment_processor() {}\npub fn caller() { payment_processor(); }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("tests").join("it.rs"),
        "fn it_test() { payment_processor(); }\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = vex_in(tmp.path())
        .args(["usages", "payment_processor", "--exclude", "tests/**"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout)
        .replace('\\', "/"); // Normalise Windows path separators for substring checks.
    assert!(
        stdout.contains("src/lib.rs"),
        "expected src/ usage, got: {stdout}"
    );
    assert!(
        !stdout.contains("tests/it.rs"),
        "tests/ usage should be excluded, got: {stdout}"
    );
}
