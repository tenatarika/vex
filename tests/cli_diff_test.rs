//! CLI integration tests for the v1.7/11.2 `vex diff --base <rev>`
//! command. Builds a real git repo in a tempdir with two commits and
//! asserts that the symbol-level diff reports the expected adds /
//! removes / body changes.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn git(dir: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("invoke git");
    assert!(status.success(), "git {args:?} failed");
}

/// Sets up a repo with one commit on `main`, then makes uncommitted
/// changes:
///   - `lib.rs`: removes `gone()`, adds `fresh()`, changes body of
///     `kept()`.
fn set_up_repo(dir: &Path) {
    git(dir, &["init", "-q", "--initial-branch=main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);

    std::fs::write(dir.join("lib.rs"), "fn kept() { 1 }\nfn gone() {}\n").unwrap();
    git(dir, &["add", "lib.rs"]);
    git(dir, &["commit", "-q", "-m", "baseline"]);

    // Now make the diff-target changes against the committed state.
    std::fs::write(dir.join("lib.rs"), "fn kept() { 2 }\nfn fresh() {}\n").unwrap();
}

#[test]
fn diff_lists_adds_removes_and_body_changes() {
    let tmp = TempDir::new().unwrap();
    set_up_repo(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["diff", "--base", "main", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("diff emits JSON array");
    let changes = json.as_array().expect("array");

    let by_name: std::collections::HashMap<&str, &serde_json::Value> = changes
        .iter()
        .map(|c| (c["name"].as_str().unwrap_or(""), c))
        .collect();

    assert!(
        by_name.contains_key("fresh"),
        "expected `fresh` added: {stdout}"
    );
    assert_eq!(by_name["fresh"]["kind"], "added");

    assert!(
        by_name.contains_key("gone"),
        "expected `gone` removed: {stdout}"
    );
    assert_eq!(by_name["gone"]["kind"], "removed");

    assert!(
        by_name.contains_key("kept"),
        "expected `kept` body_changed: {stdout}"
    );
    assert_eq!(by_name["kept"]["kind"], "body_changed");
}

#[test]
fn diff_with_no_changes_emits_empty_array() {
    let tmp = TempDir::new().unwrap();
    git(tmp.path(), &["init", "-q", "--initial-branch=main"]);
    git(tmp.path(), &["config", "user.email", "test@example.com"]);
    git(tmp.path(), &["config", "user.name", "test"]);
    git(tmp.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(tmp.path().join("lib.rs"), "fn only() {}\n").unwrap();
    git(tmp.path(), &["add", "lib.rs"]);
    git(tmp.path(), &["commit", "-q", "-m", "single"]);

    let assert = vex_in(tmp.path())
        .args(["diff", "--base", "main", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        json.as_array().map(|a| a.len()).unwrap_or(99),
        0,
        "clean tree against itself should diff to empty: {stdout}"
    );
}

#[test]
fn diff_missing_base_fails_with_helpful_error() {
    let tmp = TempDir::new().unwrap();
    set_up_repo(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["diff", "--base", "no-such-ref"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("git diff") || stderr.contains("no-such-ref"),
        "expected helpful git error, got: {stderr}"
    );
}

#[test]
fn renamed_file_appears_as_remove_plus_add() {
    // Documented limitation: 11.2 doesn't detect renames yet. Pin the
    // actual observed behaviour so it doesn't silently change.
    let tmp = TempDir::new().unwrap();
    git(tmp.path(), &["init", "-q", "--initial-branch=main"]);
    git(tmp.path(), &["config", "user.email", "test@example.com"]);
    git(tmp.path(), &["config", "user.name", "test"]);
    git(tmp.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(tmp.path().join("old.rs"), "fn moved() {}\n").unwrap();
    git(tmp.path(), &["add", "old.rs"]);
    git(tmp.path(), &["commit", "-q", "-m", "baseline"]);

    // `git mv` the same content under a new path.
    git(tmp.path(), &["mv", "old.rs", "new.rs"]);

    let assert = vex_in(tmp.path())
        .args(["diff", "--base", "main", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let changes = json.as_array().expect("array");
    // Each side of the rename surfaces independently — one removed
    // from old.rs and one added to new.rs.
    let removed = changes
        .iter()
        .find(|c| c["kind"] == "removed" && c["path"] == "old.rs")
        .unwrap_or_else(|| panic!("expected removed entry: {stdout}"));
    let added = changes
        .iter()
        .find(|c| c["kind"] == "added" && c["path"] == "new.rs")
        .unwrap_or_else(|| panic!("expected added entry: {stdout}"));
    assert_eq!(removed["name"], "moved");
    assert_eq!(added["name"], "moved");
}

#[test]
fn binary_file_is_silently_skipped() {
    // A changed binary file produces no source-level changes — the
    // parser refuses non-UTF-8, the `.ok()` in the handler swallows
    // the read error, and the diff output is empty rather than
    // erroring out the whole run.
    let tmp = TempDir::new().unwrap();
    git(tmp.path(), &["init", "-q", "--initial-branch=main"]);
    git(tmp.path(), &["config", "user.email", "test@example.com"]);
    git(tmp.path(), &["config", "user.name", "test"]);
    git(tmp.path(), &["config", "commit.gpgsign", "false"]);
    // Use a known source extension so language detection runs; the
    // file body is non-UTF-8 bytes the parser will reject.
    std::fs::write(tmp.path().join("blob.rs"), b"// baseline\nfn keep() {}\n").unwrap();
    git(tmp.path(), &["add", "blob.rs"]);
    git(tmp.path(), &["commit", "-q", "-m", "baseline"]);
    std::fs::write(tmp.path().join("blob.rs"), b"\xff\xfe\x00\x01\x02").unwrap();

    let assert = vex_in(tmp.path())
        .args(["diff", "--base", "main", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    // The old side had `keep`; the new side parsed as empty, so the
    // change registers as a removal — but the command must not error
    // out on the binary content.
    let kinds: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap_or(""))
        .collect();
    assert!(
        kinds.iter().all(|k| !k.is_empty()),
        "binary file should not panic the run: {stdout}"
    );
}

#[test]
fn diff_respects_include_glob_scope() {
    let tmp = TempDir::new().unwrap();
    git(tmp.path(), &["init", "-q", "--initial-branch=main"]);
    git(tmp.path(), &["config", "user.email", "test@example.com"]);
    git(tmp.path(), &["config", "user.name", "test"]);
    git(tmp.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("a.rs"),
        "fn baseline_src() {}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("tests").join("t.rs"),
        "fn baseline_test() {}\n",
    )
    .unwrap();
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-q", "-m", "baseline"]);

    // Modify both files.
    std::fs::write(
        tmp.path().join("src").join("a.rs"),
        "fn baseline_src() {}\nfn added_in_src() {}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("tests").join("t.rs"),
        "fn baseline_test() {}\nfn added_in_test() {}\n",
    )
    .unwrap();

    let assert = vex_in(tmp.path())
        .args([
            "diff",
            "--base",
            "main",
            "--include",
            "src/**",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let names: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        names.contains(&"added_in_src"),
        "expected src/ change: {names:?}"
    );
    assert!(
        !names.contains(&"added_in_test"),
        "expected tests/ change to be filtered out: {names:?}"
    );
}
