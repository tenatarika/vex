//! CLI-level integration tests for v1.6.x behaviour: auto-bootstrap,
//! local_cache mode, cache_dir override, and the helpful-error path
//! when neither auto_update nor an existing index is present.
//!
//! These tests drive the actual `vex` binary via assert_cmd. Each test
//! is self-contained: it creates a tempdir, writes a `.vex.toml` and
//! one source file, runs vex with `current_dir` pointed at the
//! tempdir, and either sets `local_cache = true` or scopes the cache
//! to the tempdir via `$VEX_CACHE_DIR` to keep the global cache
//! pristine.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

/// Spawn the vex binary configured to use a project-local cache so
/// the test never touches the shared platform cache.
fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    // Defence in depth — even if the test forgets to write local_cache
    // in .vex.toml, do not leak into the user's real cache dir.
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write_project(dir: &Path, vex_toml: &str, source_name: &str, source: &str) {
    std::fs::write(dir.join(".vex.toml"), vex_toml).unwrap();
    std::fs::write(dir.join(source_name), source).unwrap();
}

#[test]
fn auto_update_in_config_bootstraps_missing_index() {
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "auto_update = true\nlocal_cache = true\n",
        "a.rs",
        "fn payment_processor() {}\n",
    );

    let assert = vex_in(tmp.path())
        .args(["search", "payment_processor"])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stderr.contains("bootstrapping"),
        "expected bootstrap message in stderr, got: {stderr}"
    );
    assert!(
        stdout.contains("payment_processor"),
        "expected search hit in stdout, got: {stdout}"
    );
    // Bootstrap must persist an index — second invocation should not
    // re-bootstrap.
    let second = vex_in(tmp.path())
        .args(["search", "payment_processor"])
        .assert()
        .success();
    let stderr2 = String::from_utf8_lossy(&second.get_output().stderr);
    assert!(
        !stderr2.contains("bootstrapping"),
        "second invocation should reuse the existing index, got: {stderr2}"
    );
}

#[test]
fn cli_auto_update_flag_bootstraps_without_config() {
    let tmp = TempDir::new().unwrap();
    // No auto_update in .vex.toml — only the CLI flag should drive it.
    write_project(
        tmp.path(),
        "local_cache = true\n",
        "lib.rs",
        "pub fn render() {}\n",
    );

    let assert = vex_in(tmp.path())
        .args(["search", "render", "--auto-update"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stderr.contains("bootstrapping"),
        "stderr should mention bootstrapping: {stderr}"
    );
    assert!(stdout.contains("render"), "stdout missing hit: {stdout}");
}

#[test]
fn missing_index_without_auto_update_emits_both_remedies() {
    let tmp = TempDir::new().unwrap();
    // Empty .vex.toml — no auto_update anywhere.
    write_project(tmp.path(), "local_cache = true\n", "a.rs", "fn x() {}\n");

    let assert = vex_in(tmp.path()).args(["search", "x"]).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    // The error must surface BOTH fixes — running `vex index` or
    // setting auto_update in .vex.toml. The previous "Run `vex index`
    // first" message was found unhelpful by first-time Windows users.
    assert!(
        stderr.contains("vex index"),
        "stderr should suggest `vex index`: {stderr}"
    );
    assert!(
        stderr.contains("auto_update = true"),
        "stderr should mention auto_update fix: {stderr}"
    );
    assert!(
        stderr.contains("No index found"),
        "stderr should label the failure: {stderr}"
    );
}

#[test]
fn local_cache_bootstrap_writes_project_gitignore() {
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "auto_update = true\nlocal_cache = true\n",
        "a.rs",
        "fn foo() {}\n",
    );

    // No env override — local_cache should anchor the cache inside
    // the tempdir, and the helper should drop a .gitignore for safety.
    let assert = Command::cargo_bin("vex")
        .unwrap()
        .current_dir(tmp.path())
        .args(["search", "foo"])
        .assert()
        .success();
    let _ = assert;

    let gitignore = tmp.path().join(".vex_cache").join(".gitignore");
    assert!(
        gitignore.is_file(),
        "expected .vex_cache/.gitignore at {}",
        gitignore.display()
    );
    let body = std::fs::read_to_string(&gitignore).unwrap();
    assert!(
        body.contains("*"),
        "gitignore should contain `*` ignore-all, got: {body:?}"
    );
}

