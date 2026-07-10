//! VCS-BACKENDS Phase 2 — `--vcs` / `VEX_VCS` / `.vex.toml vcs` override
//! precedence and the `none` floor, exercised end-to-end.
//!
//! These MUST be subprocess (integration) tests: the override lives in a
//! process-global `OnceLock` (`vcs::detect::VCS_OVERRIDE`) with no reset, so
//! precedence cannot be proven from in-process unit tests (they share one test
//! binary). Each `vex` invocation here is a fresh process with a fresh lock.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

fn run_git(root: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git invocation");
    assert!(status.success(), "git {args:?} failed");
}

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    // Never inherit an ambient override from the dev/CI shell — each test sets
    // exactly the tiers it means to exercise.
    cmd.env_remove("VEX_VCS");
    cmd
}

/// A git repo with two commits (so `HEAD~1` exists and `alpha.rs` is in the
/// `HEAD~1..HEAD` change set), an index built, and a `.vex.toml` carrying the
/// given trailing lines (e.g. a `vcs = "..."` key).
fn seed_repo(dir: &Path, extra_toml: &str) {
    run_git(dir, &["init", "-q", "-b", "main"]);
    run_git(dir, &["config", "user.email", "t@t"]);
    run_git(dir, &["config", "user.name", "T"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join(".vex.toml"), format!("local_cache = true\n{extra_toml}")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/alpha.rs"), "pub fn alpha_handler() {}\n").unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "init"]);
    // Second commit edits alpha.rs so it lands in HEAD~1..HEAD.
    std::fs::write(dir.join("src/alpha.rs"), "pub fn alpha_handler() { 1; }\n").unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "edit"]);
    vex_in(dir).args(["index"]).assert().success();
}

/// Config `vcs = "none"` (beating marker auto-detect) disables diff-scoping:
/// the `--since` search fails with the git-only decline message.
#[test]
fn config_vcs_none_disables_diff_scoping() {
    let tmp = TempDir::new().unwrap();
    seed_repo(tmp.path(), "vcs = \"none\"\n");
    vex_in(tmp.path())
        .args(["search", "alpha", "--since", "HEAD~1"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not a git repository"));
}

/// `VEX_VCS=git` overrides `.vex.toml vcs = "none"` → diff-scoping works and
/// the query (whose file changed in HEAD~1..HEAD) returns a result.
#[test]
fn env_vcs_overrides_config_none() {
    let tmp = TempDir::new().unwrap();
    seed_repo(tmp.path(), "vcs = \"none\"\n");
    vex_in(tmp.path())
        .env("VEX_VCS", "git")
        .args(["search", "alpha_handler", "--since", "HEAD~1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("alpha_handler"));
}

/// `--vcs git` (flag) overrides `VEX_VCS=none` (env) → diff-scoping works.
/// Proves the top tier beats the env tier.
#[test]
fn flag_vcs_overrides_env_none() {
    let tmp = TempDir::new().unwrap();
    seed_repo(tmp.path(), "");
    vex_in(tmp.path())
        .env("VEX_VCS", "none")
        .args(["search", "alpha_handler", "--since", "HEAD~1", "--vcs", "git"])
        .assert()
        .success()
        .stdout(predicates::str::contains("alpha_handler"));
}

/// `--vcs none` in a real git repo forces the floor: diff-scoping declines.
#[test]
fn flag_vcs_none_forces_floor_in_git_repo() {
    let tmp = TempDir::new().unwrap();
    seed_repo(tmp.path(), "");
    vex_in(tmp.path())
        .args(["search", "alpha", "--since", "HEAD~1", "--vcs", "none"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not a git repository"));
}

/// `--vcs svn` (no backend yet) declines with the honest "not yet available"
/// message rather than the generic no-repo one.
#[test]
fn flag_vcs_svn_reports_not_yet_available() {
    let tmp = TempDir::new().unwrap();
    seed_repo(tmp.path(), "");
    vex_in(tmp.path())
        .args(["search", "alpha", "--changed-only", "--vcs", "svn"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("svn backend is not yet available"));
}
