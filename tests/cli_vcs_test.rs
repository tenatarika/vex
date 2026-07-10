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
    std::fs::write(
        dir.join(".vex.toml"),
        format!("local_cache = true\n{extra_toml}"),
    )
    .unwrap();
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
        .args([
            "search",
            "alpha_handler",
            "--since",
            "HEAD~1",
            "--vcs",
            "git",
        ])
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

/// Phase 5A: the blob-SHA parse cache is routed through the VCS backend, so
/// `--vcs none` disables it. Indexing must still succeed via the xxh3/mtime
/// fallback and produce a searchable index — a correctness-neutral speed
/// degradation, never a break. (Covers the reviewers' end-to-end gap for the
/// `--vcs`-disables-blob-cache behavior change.)
#[test]
fn index_with_vcs_none_still_indexes_via_xxh3_fallback() {
    let tmp = TempDir::new().unwrap();
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.email", "t@t"]);
    run_git(tmp.path(), &["config", "user.name", "T"]);
    run_git(tmp.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/alpha.rs"),
        "pub fn alpha_handler() {}\n",
    )
    .unwrap();
    run_git(tmp.path(), &["add", "-A"]);
    run_git(tmp.path(), &["commit", "-q", "-m", "init"]);

    // Index with the git blob cache forcibly disabled by the backend override.
    vex_in(tmp.path())
        .args(["index", "--vcs", "none"])
        .assert()
        .success();
    // The index is complete and searchable despite the disabled blob cache.
    vex_in(tmp.path())
        .args(["check", "alpha_handler"])
        .assert()
        .success()
        .stdout(predicates::str::contains("alpha_handler"));
}

/// `--vcs svn` routes to `SvnVcs`, which shells out to `svn`. In a directory
/// that isn't an svn working copy (here a git repo), diff-scope resolution must
/// fail *gracefully* with an svn-specific error — NOT fall back to git, NOT
/// return an empty set, NOT panic. Holds whether or not `svn` is installed: if
/// present, `svn info` errors with `E155007`; if absent, the spawn fails — both
/// messages name `svn`.
#[test]
fn flag_vcs_svn_routes_to_svnvcs_and_fails_gracefully() {
    let tmp = TempDir::new().unwrap();
    seed_repo(tmp.path(), "");
    vex_in(tmp.path())
        .args(["search", "alpha", "--changed-only", "--vcs", "svn"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("svn"));
}

/// `--vcs arc` routes to the (provisional) `ArcVcs`, which shells out to `arc`.
/// On a machine without `arc` (the common case, incl. this CI) the diff-scope
/// resolution must fail *gracefully* with an arc-specific error — NOT fall back
/// to git, NOT return an empty set, NOT panic. This verifies the Phase-3 wiring
/// end-to-end regardless of whether `arc` is installed (if it IS present but
/// the dir isn't an arc checkout, `arc root` still errors with "arc" in it).
#[test]
fn flag_vcs_arc_routes_to_arcvcs_and_fails_gracefully_without_arc() {
    let tmp = TempDir::new().unwrap();
    seed_repo(tmp.path(), "");
    vex_in(tmp.path())
        .args(["search", "alpha", "--changed-only", "--vcs", "arc"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("arc"));
}
