//! H5-full contract pin (S1 review): every CLI subcommand that emits
//! `--format json` MUST wrap its results in the Phase 13 envelope with
//! `protocol_version: "v1"` at the top level. Pre-H5-full, the bare
//! arrays / objects emitted by ~14 subcommands silently broke
//! agent-side parsers that learned the envelope contract from `search`.
//!
//! Each test runs the subcommand against a tiny fixture project,
//! parses stdout as a JSON envelope, and asserts the top-level
//! `protocol_version` field equals `"v1"`. We do not assert anything
//! about the shape of `results` here — the per-subcommand tests pin
//! that.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-cache"));
    cmd
}

/// Build the tiniest possible Rust project that produces a non-empty
/// index. One file, one function, one struct — enough for `search`,
/// `usages`, `pattern`, `show`, `outline`, `check`, `grep`, `status`,
/// `similar`, `duplicates`, and the callgraph commands to return
/// something.
fn write_min_project(root: &Path) {
    std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src").join("lib.rs"),
        "pub struct Foo;\npub fn bar() -> Foo { Foo }\npub fn caller() { bar(); }\n",
    )
    .unwrap();
    // Index the project once; per-subcommand calls reuse this index.
    Command::cargo_bin("vex")
        .unwrap()
        .current_dir(root)
        .env("VEX_CACHE_DIR", root.join(".vex-cache"))
        .args(["index"])
        .assert()
        .success();
}

/// Run `vex <args>` with `--format json` and assert the envelope
/// contract: stdout parses as JSON; the root object has
/// `protocol_version == "v1"`.
#[track_caller]
fn assert_envelope(root: &Path, args: &[&str]) {
    let out = vex_in(root)
        .args(args)
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let env: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("`vex {args:?}` stdout was not valid JSON: {e}\n---\n{stdout}"));
    assert_eq!(
        env.get("protocol_version").and_then(|v| v.as_str()),
        Some("v1"),
        "`vex {args:?}` must emit envelope with protocol_version=\"v1\"; got: {env}"
    );
}

#[test]
fn search_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["search", "bar", "--format", "json"]);
}

#[test]
fn usages_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["usages", "bar", "--format", "json"]);
}

#[test]
fn pattern_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(
        tmp.path(),
        &["pattern", "fn $N()", "--lang", "rust", "--format", "json"],
    );
}

#[test]
fn show_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["show", "bar", "--format", "json"]);
}

#[test]
fn outline_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["outline", "src/lib.rs", "--format", "json"]);
}

#[test]
fn grep_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["grep", "bar", "--format", "json"]);
}

#[test]
fn status_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["status", "--format", "json"]);
}

#[test]
fn check_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["check", "bar", "--format", "json"]);
}

#[test]
fn callers_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["callers", "bar", "--format", "json"]);
}

#[test]
fn callees_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["callees", "caller", "--format", "json"]);
}

#[test]
fn paths_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["paths", "caller", "bar", "--format", "json"]);
}

#[test]
fn reachable_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["reachable", "bar", "--format", "json"]);
}

#[test]
fn implementations_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    assert_envelope(tmp.path(), &["implementations", "Foo", "--format", "json"]);
}

#[test]
fn diff_emits_envelope_v1() {
    let tmp = TempDir::new().unwrap();
    write_min_project(tmp.path());
    // Initialize git and commit the file so `vex diff` has a `HEAD` to
    // diff against.
    std::process::Command::new("git")
        .current_dir(tmp.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(tmp.path())
        .args(["config", "user.email", "test@example.com"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(tmp.path())
        .args(["config", "user.name", "T"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(tmp.path())
        .args(["add", "."])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(tmp.path())
        .args(["commit", "-qm", "initial"])
        .status()
        .unwrap();
    assert_envelope(tmp.path(), &["diff", "--base", "HEAD", "--format", "json"]);
}