#[test]
fn cache_dir_path_traversal_is_rejected_and_falls_back() {
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "auto_update = true\ncache_dir = \"../../etc/evil\"\n",
        "a.rs",
        "fn foo() {}\n",
    );

    // Use a tempdir-scoped global cache so the platform default fallback
    // lands in an isolated location and doesn't pollute the user's cache.
    let isolated_cache = TempDir::new().unwrap();
    let assert = Command::cargo_bin("vex")
        .unwrap()
        .current_dir(tmp.path())
        .env("HOME", isolated_cache.path())
        .env("XDG_CACHE_HOME", isolated_cache.path())
        .args(["search", "foo"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    // Warning is printed, fallback applies, search still succeeds.
    assert!(
        stderr.contains("traversal"),
        "stderr should warn about `..` traversal: {stderr}"
    );

    // The traversal target must not have been touched.
    let etc_evil = tmp.path().join("../../etc/evil");
    assert!(
        !etc_evil.exists(),
        "vex must not have created the traversed path"
    );
}

#[test]
fn local_cache_index_lives_inside_project() {
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "auto_update = true\nlocal_cache = true\n",
        "a.rs",
        "fn foo() {}\n",
    );

    Command::cargo_bin("vex")
        .unwrap()
        .current_dir(tmp.path())
        .args(["search", "foo"])
        .assert()
        .success();

    // local_cache = true skips the hash subdir — index lives directly
    // under <project>/.vex_cache/.
    let direct = tmp.path().join(".vex_cache").join("index.vex");
    assert!(
        direct.is_file(),
        "expected index at {} (no hash subdir for local_cache)",
        direct.display()
    );
}

#[test]
fn no_stale_check_skips_auto_update_after_bootstrap() {
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "auto_update = true\nlocal_cache = true\n",
        "a.rs",
        "fn one() {}\n",
    );

    // Bootstrap on first run.
    vex_in(tmp.path())
        .args(["search", "one"])
        .assert()
        .success();

    // Second run with --no-stale-check should not even try to update.
    let assert = vex_in(tmp.path())
        .args(["search", "one", "--no-stale-check"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains("auto-updating"),
        "stderr should NOT contain auto-update message with --no-stale-check: {stderr}"
    );
}

#[test]
fn check_command_bootstraps_when_auto_update_set() {
    // Regression: `vex check` is one of the six handlers refactored to
    // call ensure_index_exists. Verify the bootstrap path fires for it
    // the same way it does for `search`.
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "auto_update = true\nlocal_cache = true\n",
        "a.rs",
        "fn known() {}\n",
    );

    let assert = vex_in(tmp.path())
        .args(["check", "known"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stderr.contains("bootstrapping"),
        "check should bootstrap missing index: {stderr}"
    );
    // The check output reports presence — exact format is text or JSON
    // depending on cfg; just assert the queried name appears.
    assert!(
        stdout.contains("known"),
        "check stdout should reference the queried symbol: {stdout}"
    );
}

#[test]
fn callers_auto_update_flag_bootstraps_missing_index() {
    // 10.2 regression: `vex callers --auto-update` in a project with no
    // index must bootstrap on first call instead of staying in live-scan.
    // Without bootstrap, the persistent call-graph FST is never available.
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "local_cache = true\n",
        "lib.rs",
        "fn helper() {}\nfn main() { helper(); }\n",
    );

    let assert = vex_in(tmp.path())
        .args(["callers", "helper", "--auto-update"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stderr.contains("bootstrapping"),
        "callers --auto-update should bootstrap missing index: {stderr}"
    );
    assert!(
        stdout.contains("main"),
        "callers should report `main` as caller of `helper`: {stdout}"
    );
    // Bootstrap must persist — second invocation reuses the index, so
    // the bootstrap banner must not appear again.
    let second = vex_in(tmp.path())
        .args(["callers", "helper", "--auto-update"])
        .assert()
        .success();
    let stderr2 = String::from_utf8_lossy(&second.get_output().stderr);
    assert!(
        !stderr2.contains("bootstrapping"),
        "second callers invocation should reuse the existing index, got: {stderr2}"
    );
}

