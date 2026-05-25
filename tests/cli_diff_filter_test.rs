//! Phase 13.7-D3: end-to-end coverage for the `--since` / `--since-branched`
//! / `--changed-only` diff-context flags on the search-shaped commands.
//!
//! Each test bootstraps a tiny git repo under a tempdir (so the index lives
//! in a project-local cache) and asserts that the flag narrows results to
//! the expected file set.

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

fn run_git(root: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git invocation");
    assert!(status.success(), "git {args:?} failed");
}

fn init_test_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.email", "t@t"]);
    run_git(tmp.path(), &["config", "user.name", "T"]);
    run_git(tmp.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    tmp
}

fn commit_all(root: &Path, msg: &str) {
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-q", "-m", msg]);
}

#[test]
fn since_restricts_search_to_files_changed_in_head() {
    let tmp = init_test_repo();
    let root = tmp.path();
    std::fs::write(root.join("foo.rs"), "pub fn target_symbol() {}\n").unwrap();
    std::fs::write(root.join("bar.rs"), "pub fn target_symbol() {}\n").unwrap();
    commit_all(root, "init");
    // Index ALL files at the "init" commit so both symbols live in the index.
    vex_in(root).args(["index"]).assert().success();

    // Modify bar only.
    std::fs::write(root.join("bar.rs"), "pub fn target_symbol() { 1; }\n").unwrap();
    commit_all(root, "edit bar");

    let assert = vex_in(root)
        .args(["search", "target_symbol", "--since", "HEAD~1"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).replace('\\', "/");
    assert!(
        stdout.contains("bar.rs"),
        "expected bar.rs in --since results, got: {stdout}"
    );
    assert!(
        !stdout.contains("foo.rs"),
        "foo.rs should be filtered out, got: {stdout}"
    );
}

#[test]
fn since_branched_uses_merge_base_with_main() {
    let tmp = init_test_repo();
    let root = tmp.path();
    std::fs::write(root.join("trunk.rs"), "pub fn target_symbol() {}\n").unwrap();
    commit_all(root, "trunk");
    run_git(root, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(root.join("feature.rs"), "pub fn target_symbol() {}\n").unwrap();
    commit_all(root, "feature");
    vex_in(root).args(["index"]).assert().success();

    let assert = vex_in(root)
        .args(["search", "target_symbol", "--since-branched"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).replace('\\', "/");
    assert!(
        stdout.contains("feature.rs"),
        "expected feature.rs in --since-branched results, got: {stdout}"
    );
    assert!(
        !stdout.contains("trunk.rs"),
        "trunk.rs should be filtered out, got: {stdout}"
    );
}

#[test]
fn changed_only_includes_unstaged_working_tree_changes() {
    let tmp = init_test_repo();
    let root = tmp.path();
    std::fs::write(root.join("foo.rs"), "pub fn target_symbol() {}\n").unwrap();
    std::fs::write(root.join("bar.rs"), "pub fn target_symbol() {}\n").unwrap();
    commit_all(root, "init");
    vex_in(root).args(["index"]).assert().success();

    // Modify bar.rs without committing — `--changed-only` should pick this up.
    std::fs::write(root.join("bar.rs"), "pub fn target_symbol() { 1; }\n").unwrap();

    let assert = vex_in(root)
        .args(["search", "target_symbol", "--changed-only"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).replace('\\', "/");
    assert!(
        stdout.contains("bar.rs"),
        "dirty bar.rs should appear, got: {stdout}"
    );
    assert!(
        !stdout.contains("foo.rs"),
        "clean foo.rs should be filtered, got: {stdout}"
    );
}

#[test]
fn changed_only_includes_untracked_files() {
    let tmp = init_test_repo();
    let root = tmp.path();
    std::fs::write(root.join("foo.rs"), "pub fn target_symbol() {}\n").unwrap();
    commit_all(root, "init");
    // Index the committed state, then drop an untracked file with the same
    // symbol name. The untracked file is in the working tree but not in the
    // index — we need to confirm `--changed-only` itself recognises the
    // file. Re-index AFTER creating the untracked file so the symbol is
    // findable, then assert the filter scopes the result set.
    std::fs::write(root.join("new.rs"), "pub fn target_symbol() {}\n").unwrap();
    vex_in(root).args(["index"]).assert().success();

    let assert = vex_in(root)
        .args(["search", "target_symbol", "--changed-only"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).replace('\\', "/");
    assert!(
        stdout.contains("new.rs"),
        "untracked new.rs should appear, got: {stdout}"
    );
    assert!(
        !stdout.contains("foo.rs"),
        "committed foo.rs should be filtered, got: {stdout}"
    );
}

#[test]
fn diff_flags_are_mutually_exclusive() {
    let tmp = init_test_repo();
    let root = tmp.path();
    std::fs::write(root.join("foo.rs"), "pub fn target_symbol() {}\n").unwrap();
    commit_all(root, "init");
    vex_in(root).args(["index"]).assert().success();

    vex_in(root)
        .args([
            "search",
            "target_symbol",
            "--since",
            "HEAD~1",
            "--changed-only",
        ])
        .assert()
        .failure();
    vex_in(root)
        .args([
            "search",
            "target_symbol",
            "--since-branched",
            "--changed-only",
        ])
        .assert()
        .failure();
    vex_in(root)
        .args([
            "search",
            "target_symbol",
            "--since",
            "HEAD~1",
            "--since-branched",
        ])
        .assert()
        .failure();
}

#[test]
fn non_git_repo_surfaces_actionable_error() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(root.join("foo.rs"), "pub fn target_symbol() {}\n").unwrap();
    vex_in(root).args(["index"]).assert().success();

    let assert = vex_in(root)
        .args(["search", "target_symbol", "--since", "HEAD~1"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("not a git repository"),
        "expected not-a-repo guidance, got: {stderr}"
    );
}

#[test]
fn diff_filter_metadata_in_json_envelope() {
    let tmp = init_test_repo();
    let root = tmp.path();
    std::fs::write(root.join("foo.rs"), "pub fn target_symbol() {}\n").unwrap();
    std::fs::write(root.join("bar.rs"), "pub fn target_symbol() {}\n").unwrap();
    commit_all(root, "init");
    vex_in(root).args(["index"]).assert().success();
    std::fs::write(root.join("bar.rs"), "pub fn target_symbol() { 1; }\n").unwrap();
    commit_all(root, "edit bar");

    let assert = vex_in(root)
        .args([
            "search",
            "target_symbol",
            "--since",
            "HEAD~1",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON envelope");
    let df = envelope
        .pointer("/_meta/diff_filter")
        .expect("diff_filter present in _meta");
    assert_eq!(df["scope"], "since");
    assert!(
        df["changed_paths"].as_u64().unwrap() >= 1,
        "changed_paths should be >= 1, got: {df}"
    );
    // `retained` is the post-diff pre-`--limit` count. We expect bar.rs to
    // be retained and foo.rs dropped.
    assert!(df["retained"].is_u64(), "retained must be u64: {df}");
    assert!(df["dropped"].is_u64(), "dropped must be u64: {df}");
}

#[test]
fn since_branched_falls_back_to_local_main_when_no_remote() {
    // No `git remote add origin ...`, so `origin/main` doesn't exist.
    // The resolver should walk through to local `main`.
    let tmp = init_test_repo();
    let root = tmp.path();
    std::fs::write(root.join("trunk.rs"), "pub fn target_symbol() {}\n").unwrap();
    commit_all(root, "trunk");
    run_git(root, &["checkout", "-q", "-b", "feat"]);
    std::fs::write(root.join("feat.rs"), "pub fn target_symbol() {}\n").unwrap();
    commit_all(root, "feat");
    vex_in(root).args(["index"]).assert().success();

    let assert = vex_in(root)
        .args(["search", "target_symbol", "--since-branched"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).replace('\\', "/");
    assert!(
        stdout.contains("feat.rs"),
        "feat.rs should be retained, got: {stdout}"
    );
    assert!(
        !stdout.contains("trunk.rs"),
        "trunk.rs should be filtered, got: {stdout}"
    );
}

#[test]
fn diff_filter_applies_to_callers() {
    // `callers` is the cleanest cross-file refs verification because the
    // persistent call graph picks up unqualified `helper()` calls in plain
    // Rust without an `use` statement. `usages` would need a more involved
    // fixture; we exercise that command via the unit-level filter logic
    // anyway, and via the JSON-envelope test for `_meta`.
    let tmp = init_test_repo();
    let root = tmp.path();
    std::fs::write(
        root.join("def.rs"),
        "pub fn helper() {}\npub fn caller_a() { helper(); }\n",
    )
    .unwrap();
    std::fs::write(root.join("other.rs"), "pub fn caller_b() { helper(); }\n").unwrap();
    commit_all(root, "init");
    vex_in(root).args(["index"]).assert().success();
    std::fs::write(
        root.join("def.rs"),
        "pub fn helper() {}\npub fn caller_a() { helper(); helper(); }\n",
    )
    .unwrap();
    commit_all(root, "edit def");
    vex_in(root).args(["update"]).assert().success();

    let assert = vex_in(root)
        .args(["callers", "helper", "--since", "HEAD~1"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).replace('\\', "/");
    assert!(
        stdout.contains("def.rs"),
        "caller_a in def.rs should remain, got: {stdout}"
    );
    assert!(
        !stdout.contains("other.rs"),
        "caller_b in unchanged other.rs should be filtered, got: {stdout}"
    );
}

#[test]
fn diff_filter_applies_to_grep() {
    let tmp = init_test_repo();
    let root = tmp.path();
    std::fs::write(root.join("foo.rs"), "// TODO foo\n").unwrap();
    std::fs::write(root.join("bar.rs"), "// TODO bar\n").unwrap();
    commit_all(root, "init");
    std::fs::write(root.join("bar.rs"), "// TODO bar updated\n").unwrap();
    commit_all(root, "edit bar");

    let assert = vex_in(root)
        .args(["grep", "TODO", "--since", "HEAD~1"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).replace('\\', "/");
    assert!(stdout.contains("bar.rs"));
    assert!(
        !stdout.contains("foo.rs"),
        "foo.rs should be filtered, got: {stdout}"
    );
}
