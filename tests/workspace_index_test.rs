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
