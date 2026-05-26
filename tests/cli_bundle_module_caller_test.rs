//! CLI bundle regression tests for Phase 14.1 — module-level callers in
//! bundle output.
//!
//! Test H: `vex bundle --mode symbol create_app` must include the
//! synthetic `<module:…>` caller in the `callers` / `transitive_callers`
//! field of the JSON envelope when a module-scope `app = create_app()`
//! expression exists in the indexed project.
//!
//! The `pr-impact` mode requires a real git history, which is expensive to
//! wire in a unit test. We instead use `--mode symbol` which assembles the
//! per-symbol bundle from the persistent call-graph without needing a git
//! diff context.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write_and_index(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("app.py"),
        "def create_app():\n    return None\n\napp = create_app()\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

// ---------------------------------------------------------------------------
// Test H: bundle symbol mode surfaces Module caller
// ---------------------------------------------------------------------------

/// `vex bundle --mode symbol --symbol create_app` assembles a JSON envelope
/// for the symbol. When `app = create_app()` exists at module scope, the
/// `callers` field (or `transitive_callers`) must include the
/// `<module:src/app.py>` entry.
///
/// If the bundle JSON shape for symbol mode does not expose a `callers`
/// field (possible if inc wiring is incomplete), the test is marked
/// `#[ignore]` to serve as a Phase 14.1 follow-up placeholder rather than
/// a flaky failure.
#[test]
#[ignore = "Phase 14.1 follow-up: bundle --mode symbol callers field wiring \
            is not yet confirmed; un-ignore once GREEN phase wires it"]
fn bundle_pr_impact_surfaces_module_caller() {
    let tmp = TempDir::new().unwrap();
    write_and_index(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["bundle", "--mode", "symbol", "--symbol", "create_app"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("`vex bundle --mode symbol` returned non-JSON: {e}\n---\n{stdout}")
    });

    // Bundle schema: results.items is a flat array of objects; each item
    // has a `role` field. Callers have role == "caller". The Module
    // synthetic symbol appears as a caller item with name == "<module:…>".
    let items = json
        .get("results")
        .and_then(|r| r.get("items"))
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("bundle JSON must have `results.items` array; got: {json}"));

    let caller_items: Vec<&serde_json::Value> = items
        .iter()
        .filter(|item| item["role"].as_str() == Some("caller"))
        .collect();

    let has_module_caller = caller_items.iter().any(|c| {
        c["name"]
            .as_str()
            .map(|n| n.starts_with("<module:"))
            .unwrap_or(false)
    });

    assert!(
        has_module_caller,
        "bundle `results.items` (role=caller) must include the Module caller \
         for `create_app`; caller items: {caller_items:?}"
    );
}

// ---------------------------------------------------------------------------
// Smoke test: bundle command accepts symbol mode without panicking
// ---------------------------------------------------------------------------

/// Smoke test that does NOT assert on Module caller content — just verifies
/// `vex bundle --mode symbol --symbol create_app` exits 0 and emits valid
/// JSON. This test is NOT ignored and runs during GREEN phase to confirm
/// basic wiring before the full assertion in Test H is un-ignored.
#[test]
fn bundle_symbol_mode_emits_valid_json() {
    let tmp = TempDir::new().unwrap();
    write_and_index(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["bundle", "--mode", "symbol", "--symbol", "create_app"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // Must parse as JSON — any valid JSON shape is accepted here.
    let _: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("`vex bundle --mode symbol create_app` returned non-JSON: {e}\n---\n{stdout}")
    });
}
