//! CLI tests for `vex usages --strict` (11.1.2d).
//!
//! Until the persistent `reference_edges` section lands in 11.1.3,
//! `--strict` is a deferral knob: it MUST print a clear warning on
//! stderr that type-aware refs aren't built yet, and MUST still serve
//! results from the existing refs FST so the user isn't blocked.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn payment_processor() {}\n\nfn caller() {\n    payment_processor();\n}\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn strict_emits_warning_on_stderr() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["usages", "payment_processor", "--strict"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("type-aware refs not yet built"),
        "expected deferral warning on stderr, got: {stderr}"
    );
}

#[test]
fn strict_still_returns_results_from_legacy_fst() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["usages", "payment_processor", "--strict"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("payment_processor"),
        "strict must still serve from the legacy FST until 11.1.3 lands; stdout: {stdout}"
    );
}

#[test]
fn no_strict_does_not_print_warning() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["usages", "payment_processor"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        !stderr.contains("type-aware refs not yet built"),
        "deferral warning must only fire on --strict, got: {stderr}"
    );
}