#[test]
fn callees_auto_update_flag_bootstraps_missing_index() {
    // Symmetric to the callers test — verifies the same wiring on the
    // callees branch of cmd_callgraph.
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "local_cache = true\n",
        "lib.rs",
        "fn helper() {}\nfn main() { helper(); }\n",
    );

    let assert = vex_in(tmp.path())
        .args(["callees", "main", "--auto-update"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stderr.contains("bootstrapping"),
        "callees --auto-update should bootstrap missing index: {stderr}"
    );
    assert!(
        stdout.contains("helper"),
        "callees should report `helper` as callee of `main`: {stdout}"
    );
}

#[test]
fn callers_without_auto_update_falls_back_to_live_scan() {
    // Pre-10.2 UX must be preserved: `vex callers Foo` in a project
    // without an index and without auto_update enabled should NOT bail
    // with "No index found" — it falls through to the live tree-sitter
    // scan that callers always supported.
    let tmp = TempDir::new().unwrap();
    // local_cache so VEX_CACHE_DIR fallback is not hit; no auto_update.
    write_project(
        tmp.path(),
        "local_cache = true\n",
        "lib.rs",
        "fn helper() {}\nfn main() { helper(); }\n",
    );

    let assert = vex_in(tmp.path())
        .args(["callers", "helper"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        !stderr.contains("No index found"),
        "live-scan fallback must not surface index errors: {stderr}"
    );
    assert!(
        !stderr.contains("bootstrapping"),
        "no bootstrap should run without auto_update: {stderr}"
    );
    assert!(
        stdout.contains("main"),
        "live-scan should still find `main` as caller of `helper`: {stdout}"
    );
}

#[test]
fn callers_auto_update_config_drives_bootstrap() {
    // CLI flag is optional when `.vex.toml` sets auto_update = true.
    // Verifies cmd_callgraph's `auto_update || cfg.auto_update` check.
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "auto_update = true\nlocal_cache = true\n",
        "lib.rs",
        "fn helper() {}\nfn main() { helper(); }\n",
    );

    let assert = vex_in(tmp.path())
        .args(["callers", "helper"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("bootstrapping"),
        "callers should bootstrap when .vex.toml auto_update = true: {stderr}"
    );
}

#[test]
fn self_update_check_and_yes_are_mutually_exclusive() {
    // Regression for the earlier review finding: --check + --yes used
    // to be silently accepted with --yes ignored. clap must now reject
    // the combination.
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path(), "local_cache = true\n", "a.rs", "fn x() {}\n");

    let assert = vex_in(tmp.path())
        .args(["self-update", "--check", "--yes"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflicts"),
        "clap should reject --check + --yes combo: {stderr}"
    );
}

/// Read the project's manifest as JSON. Locates the cache root via the
/// VEX_CACHE_DIR env var that `vex_in` sets, then matches the only
/// project-hash subdir inside it. Returns the parsed manifest.
fn read_manifest(tmp: &Path) -> serde_json::Value {
    // VEX_CACHE_DIR is set by `vex_in` to <tmp>/.vex-test-cache. The
    // resolver creates a single project-hash subdir under it.
    let cache = tmp.join(".vex-test-cache");
    let entries: Vec<_> = std::fs::read_dir(&cache)
        .unwrap_or_else(|e| panic!("cache dir {} not readable: {e}", cache.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one project subdir in {}",
        cache.display()
    );
    let manifest_path = entries[0].path().join("manifest.json");
    let bytes = std::fs::read(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
    serde_json::from_slice(&bytes).expect("parse manifest")
}

#[test]
fn index_no_call_graph_persists_opt_out_in_manifest() {
    // 10.3 core invariant: `--no-call-graph` writes call_graph=false in
    // the manifest so subsequent `vex update` knows the user opted out.
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "",
        "lib.rs",
        "fn helper() {}\nfn main() { helper(); }\n",
    );

    vex_in(tmp.path())
        .args(["index", "--no-call-graph"])
        .assert()
        .success();

    let manifest = read_manifest(tmp.path());
    assert_eq!(
        manifest["call_graph"].as_bool(),
        Some(false),
        "manifest should record call_graph=false after --no-call-graph: {manifest}"
    );
    // BM25 was not opted out — should be true.
    assert_eq!(
        manifest["bm25"].as_bool(),
        Some(true),
        "manifest should record bm25=true when only call-graph was opted out: {manifest}"
    );
}

#[test]
fn index_no_bm25_persists_opt_out_in_manifest() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path(), "", "lib.rs", "fn helper() {}\n");

    vex_in(tmp.path())
        .args(["index", "--no-bm25"])
        .assert()
        .success();

    let manifest = read_manifest(tmp.path());
    assert_eq!(manifest["bm25"].as_bool(), Some(false));
    assert_eq!(manifest["call_graph"].as_bool(), Some(true));
}

