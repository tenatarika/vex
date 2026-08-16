//! CLI-level tests for `--async-update`: a stale index is refreshed *behind*
//! the query instead of in front of it.
//!
//! Readers take no lock and a live mmap survives the index's atomic rename, so
//! waiting for a rebuild in front of the query buys nothing — see the CHANGELOG
//! entry for the measurements. These tests pin the observable contract rather
//! than the timing, which is what survives on a loaded CI box: what the caller
//! is told, and that the refresh really happens.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// Locate the index directory the test's cache override produced. The layout
/// (`$VEX_CACHE_DIR/<project-hash>/`) is an implementation detail, so find it by
/// looking for the manifest rather than reconstructing the hash.
fn find_index_dir(project: &Path) -> Option<std::path::PathBuf> {
    fn walk(dir: &Path, depth: usize) -> Option<std::path::PathBuf> {
        if depth > 3 {
            return None;
        }
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if p.join("manifest.json").is_file() {
                    return Some(p);
                }
                if let Some(found) = walk(&p, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(&project.join(".vex-test-cache"), 0)
}

/// A project with an index already built, plus one edit so the index is stale.
fn stale_project(tmp: &TempDir, extra_config: &str) {
    let dir = tmp.path();
    std::fs::write(
        dir.join(".vex.toml"),
        format!("local_cache = true\n{extra_config}"),
    )
    .unwrap();
    std::fs::write(dir.join("a.rs"), "fn payment_processor() {}\n").unwrap();
    vex_in(dir).arg("index").assert().success();
    // The edit that makes it stale — a new symbol the index does not know.
    std::fs::write(
        dir.join("b.rs"),
        "fn settlement_reconciler() {}\nfn payment_processor_two() {}\n",
    )
    .unwrap();
}

/// Wait for the detached child to land its rebuild. Polls rather than sleeping a
/// fixed time so the test is not a race on a loaded machine.
fn wait_for_symbol(dir: &Path, symbol: &str) -> bool {
    for _ in 0..100 {
        let out = vex_in(dir)
            .args(["check", symbol, "--no-stale-check"])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&out.stdout).contains(symbol) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

#[test]
fn async_update_answers_from_the_existing_index_and_says_it_is_stale() {
    let tmp = TempDir::new().unwrap();
    stale_project(&tmp, "");

    let assert = vex_in(tmp.path())
        .args([
            "search",
            "payment_processor",
            "--auto-update",
            "--async-update",
            "--format",
            "json",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let meta = &env["_meta"];

    assert_eq!(
        meta["vex.dev/stale"],
        serde_json::json!(true),
        "a query served from an unrefreshed index must advertise staleness: {stdout}"
    );
    let reason = meta["vex.dev/stale_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("background"),
        "the reason should say a refresh is running, got: {reason}"
    );
    assert!(
        stderr.contains("refreshing in the background"),
        "expected the background notice on stderr, got: {stderr}"
    );
    // The pre-existing symbol is still answerable — the whole point is that the
    // query is served, not deferred.
    assert!(
        env["results"].as_array().is_some_and(|r| !r.is_empty()),
        "expected results from the existing index: {stdout}"
    );

    // And the refresh genuinely happens: the new symbol shows up without any
    // further `vex update`.
    assert!(
        wait_for_symbol(tmp.path(), "settlement_reconciler"),
        "the background refresh never landed the new symbol"
    );
}

#[test]
fn async_update_can_be_set_in_config() {
    let tmp = TempDir::new().unwrap();
    stale_project(&tmp, "auto_update = true\nasync_update = true\n");

    let assert = vex_in(tmp.path())
        .args(["search", "payment_processor"])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("refreshing in the background"),
        "config `async_update` should behave like the flag, got: {stderr}"
    );
    assert!(wait_for_symbol(tmp.path(), "settlement_reconciler"));
}

/// Without the flag the old contract stands: the query waits, and the result is
/// fresh rather than flagged stale.
#[test]
fn blocking_auto_update_is_unchanged() {
    let tmp = TempDir::new().unwrap();
    stale_project(&tmp, "");

    let assert = vex_in(tmp.path())
        .args([
            "search",
            "settlement_reconciler",
            "--auto-update",
            "--format",
            "json",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        env["_meta"]["vex.dev/stale"].is_null(),
        "a synchronous refresh should leave no stale flag: {stdout}"
    );
    assert!(
        env["results"].as_array().is_some_and(|r| !r.is_empty()),
        "the symbol added before the query should be found: {stdout}"
    );
}

/// The flag's own help text promises it does nothing without auto-update. That
/// promise was never exercised until review asked for it.
#[test]
fn async_update_alone_does_nothing() {
    let tmp = TempDir::new().unwrap();
    stale_project(&tmp, "");

    let dir = tmp.path();
    let assert = vex_in(dir)
        .args([
            "search",
            "payment_processor",
            "--async-update",
            "--format",
            "json",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        !stderr.contains("refreshing in the background"),
        "without auto-update nothing should be refreshed, got: {stderr}"
    );
    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        env["_meta"]["vex.dev/stale_reason"].is_null(),
        "no refresh was attempted, so there is no background reason to report: {stdout}"
    );
    assert!(
        find_index_dir(dir).is_none_or(|d| !d.join("async_update.attempt").exists()),
        "the flag alone must not record a background attempt"
    );
    // And the new symbol is still absent, i.e. nothing refreshed behind us.
    let check = vex_in(dir)
        .args(["check", "settlement_reconciler", "--no-stale-check"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&check.stdout).contains("settlement_reconciler:"),
        "the index must not have been refreshed"
    );
}

/// The bug review caught: the background path used to return *before* the
/// embedder-mismatch guard, so a child would have re-embedded with a different
/// model and mixed embedding spaces on disk — with its warning discarded down a
/// nulled stderr. The guard must win over the refresh, background or not.
#[test]
fn embedder_mismatch_refuses_even_with_async_update() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join(".vex.toml"),
        "local_cache = true\nsemantic = true\nembedder = \"minilm-l6-v2\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("a.rs"), "fn payment_processor() {}\n").unwrap();
    // Build without embeddings so the manifest records no vectors, then claim a
    // different embedder in config: that is the mismatch the guard is about. A
    // real semantic build would need the model downloaded, which a unit-scope
    // test must not do — the manifest field is what the guard reads.
    vex_in(dir).arg("index").assert().success();
    let index_dir = find_index_dir(dir).expect("index dir after a successful index");
    let manifest = index_dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    json["embedder_id"] = serde_json::json!("some-other-embedder");
    std::fs::write(&manifest, serde_json::to_string(&json).unwrap()).unwrap();
    std::fs::write(dir.join("b.rs"), "fn settlement_reconciler() {}\n").unwrap();

    // Not asserting success: with `semantic = true` the search path itself
    // refuses a mismatched embedder further down, which is pre-existing
    // behaviour and not what this test is about. What matters is that the guard
    // fired *before* any refresh and that nothing was started in the background.
    let out = vex_in(dir)
        .args([
            "search",
            "payment_processor",
            "--auto-update",
            "--async-update",
        ])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("would switch embedder"),
        "the mismatch guard must fire before any refresh, got: {stderr}"
    );
    assert!(
        !stderr.contains("refreshing in the background"),
        "a refused refresh must not be started in the background either, got: {stderr}"
    );
    assert!(
        !index_dir.join("async_update.attempt").exists(),
        "no background attempt should have been recorded"
    );
}

/// The failure mode that defeated the first attempt at this: the attempt marker
/// and the child's log both live in the index directory, so an index directory
/// that cannot be written disabled the very diagnostics meant to explain it —
/// every query silently forked a doomed child forever. Opening the log is now
/// the writability probe, and an unwritable directory is reported instead.
#[test]
fn unwritable_index_dir_is_reported_instead_of_forking_forever() {
    let tmp = TempDir::new().unwrap();
    stale_project(&tmp, "auto_update = true\nasync_update = true\n");
    let dir = tmp.path();
    let index_dir = find_index_dir(dir).expect("index dir after a successful index");

    let mut perms = std::fs::metadata(&index_dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&index_dir, perms).unwrap();
    // Running as root ignores the read-only bit; skip rather than assert a
    // condition the platform refuses to create.
    let writable_anyway = std::fs::File::create(index_dir.join(".probe")).is_ok();
    if writable_anyway {
        let _ = std::fs::remove_file(index_dir.join(".probe"));
        let mut perms = std::fs::metadata(&index_dir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&index_dir, perms).unwrap();
        return;
    }

    let out = vex_in(dir)
        .args(["search", "payment_processor", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // Restore before asserting so a failure cannot leave an unremovable tempdir.
    let mut perms = std::fs::metadata(&index_dir).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&index_dir, perms).unwrap();

    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let reason = env["_meta"]["vex.dev/stale_reason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reason.contains("not writable"),
        "the caller should be told why no refresh can succeed, got: {reason}"
    );
    assert!(
        !stderr.contains("refreshing in the background"),
        "nothing was started, so nothing should be announced: {stderr}"
    );
}

/// The rule that keeps the feature honest: with no index there is nothing to
/// answer from, so the build stays synchronous even with the flag.
#[test]
fn missing_index_still_bootstraps_synchronously() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join(".vex.toml"),
        "local_cache = true\nauto_update = true\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn payment_processor() {}\n").unwrap();

    let assert = vex_in(tmp.path())
        .args(["search", "payment_processor", "--async-update"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("payment_processor"),
        "a cold start must still answer from a freshly built index: {stdout}"
    );
}
