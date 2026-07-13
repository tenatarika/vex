//! `--config` / `$VEX_CONFIG` external-config override (feature A).
//!
//! Drives the real `vex` binary via assert_cmd in a fresh process — the only
//! way to exercise the process-wide `CONFIG_OVERRIDE` OnceLock, which a unit
//! test can't set twice. Proves the override loads an EXTERNAL file (not the
//! repo's `.vex.toml`), fails loud on a bad path, honours `$VEX_CONFIG`, and
//! keeps the repo clean.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

/// A vex invocation with an isolated cache so the test never touches the
/// real platform cache (mirrors `cli_bootstrap_test::vex_in`).
fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

#[test]
fn config_flag_missing_path_is_a_hard_error() {
    let repo = TempDir::new().unwrap();
    std::fs::write(repo.path().join("a.rs"), "fn foo() {}\n").unwrap();
    let missing = repo.path().join("nope").join("absent.toml");

    let assert = vex_in(repo.path())
        .args(["--config", missing.to_str().unwrap(), "check", "foo"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("absent.toml"),
        "error should name the missing config path, got: {stderr}"
    );
}

#[test]
fn config_flag_pointing_at_a_directory_is_a_hard_error() {
    let repo = TempDir::new().unwrap();
    std::fs::write(repo.path().join("a.rs"), "fn foo() {}\n").unwrap();
    let dir = repo.path().join("adir");
    std::fs::create_dir(&dir).unwrap();

    let assert = vex_in(repo.path())
        .args(["--config", dir.to_str().unwrap(), "check", "foo"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("not a readable file") && stderr.contains("adir"),
        "directory config path should give a crisp not-a-file error, got: {stderr}"
    );
}

#[test]
fn config_flag_loads_the_external_file_and_parse_errors_point_at_it() {
    // Repo has NO `.vex.toml`; the external config has an unknown field, which
    // `deny_unknown_fields` rejects — proving the EXTERNAL file was loaded and
    // parsed (a default/no-config run would succeed).
    let repo = TempDir::new().unwrap();
    std::fs::write(repo.path().join("a.rs"), "fn foo() {}\n").unwrap();
    let ext_dir = TempDir::new().unwrap();
    let ext = ext_dir.path().join("external.toml");
    std::fs::write(&ext, "definitely_not_a_real_field = 1\n").unwrap();

    let assert = vex_in(repo.path())
        .args(["--config", ext.to_str().unwrap(), "check", "foo"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("external.toml") || stderr.contains("parse"),
        "parse error should reference the external config, got: {stderr}"
    );
    assert!(
        !repo.path().join(".vex.toml").exists(),
        "the override must not create a .vex.toml in the repo"
    );
}

#[test]
fn valid_external_config_runs_and_leaves_the_repo_clean() {
    let repo = TempDir::new().unwrap();
    std::fs::write(repo.path().join("a.rs"), "fn foo() {}\n").unwrap();
    let ext_dir = TempDir::new().unwrap();
    let ext = ext_dir.path().join("external.toml");
    // Valid, minimal external config living entirely outside the repo.
    std::fs::write(&ext, "local_cache = true\n").unwrap();

    vex_in(repo.path())
        .args(["--config", ext.to_str().unwrap(), "index"])
        .assert()
        .success();

    assert!(
        !repo.path().join(".vex.toml").exists(),
        "repo must stay clean — no in-tree .vex.toml"
    );
}

#[test]
fn vex_config_env_var_is_honoured() {
    // Same external-load proof, but via `$VEX_CONFIG` (no `--config` flag) —
    // exercises the env branch of `resolve_config_override`.
    let repo = TempDir::new().unwrap();
    std::fs::write(repo.path().join("a.rs"), "fn foo() {}\n").unwrap();
    let ext_dir = TempDir::new().unwrap();
    let ext = ext_dir.path().join("external.toml");
    std::fs::write(&ext, "definitely_not_a_real_field = 1\n").unwrap();

    let assert = vex_in(repo.path())
        .env("VEX_CONFIG", &ext)
        .args(["check", "foo"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("external.toml") || stderr.contains("parse"),
        "env-var config parse error should reference the external file, got: {stderr}"
    );
}
