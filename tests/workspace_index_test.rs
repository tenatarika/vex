//! E2E for `vex index --workspace` (multi-repo phase 3): each member of a
//! `.vex-workspace.toml` is indexed into its own per-repo index dir, and a
//! subsequent per-member query resolves against that member's index only.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path, cache: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", cache);
    cmd
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

#[test]
fn index_workspace_indexes_each_member_into_its_own_index() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");

    write(
        &root.join("alpha").join("alpha.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(
        &root.join("beta").join("beta.rs"),
        "pub fn beta_thing() {}\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );

    // Index the whole workspace in one shot.
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    // `vex check` prints "+ name" on a hit and "- name" on a miss (exit 0
    // either way), so assert on the mark, not the exit code.
    let check = |sym: &str, member: &str| -> String {
        let out = vex_in(root, &cache)
            .args(["check", sym, "--path", member])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // Each member resolves its OWN symbol via its own index dir.
    assert!(
        check("alpha_thing", "alpha").contains("+ alpha_thing"),
        "alpha_thing should be found in member alpha"
    );
    assert!(
        check("beta_thing", "beta").contains("+ beta_thing"),
        "beta_thing should be found in member beta"
    );

    // Cross-member isolation: beta_thing is absent from member alpha's index.
    assert!(
        check("beta_thing", "alpha").contains("- beta_thing"),
        "beta_thing must not resolve in member alpha (separate indexes)"
    );
}

#[test]
fn check_workspace_reports_which_repos_define_a_symbol() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    // `alpha_thing` lives only in member alpha; the text line tags the repo.
    let out = vex_in(root, &cache)
        .args(["check", "alpha_thing", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("+ alpha_thing") && stdout.contains("alpha"),
        "alpha_thing should be found and attributed to member alpha: {stdout}"
    );
    assert!(
        !stdout.contains("beta"),
        "alpha_thing must not be attributed to member beta: {stdout}"
    );

    // A name in no member resolves to a miss.
    let miss = vex_in(root, &cache)
        .args(["check", "ghost_thing", "--workspace"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&miss.stdout).contains("- ghost_thing"),
        "absent name should print a miss"
    );
}

#[test]
fn check_workspace_json_lists_repos_and_names() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\nname = \"A\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["check", "alpha_thing", "--workspace", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"A\""),
        "json should name member A: {stdout}"
    );
    assert!(
        stdout.contains("alpha_thing") && stdout.contains("\"exists\""),
        "json should carry the name + exists flag: {stdout}"
    );
}

#[test]
fn search_workspace_groups_results_by_repo() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["search", "alpha_thing", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Both members get a section header; the hit lands under alpha.
    assert!(stdout.contains("── alpha ──"), "alpha section: {stdout}");
    assert!(stdout.contains("── beta ──"), "beta section: {stdout}");
    assert!(stdout.contains("alpha_thing"), "alpha hit: {stdout}");
    // beta has no `alpha_thing` symbol.
    let beta_section = stdout.split("── beta ──").nth(1).unwrap_or("");
    assert!(
        beta_section.contains("No results"),
        "beta should report no results: {stdout}"
    );
}

#[test]
fn search_workspace_json_groups_by_repo() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\nname = \"A\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["search", "alpha_thing", "--workspace", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"A\""),
        "json should name member A: {stdout}"
    );
    assert!(
        stdout.contains("\"repos\""),
        "json should group by repo: {stdout}"
    );
}

#[test]
fn grep_workspace_groups_matches_by_repo() {
    // grep scans the filesystem directly — no `vex index` needed first.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "fn f() { let NEEDLE = 1; }\n",
    );
    write(
        &root.join("beta").join("b.rs"),
        "fn g() { let other = 2; }\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );

    let out = vex_in(root, &cache)
        .args(["grep", "NEEDLE", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("── alpha ──"), "alpha section: {stdout}");
    assert!(stdout.contains("── beta ──"), "beta section: {stdout}");
    assert!(stdout.contains("NEEDLE"), "alpha match text: {stdout}");
    // The needle is only in alpha; beta's section reports no matches.
    let beta_section = stdout.split("── beta ──").nth(1).unwrap_or("");
    assert!(
        beta_section.contains("No matches"),
        "beta should report no matches: {stdout}"
    );
}

#[test]
fn grep_workspace_json_groups_by_repo() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "fn f() { let NEEDLE = 1; }\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\nname = \"A\"\n",
    );

    let out = vex_in(root, &cache)
        .args(["grep", "NEEDLE", "--workspace", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"A\""),
        "json should name member A: {stdout}"
    );
    assert!(
        stdout.contains("\"matches\"") && stdout.contains("NEEDLE"),
        "json should carry matches: {stdout}"
    );
}

