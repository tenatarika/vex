//! E2E for `vex index --workspace` (multi-repo phase 3): each member of a
//! `.vex-workspace.toml` is indexed into its own per-repo index dir, and a
//! subsequent per-member query resolves against that member's index only.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

/// Smoke test for `vex watch --workspace` (Phase 7): it is long-running, so
/// rather than depend on event-delivery timing we spawn it, poll for both
/// members' initial index files (the startup build), then kill it. Guards
/// against startup regressions (resolver not installed, panic, member-loop
/// bug) without flaky event-timing assertions.
#[test]
fn watch_workspace_builds_initial_member_indexes() {
    use std::process::{Command as ProcCommand, Stdio};
    use std::time::{Duration, Instant};

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

    let bin = assert_cmd::cargo::cargo_bin("vex");
    let mut child = ProcCommand::new(bin)
        .args(["watch", "--workspace"])
        .current_dir(root)
        .env("VEX_CACHE_DIR", &cache)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vex watch --workspace");

    // Both members' indexes live under the platform cache, hashed by their
    // canonical root. Poll the cache tree for two `index.vex` files.
    let deadline = Instant::now() + Duration::from_secs(30);
    let count_indexes = || -> usize { walk_count_index_vex(&cache) };
    let mut built = 0;
    while Instant::now() < deadline {
        built = count_indexes();
        if built >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        built >= 2,
        "watch --workspace should build both members' initial indexes (found {built})"
    );
}

/// Recursively count `index.vex` files under `dir` (test helper).
fn walk_count_index_vex(dir: &Path) -> usize {
    let mut n = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            n += walk_count_index_vex(&path);
        } else if path.file_name().and_then(|s| s.to_str()) == Some("index.vex") {
            n += 1;
        }
    }
    n
}

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

/// Build a one-member workspace (indexed WITHOUT `--semantic`, so its index
/// carries no vectors) and return `(TempDir, root, cache)`. Kept as a TempDir
/// guard so the caller controls teardown timing.
fn semantic_fallback_workspace() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\nname = \"A\"\n",
    );
    vex_in(&root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();
    (tmp, root, cache)
}

/// Parse `search --workspace --format json` stdout and return the first repo
/// object, asserting the `results.repos` array path is well-formed (a bare
/// `json["results"]["repos"][0]` would silently yield `Value::Null` on a
/// shape change and assert against nothing).
fn first_repo(stdout: &str) -> serde_json::Value {
    let json: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"));
    let repos = json["results"]["repos"]
        .as_array()
        .unwrap_or_else(|| panic!("expected results.repos array: {stdout}"));
    assert!(!repos.is_empty(), "expected at least one repo: {stdout}");
    repos[0].clone()
}