#[test]
fn update_inherits_no_call_graph_from_manifest() {
    // The sticky-opt-out invariant: after `vex index --no-call-graph`,
    // a bare `vex update` must NOT silently grow the call-graph section.
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "",
        "lib.rs",
        "fn helper() {}\nfn main() { helper(); }\n",
    );

    vex_in(tmp.path())
        .args(["index", "--no-call-graph"])
        .assert()
        .success();

    // Touch the source to force a meaningful update pass.
    std::fs::write(
        tmp.path().join("lib.rs"),
        "fn helper() {}\nfn main() { helper(); }\nfn extra() {}\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["update"]).assert().success();

    let manifest = read_manifest(tmp.path());
    assert_eq!(
        manifest["call_graph"].as_bool(),
        Some(false),
        "update without flag must inherit call_graph=false from prior manifest: {manifest}"
    );
}

#[test]
fn config_call_graph_false_drives_index() {
    // `.vex.toml call_graph = false` should opt out without needing the
    // CLI flag.
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path(), "call_graph = false\n", "lib.rs", "fn x() {}\n");

    vex_in(tmp.path()).args(["index"]).assert().success();

    let manifest = read_manifest(tmp.path());
    assert_eq!(manifest["call_graph"].as_bool(), Some(false));
}

#[test]
fn callers_works_with_no_call_graph_index() {
    // Primary user-visible contract: after `vex index --no-call-graph`,
    // `vex callers` must still find callers via live-scan fallback. The
    // index lacks the call-graph FST so the reader's fast path is gated
    // off (`has_call_graph()` returns false) and cmd_callgraph drops to
    // the tree-sitter walker.
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "",
        "lib.rs",
        "fn helper() {}\nfn main() { helper(); }\n",
    );

    vex_in(tmp.path())
        .args(["index", "--no-call-graph"])
        .assert()
        .success();

    let assert = vex_in(tmp.path())
        .args(["callers", "helper"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("main"),
        "callers must still find `main` via live-scan when call-graph section is absent: {stdout}"
    );
}

#[test]
fn search_works_with_no_bm25_index() {
    // `--no-bm25` drops the BM25 channel from hybrid search. Structural
    // (FST) search must still resolve symbol names — degradation is
    // graceful, not breakage.
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "",
        "lib.rs",
        "fn payment_processor() {}\nfn unrelated() {}\n",
    );

    vex_in(tmp.path())
        .args(["index", "--no-bm25"])
        .assert()
        .success();

    let assert = vex_in(tmp.path())
        .args(["search", "payment_processor"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("payment_processor"),
        "structural search must still return symbol hits with bm25 disabled: {stdout}"
    );
}

#[test]
fn cli_no_call_graph_overrides_config_true() {
    // Precedence test: CLI flag wins over `.vex.toml call_graph = true`.
    // Tested direction is the opposite of the existing config-drives-index
    // test (config=false). Together they cover both directions.
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path(), "call_graph = true\n", "lib.rs", "fn x() {}\n");

    vex_in(tmp.path())
        .args(["index", "--no-call-graph"])
        .assert()
        .success();

    let manifest = read_manifest(tmp.path());
    assert_eq!(
        manifest["call_graph"].as_bool(),
        Some(false),
        "CLI --no-call-graph must override .vex.toml call_graph = true: {manifest}"
    );
}
