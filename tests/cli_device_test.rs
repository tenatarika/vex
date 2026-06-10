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
    cmd.env_remove("VEX_EMBEDDER");
    // The envelope opt-out would break the JSON-shape assertions below.
    cmd.env_remove("VEX_JSON_ENVELOPE");
    cmd
}

/// Run `vex gpu [extra args] --format json` and return the parsed envelope.
/// Panics (failing the test) if stdout is not a single JSON document — the
/// contract MCP agents rely on (no raw progress/diagnostic lines mixed in).
fn gpu_json(dir: &Path, extra: &[&str]) -> serde_json::Value {
    let assert = vex_in(dir)
        .arg("gpu")
        .args(extra)
        .args(["--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not a single JSON document: {e}\n---\n{stdout}"))
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
fn status_json_reports_gpu_fields_with_and_without_index() {
    // MCP agents gate on the JSON envelope (e.g. decide whether to pass
    // --gpu) — both status branches must carry the compile-time GPU fields.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();

    let parse = |bytes: &[u8]| -> serde_json::Value {
        let stdout = String::from_utf8(bytes.to_vec()).unwrap();
        serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("stdout is not a single JSON document: {e}\n{stdout}"))
    };

    // Before the first index: the agent decides --gpu for `vex index` here.
    let assert = vex_in(tmp.path())
        .args(["status", "--format", "json"])
        .assert()
        .success();
    let env = parse(&assert.get_output().stdout);
    assert_eq!(env["results"]["default_device"], serde_json::json!("cpu"));
    assert!(env["results"]["gpu_support"].is_string());

    // After an index exists: same fields in the full report.
    vex_in(tmp.path()).arg("index").assert().success();
    let assert = vex_in(tmp.path())
        .args(["status", "--format", "json"])
        .assert()
        .success();
    let env = parse(&assert.get_output().stdout);
    assert_eq!(env["results"]["default_device"], serde_json::json!("cpu"));
    let support = env["results"]["gpu_support"].as_str().unwrap();
    assert!(support.starts_with("no"), "CPU build, got {support}");
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

#[test]
fn gpu_doctor_enable_without_gpu_states_nothing_pinned() {
    // On a CPU build, `vex gpu --enable` must explicitly say it pinned nothing
    // rather than silently dropping --enable.
    let tmp = TempDir::new().unwrap();
    vex_in(tmp.path())
        .args(["gpu", "--enable"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "no GPU support in this build to pin",
        ));
}

#[test]
fn gpu_doctor_specific_uncompiled_device_reports_clearly() {
    // Asking for a specific EP the binary wasn't built with reports that
    // plainly (mirrors the index path's hard error for `--device cuda`).
    let tmp = TempDir::new().unwrap();
    vex_in(tmp.path())
        .args(["gpu", "cuda"])
        .assert()
        .success()
        .stdout(predicates::str::contains("was not built with cuda support"));
}

#[test]
fn gpu_doctor_format_json_emits_single_envelope() {
    // The default CPU build (what CI compiles) has no EP baked in, so the
    // payload shape for the "nothing to probe" branch is deterministic.
    let tmp = TempDir::new().unwrap();
    let env = gpu_json(tmp.path(), &[]);
    // Standard MetaEnvelope wrapper, same as every other --format json command.
    assert!(env.get("protocol_version").is_some(), "envelope: {env}");
    assert!(env.get("capabilities").is_some(), "envelope: {env}");
    assert!(env.get("_meta").is_some(), "envelope: {env}");
    let r = &env["results"];
    assert_eq!(r["compiled"], serde_json::json!([]));
    assert_eq!(r["probes"], serde_json::json!([]));
    assert_eq!(r["engaged"], serde_json::Value::Null);
    assert_eq!(r["pinned"], serde_json::json!(false));
    assert_eq!(r["default_device"], serde_json::json!("cpu"));
    let build = r["build"].as_str().unwrap();
    assert!(build.starts_with("no"), "CPU build, got build={build}");
    let note = r["note"].as_str().unwrap();
    assert!(
        note.contains("no GPU execution provider compiled in"),
        "note: {note}"
    );
}

#[test]
fn gpu_doctor_format_json_enable_acknowledges_nothing_pinned() {
    // `--enable` on a CPU build: the JSON note must carry the explicit
    // "nothing to pin" acknowledgement — `pinned: false` alone doesn't say why.
    let tmp = TempDir::new().unwrap();
    let env = gpu_json(tmp.path(), &["--enable"]);
    let r = &env["results"];
    assert_eq!(r["pinned"], serde_json::json!(false));
    let note = r["note"].as_str().unwrap();
    assert!(
        note.contains("no GPU support in this build to pin"),
        "note: {note}"
    );
}

#[test]
fn gpu_doctor_format_json_uncompiled_device_notes_missing_support() {
    let tmp = TempDir::new().unwrap();
    let env = gpu_json(tmp.path(), &["cuda"]);
    let r = &env["results"];
    assert_eq!(r["engaged"], serde_json::Value::Null);
    let note = r["note"].as_str().unwrap();
    assert!(
        note.contains("was not built with cuda support"),
        "note: {note}"
    );
}