/// `search --semantic --workspace` over a member whose index has no vectors
/// surfaces the fallback per repo: the JSON repo object carries
/// `semantic_channel: "index_lacks_vectors"`. No embedder is loaded — the
/// reason is pinned from `reader.has_vectors()` before any semantic channel
/// runs — so this stays offline / ONNX-free.
#[test]
fn search_workspace_json_surfaces_semantic_channel_fallback() {
    let (_tmp, root, cache) = semantic_fallback_workspace();
    let out = vex_in(&root, &cache)
        .args([
            "search",
            "alpha_thing",
            "--semantic",
            "--workspace",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let repo = first_repo(&stdout);
    assert_eq!(
        repo["semantic_channel"], "index_lacks_vectors",
        "repo object should surface the semantic fallback reason: {stdout}"
    );
}

/// The contract inverse: a NON-`--semantic` workspace search must NOT emit a
/// `semantic_channel` field. `not_requested` is uniform across members and
/// derivable from the absent flag, so it is suppressed as noise (the field's
/// presence is reserved to mean "this member degraded").
#[test]
fn search_workspace_json_omits_semantic_channel_when_not_requested() {
    let (_tmp, root, cache) = semantic_fallback_workspace();
    let out = vex_in(&root, &cache)
        .args(["search", "alpha_thing", "--workspace", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let repo = first_repo(&stdout);
    assert!(
        repo.get("semantic_channel").is_none(),
        "non-semantic search must not emit semantic_channel: {stdout}"
    );
}

/// §4 agent-output in workspace mode: the drift advisory is query-scoped, so
/// it surfaces on the top-level `_meta.vex.dev/search_hint` only when EVERY
/// member drifted (no member has a structural definition); a defined symbol
/// omits it.
#[test]
fn search_workspace_json_surfaces_drift_hint_when_all_members_drift() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn caller() { undefined_symbol(); }\npub fn known_def() -> u8 { 7 }\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\nname = \"A\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    // Undefined identifier → the single member drifts → top-level hint present.
    let out = vex_in(root, &cache)
        .args([
            "search",
            "undefined_symbol",
            "--workspace",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let hint = &json["_meta"]["vex.dev/search_hint"];
    assert_eq!(
        hint["reason"], "no_local_definition",
        "workspace drift must surface a top-level hint: {stdout}"
    );
    assert_eq!(hint["query"], "undefined_symbol");

    // Defined symbol → no drift → no hint.
    let out2 = vex_in(root, &cache)
        .args(["search", "known_def", "--workspace", "--format", "json"])
        .output()
        .unwrap();
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    let json2: serde_json::Value = serde_json::from_str(&stdout2).unwrap();
    assert!(
        json2["_meta"].get("vex.dev/search_hint").is_none(),
        "defined symbol must not surface a drift hint: {stdout2}"
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
fn workspace_rejects_root_local_cache_across_multiple_members() {
    // A hash-less cache at the WORKSPACE ROOT (local_cache) would alias every
    // member to one flat dir. With >1 member the resolver must reject it at
    // dispatch (Phase 2 narrowed guard). Per-member local_cache is fine — see
    // `workspace_honours_per_member_local_cache`.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(&root.join(".vex.toml"), "local_cache = true\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
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
        "root local_cache across >1 member must be rejected"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("collide"),
        "error should mention collision: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn workspace_root_local_cache_single_member_succeeds() {
    // The narrowed guard rejects root local_cache only across >1 member.
    // A SINGLE-member workspace with root local_cache can't alias anything,
    // so it must succeed and index into the shared in-tree cache.
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
    cmd.env_remove("VEX_CACHE_DIR"); // let root local_cache take effect
    assert!(
        cmd.args(["index", "--workspace"])
            .output()
            .unwrap()
            .status
            .success(),
        "single-member workspace with root local_cache must succeed (no aliasing)"
    );
    // Index landed in the shared in-tree `.vex_cache/` at the workspace root.
    assert!(
        root.join(".vex_cache").join("index.vex").is_file(),
        "single-member root local_cache index should live at root/.vex_cache"
    );
}

#[test]
fn workspace_honours_per_member_local_cache() {
    // Phase 2: members with their OWN `local_cache = true` each index into
    // their in-tree `<member>/.vex_cache/` (with a `*` .gitignore), in
    // DISJOINT dirs — no aliasing. Hermetic: no VEX_CACHE_DIR (env would beat
    // local_cache), so nothing touches the real platform cache.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() {}\n",
    );
    write(
        &root.join("alpha").join(".vex.toml"),
        "local_cache = true\n",
    );
    write(&root.join("beta").join("b.rs"), "pub fn beta_thing() {}\n");
    write(&root.join("beta").join(".vex.toml"), "local_cache = true\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );

    // No VEX_CACHE_DIR — let each member's local_cache take effect.
    let vex_no_env = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("vex").unwrap();
        cmd.current_dir(root);
        cmd.env_remove("VEX_CACHE_DIR");
        cmd.args(args).output().unwrap()
    };

    assert!(
        vex_no_env(&["index", "--workspace"]).status.success(),
        "index --workspace with per-member local_cache should succeed"
    );

    // Each member's index landed in its OWN in-tree dir, with a gitignore —
    // disjoint, no committable-cache leak.
    for m in ["alpha", "beta"] {
        assert!(
            root.join(m).join(".vex_cache").join("index.vex").is_file(),
            "{m} local_cache index should live in-tree"
        );
        assert!(
            root.join(m).join(".vex_cache").join(".gitignore").is_file(),
            "{m} in-tree cache must get a `*` .gitignore"
        );
    }

    // Each member resolves its OWN symbol but NOT the sibling's (disjoint
    // indexes, not aliased into one).
    let check = |sym: &str, member: &str| -> bool {
        let out = vex_no_env(&["check", sym, "--path", member]);
        String::from_utf8_lossy(&out.stdout).contains(&format!("+ {sym}"))
    };
    assert!(check("alpha_thing", "alpha"), "alpha_thing in alpha");
    assert!(check("beta_thing", "beta"), "beta_thing in beta");
    assert!(
        !check("beta_thing", "alpha"),
        "beta_thing must NOT resolve in alpha (disjoint local caches)"
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

// ── Multi-repo Phase 6: cross-repo strict-usages fallback ──────────────────

/// `alpha` defines `shared_helper`; `beta` calls it without defining it.
/// `usages shared_helper --strict --workspace` must surface beta's call site
/// as a name-resolved cross-repo hit attributed to alpha, even though beta's
/// own Pass-2 left the ref unresolved (it lives in a sibling repo).
#[test]
fn usages_strict_workspace_surfaces_cross_repo_ref() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn shared_helper() {}\n",
    );
    write(
        &root.join("beta").join("b.rs"),
        "pub fn beta_caller() { shared_helper(); }\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["usages", "shared_helper", "--strict", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // beta's call site surfaces as a cross-repo hit resolving to alpha.
    assert!(
        stdout.contains("cross-repo → alpha"),
        "beta's call should resolve cross-repo to alpha: {stdout}"
    );
    let beta_section = stdout.split("── beta ──").nth(1).unwrap_or("");
    assert!(
        beta_section.contains("b.rs"),
        "cross-repo hit should point at beta/b.rs: {stdout}"
    );
}

/// JSON surface: a cross-repo member object carries `cross_repo_usages`,
/// `resolves_to`, and `confidence: "name"` (the distinct sub-tier).
#[test]
fn usages_strict_workspace_cross_repo_json_tier() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn shared_helper() {}\n",
    );
    write(
        &root.join("beta").join("b.rs"),
        "pub fn beta_caller() { shared_helper(); }\n",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args([
            "usages",
            "shared_helper",
            "--strict",
            "--workspace",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Whitespace-insensitive value assertions (tolerant of pretty-printing):
    // the cross-repo tier must resolve to alpha and be tagged name-resolved.
    let compact: String = stdout.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(compact.contains("cross_repo_usages"), "json tier: {stdout}");
    assert!(
        compact.contains("\"resolves_to\":\"alpha\""),
        "resolves_to must equal alpha: {stdout}"
    );
    assert!(
        compact.contains("\"confidence\":\"name\""),
        "confidence must equal name: {stdout}"
    );
}

/// First-hit-wins owner attribution: when two members define the symbol,
/// a third member's cross-repo refs resolve to the FIRST-declared owner,
/// and each owner still renders its own in-repo strict section.
#[test]
fn usages_strict_workspace_multiple_owners_first_wins() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(&root.join("alpha").join("a.rs"), "pub fn dup_fn() {}\n");
    write(&root.join("beta").join("b.rs"), "pub fn dup_fn() {}\n");
    write(
        &root.join("gamma").join("g.rs"),
        "pub fn gamma_caller() { dup_fn(); }\n",
    );
    // alpha declared before beta → alpha is the first-hit owner.
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n\n[[repo]]\npath = \"gamma\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["usages", "dup_fn", "--strict", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("cross-repo → alpha"),
        "gamma's call resolves to the first-declared owner alpha: {stdout}"
    );
    assert!(
        !stdout.contains("cross-repo → beta"),
        "beta must not be the attributed owner (alpha declared first): {stdout}"
    );
}

/// A name defined NOWHERE in the workspace must NOT surface unresolved refs
/// (no owner → the fallback stays silent, preserving strict precision).
#[test]
fn usages_strict_workspace_no_owner_no_cross_repo() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn alpha_thing() { mystery_absent_fn(); }\n",
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
        .args(["usages", "mystery_absent_fn", "--strict", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("cross-repo →"),
        "no member defines mystery_absent_fn — fallback must stay silent: {stdout}"
    );
}

/// Carry-forward: after a `vex update` that re-indexes beta because one of
/// its files changed, the UNCHANGED file's cross-repo unresolved ref must
/// still surface. Guards the §6 regression where the Q4-A reconstruction
/// (resolved RefEdges only) would silently drop unresolved refs.
#[test]
fn usages_strict_workspace_cross_repo_survives_update() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let cache = root.join(".cache");
    write(
        &root.join("alpha").join("a.rs"),
        "pub fn shared_helper() {}\n",
    );
    // b.rs holds the cross-repo ref and stays UNCHANGED; b2.rs is the file
    // we mutate to force beta's incremental update.
    write(
        &root.join("beta").join("b.rs"),
        "pub fn beta_caller() { shared_helper(); }\n",
    );
    write(&root.join("beta").join("b2.rs"), "pub fn beta_other() {}\n");
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root, &cache)
        .args(["index", "--workspace"])
        .assert()
        .success();

    // Change b2.rs only, then incrementally update the workspace.
    write(
        &root.join("beta").join("b2.rs"),
        "pub fn beta_other() {}\npub fn beta_added() {}\n",
    );
    vex_in(root, &cache)
        .args(["update", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root, &cache)
        .args(["usages", "shared_helper", "--strict", "--workspace"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("cross-repo → alpha"),
        "cross-repo ref from the unchanged b.rs must survive `vex update`: {stdout}"
    );
}
