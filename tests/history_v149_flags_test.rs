//! Phase 14.9 v1.16.0 — integration tests for the new `vex history`
//! CLI flags. Covers dispatch (clap parsing + arg routing into
//! `cmd_history::history`) and the end-to-end behavior of:
//!
//! - `--kind <KIND>` filter (Tier A.4)
//! - `--author <SUBSTR>` walker filter (Tier A.3)
//! - `--author` hard-error on indexed path (Tier A.3 contract)
//! - `--diff` + `--exact-presence` clap-level mutual exclusion (final review)
//! - `--exact-presence` JSON shape (Tier B.7)
//!
//! Filter logic itself is unit-tested in `src/history/filter.rs`;
//! presence resolution in `src/history/presence.rs`. These tests guard
//! the glue: a clap field rename or a dispatch-map regression that
//! silently drops one of the new flags would not be caught by either
//! unit-test layer.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env_remove("VEX_CACHE_DIR");
    cmd
}

fn git(repo: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "alice@example.com"]);
    git(dir, &["config", "user.name", "Alice Liddell"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(
        dir.join(".vex.toml"),
        "local_cache = true\nformat = \"compact\"\n",
    )
    .unwrap();
}

fn commit_as(repo: &Path, name: &str, email: &str, file: &str, content: &str, msg: &str) {
    std::fs::write(repo.join(file), content).unwrap();
    git(repo, &["add", file]);
    git(
        repo,
        &[
            "commit",
            "-q",
            "--author",
            &format!("{name} <{email}>"),
            "-m",
            msg,
        ],
    );
}

fn commit(repo: &Path, file: &str, content: &str, msg: &str) {
    std::fs::write(repo.join(file), content).unwrap();
    git(repo, &["add", file]);
    git(repo, &["commit", "-q", "-m", msg]);
}

#[test]
fn kind_filter_suppresses_partner_rows() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // `struct Foo` and `impl Foo` share the symbol name `Foo` —
    // walker returns both kinds per the 2026-06-09 dogfooding
    // observation. `--kind struct` should drop the impl partner.
    commit(
        repo,
        "lib.rs",
        "pub struct Foo {}\nimpl Foo { pub fn new() -> Self { Self {} } }\n",
        "v1",
    );

    let out = vex_in(repo)
        .args(["history", "Foo", "--no-index", "--kind", "struct"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out);
    assert!(stdout.contains("struct"), "expected `struct` row: {stdout}");
    assert!(
        !stdout.contains("\timpl\t"),
        "--kind struct must drop the impl partner row, got:\n{stdout}"
    );
}

#[test]
fn author_filter_case_insensitive_substring_walker() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // Two commits by different authors touching the same symbol.
    commit_as(
        repo,
        "Alice Liddell",
        "alice@example.com",
        "lib.rs",
        "pub fn shared() -> u8 { 1 }\n",
        "v1 by Alice",
    );
    commit_as(
        repo,
        "Bob Smith",
        "bob@example.com",
        "lib.rs",
        "pub fn shared() -> u8 { 2 }\n",
        "v2 by Bob",
    );

    let out = vex_in(repo)
        .args(["history", "shared", "--no-index", "--author", "ALICE"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out);
    // Only Alice's commit should remain; substring match is case-insensitive.
    assert_eq!(
        stdout.matches("function").count(),
        1,
        "exactly one row expected (Alice's), got:\n{stdout}"
    );
}

#[test]
fn author_on_indexed_path_errors_with_hint() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit(repo, "lib.rs", "pub fn alpha() -> u8 { 1 }\n", "v1");

    // Build the Phase 14.8 sidecar so the indexed path is selected.
    vex_in(repo).args(["index", "--history"]).assert().success();

    // `--author` on the indexed path must hard-error with a hint
    // pointing at `--no-index`.
    let assert = vex_in(repo)
        .args(["history", "alpha", "--author", "alice"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("--no-index"),
        "stderr should mention --no-index escape hatch, got:\n{stderr}"
    );
}

#[test]
fn diff_and_exact_presence_are_mutually_exclusive() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit(repo, "lib.rs", "pub fn foo() -> u8 { 1 }\n", "v1");

    // clap should reject the combination at parse time — exit
    // non-zero with a "cannot be used with" message. We exercise the
    // walker explicitly so the runtime guard inside `history()` is
    // NOT reached (clap rejects first).
    let assert = vex_in(repo)
        .args(["history", "foo", "--no-index", "--diff", "--exact-presence"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("cannot be used with"),
        "clap should reject --diff + --exact-presence at parse time, got:\n{stderr}"
    );
}

#[test]
fn exact_presence_emits_presence_field_in_json() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit(repo, "lib.rs", "pub fn alpha() -> u8 { 1 }\n", "v1 = A");
    commit(repo, "lib.rs", "pub fn alpha() -> u8 { 2 }\n", "v2 = B");
    commit(
        repo,
        "lib.rs",
        "pub fn alpha() -> u8 { 1 }\n",
        "v3 = revert to A",
    );

    let out = vex_in(repo)
        .args([
            "history",
            "alpha",
            "--no-index",
            "--exact-presence",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out);
    let envelope: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("bad JSON: {e}\n{stdout}"));
    let results = envelope
        .get("results")
        .and_then(|r| r.as_array())
        .expect("results must be a top-level array");
    assert!(
        !results.is_empty(),
        "expected at least one history entry, got: {stdout}"
    );
    // Every entry carries a `presence` object — revert pattern means
    // the v1=A entry should have presence at c1 and c3 (2 commits),
    // the v2=B entry should have presence at c2 only (1 commit).
    let totals: Vec<usize> = results
        .iter()
        .filter_map(|r| r.get("presence"))
        .filter_map(|p| p.get("commits"))
        .filter_map(|c| c.as_array())
        .map(|a| a.len())
        .collect();
    assert_eq!(
        totals.len(),
        results.len(),
        "every entry must carry a `presence` object, got results:\n{stdout}"
    );
    // The blob_sha-dedup means we should have exactly 2 entries (A
    // and B), and presence counts should be 2 and 1 in some order.
    let mut sorted = totals.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![1, 2],
        "revert pattern: presence counts must be {{1, 2}}, got {sorted:?}"
    );
}

#[test]
fn since_until_window_works_on_walker() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit(repo, "lib.rs", "pub fn alpha() -> u8 { 1 }\n", "v1");
    commit(repo, "lib.rs", "pub fn alpha() -> u8 { 2 }\n", "v2");

    // Window forced to 1970-01-01..1970-01-02 — nothing real should match.
    let out = vex_in(repo)
        .args([
            "history",
            "alpha",
            "--no-index",
            "--since",
            "1970-01-01",
            "--until",
            "1970-01-02",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out);
    // Compact format prints one line per entry; an empty window
    // should produce no rows.
    let row_count = stdout.lines().filter(|l| l.contains("function")).count();
    assert_eq!(
        row_count, 0,
        "1970 window should match nothing, got:\n{stdout}"
    );
}

#[test]
fn calendar_invalid_since_date_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit(repo, "lib.rs", "pub fn alpha() -> u8 { 1 }\n", "v1");

    // `2026-13-99` is structurally valid (length 10, two hyphens)
    // but calendar-invalid. `parse_iso_date` must reject it before
    // the filter sees it.
    let assert = vex_in(repo)
        .args(["history", "alpha", "--no-index", "--since", "2026-13-99"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("calendar-invalid") || stderr.contains("YYYY-MM-DD"),
        "calendar-invalid date should produce a clean error, got:\n{stderr}"
    );
}