#[test]
fn search_workspace_rejects_local_cache_layout() {
    // A hash-less cache (local_cache) would alias every member to one dir.
    // The guard must reject it before any query runs (review HIGH-1).
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(&root.join(".vex.toml"), "local_cache = true\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n",
    );

    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(root);
    cmd.env_remove("VEX_CACHE_DIR"); // let local_cache take effect
    let out = cmd
        .args(["search", "alpha_thing", "--workspace"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "should reject local_cache in workspace"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("local_cache"),
        "error should mention local_cache: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn search_workspace_conflicts_with_why() {
    // `--why` is single-repo only; clap must reject the combination rather
    // than silently dropping the trace (review HIGH).
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n",
    );
    let out = vex_in(root, &cache)
        .args(["search", "alpha_thing", "--workspace", "--why"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--workspace + --why must be a clap conflict"
    );
}

#[test]
fn usages_workspace_groups_by_repo() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\npub fn caller() { alpha_thing(); }\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["usages", "alpha_thing", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("── alpha ──"), "alpha section: {stdout}");
    assert!(stdout.contains("── beta ──"), "beta section: {stdout}");
    // The call site in alpha is a usage; beta has no alpha_thing.
    assert!(
        stdout.contains("alpha/a.rs") || stdout.contains("a.rs"),
        "alpha usage: {stdout}"
    );
    let beta_section = stdout.split("── beta ──").nth(1).unwrap_or("");
    assert!(
        beta_section.contains("No usages"),
        "beta should report no usages: {stdout}"
    );
}

#[test]
fn usages_workspace_conflicts_with_why() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n",
    );
    let out = vex_in(root, &cache)
        .args(["usages", "alpha_thing", "--workspace", "--why"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--workspace + --why must be a clap conflict"
    );
}

#[test]
fn impact_workspace_reports_per_repo_verdict() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["impact", "alpha_thing", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("── alpha ──"), "alpha section: {stdout}");
    assert!(stdout.contains("── beta ──"), "beta section: {stdout}");
    // Each repo gets its own verdict line.
    assert!(
        stdout.matches("verdict:").count() >= 2,
        "one verdict per repo: {stdout}"
    );
}

#[test]
fn impact_workspace_json_lists_repo_verdicts() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\nname = \"A\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["impact", "alpha_thing", "--workspace", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"A\""),
        "json should name member A: {stdout}"
    );
    assert!(
        stdout.contains("\"verdict\""),
        "json should carry a verdict: {stdout}"
    );
}

#[test]
fn callers_workspace_groups_by_repo() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\npub fn caller() { alpha_thing(); }\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["callers", "alpha_thing", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("── alpha ──"), "alpha section: {stdout}");
    assert!(stdout.contains("── beta ──"), "beta section: {stdout}");
    // `caller` is the only caller of alpha_thing, and it's in alpha.
    assert!(stdout.contains("caller"), "alpha caller: {stdout}");
}

#[test]
fn reachable_workspace_groups_by_repo() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\npub fn caller() { alpha_thing(); }\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["reachable", "alpha_thing", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("── alpha ──"), "alpha section: {stdout}");
    assert!(stdout.contains("── beta ──"), "beta section: {stdout}");
    assert!(
        stdout.contains("caller"),
        "alpha reaches via caller: {stdout}"
    );
}

#[test]
fn callees_workspace_json_groups_by_repo() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\npub fn caller() { alpha_thing(); }\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\nname = \"A\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["callees", "caller", "--workspace", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"A\""),
        "json should name member A: {stdout}"
    );
    assert!(
        stdout.contains("\"callees\""),
        "json keyed by callees: {stdout}"
    );
}

#[test]
fn update_workspace_refreshes_each_member() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    // Add a new symbol to alpha, then update the workspace.
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\npub fn alpha_added() {}\n",
    );
    let out = vex_in(root, &cache)
        .args(["update", "--workspace"])
        .output()
        .unwrap();
    assert!(out.status.success(), "update --workspace should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("alpha"), "alpha listed: {stdout}");
    assert!(stdout.contains("beta"), "beta listed: {stdout}");

    // The new symbol resolves in alpha's refreshed index.
    let chk = vex_in(root, &cache)
        .args(["check", "alpha_added", "--path", "alpha"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&chk.stdout).contains("+ alpha_added"),
        "update should have indexed the new symbol"
    );
}

#[test]
fn update_workspace_json_lists_repos() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\nname = \"A\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["update", "--workspace", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"A\""),
        "json should name member A: {stdout}"
    );
    assert!(
        stdout.contains("total_changed"),
        "json should carry a total: {stdout}"
    );
}

#[test]
fn index_workspace_without_manifest_errors() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(&root.join("src").join("lib.rs"), "pub fn x_thing() {}\n");

    let out = vex_in(root, &cache)
        .args(["index", "--workspace"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "should fail without a workspace file"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(".vex-workspace.toml"),
        "error should mention the missing manifest, got: {stderr}"
    );
}

#[test]
fn index_workspace_json_lists_every_member() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\nname = \"A\"\n\n[[repo]]\npath = \"beta\"\nname = \"B\"\n",
    );

    let out = vex_in(root, &cache)
        .args(["index", "--workspace", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"A\""),
        "json should list member A: {stdout}"
    );
    assert!(
        stdout.contains("\"B\""),
        "json should list member B: {stdout}"
    );
    assert!(
        stdout.contains("total_symbols"),
        "json should carry a total: {stdout}"
    );
}
