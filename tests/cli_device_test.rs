//! GPU device-selection CLI surface (docs/GPU_SUPPORT.md). These tests assert
//! the flag *plumbing* — clap mutual-exclusivity, invalid-device rejection,
//! and the `vex status` device line — not GPU execution (CI runners have no
//! GPU, and the default CPU build has no EP compiled in anyway).

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    // Keep tests deterministic regardless of the developer's environment.
    cmd.env_remove("VEX_DEVICE");
    cmd
}

#[test]
fn index_rejects_gpu_and_no_gpu_together() {
    let tmp = TempDir::new().unwrap();
    vex_in(tmp.path())
        .args(["index", "--gpu", "--no-gpu"])
        .assert()
        .failure() // clap conflicts_with → exit 2, no indexing happens
        .stderr(predicates::str::contains("cannot be used with"));
}

#[test]
fn index_rejects_device_with_gpu() {
    let tmp = TempDir::new().unwrap();
    vex_in(tmp.path())
        .args(["index", "--gpu", "--device", "cpu"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}

#[test]
fn update_rejects_gpu_and_no_gpu_together() {
    let tmp = TempDir::new().unwrap();
    vex_in(tmp.path())
        .args(["update", "--gpu", "--no-gpu"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}

#[test]
fn index_rejects_unknown_device() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
    // `--device bogus` parses at clap (any string) but fails in Device::parse.
    vex_in(tmp.path())
        .args(["index", "--device", "bogus"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown device"));
}

#[test]
fn status_reports_gpu_support_line() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
    // Non-semantic index — fast, offline, no model download.
    vex_in(tmp.path()).arg("index").assert().success();

    vex_in(tmp.path())
        .arg("status")
        .assert()
        .success()
        // Default CPU build: "GPU: no (none compiled) · default cpu".
        .stdout(predicates::str::contains("GPU:"));
}

#[test]
fn gpu_doctor_runs_without_index() {
    // `vex gpu` is a pure diagnostic — no index, no .vex.toml, no cwd setup.
    let tmp = TempDir::new().unwrap();
    vex_in(tmp.path())
        .arg("gpu")
        .assert()
        .success()
        .stdout(predicates::str::contains("GPU diagnostics"))
        // The default CPU build (what CI compiles) has no EP baked in, so the
        // doctor takes the "how to get a GPU build" branch instead of probing.
        .stdout(predicates::str::contains(
            "no GPU execution provider compiled in",
        ));
}
