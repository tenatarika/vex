//! CLI integration tests for Phase 14.1 — module-level callers.
//!
//! Each source file that is indexed receives one synthetic
//! `SymbolKind::Module` record named `<module:relpath>`. Call edges whose
//! enclosing scope is NOT inside any function or method attribute their
//! `caller_sym_idx` to this synthetic symbol, so `vex callers <fn>`
//! surfaces them as module-scope callers.
//!
//! Contract matrix:
//!  - Module symbols MUST appear in  : `vex callers`
//!  - Module symbols MUST NOT appear : `vex search`, `vex outline`, `--kind`
//!    ranked output
//!  - `vex usages --strict`          : MUST NOT include module-call edges
//!    (intentional; strict reads binder refs only, not call edges)

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

/// Post-H5-full every CLI JSON emission is wrapped in the Phase 13
/// envelope (`{ protocol_version, capabilities, _meta, results }`).
/// Unwrap to `results` so call-site assertions stay the same shape
/// they were pre-H5.
fn unwrap_results(envelope: serde_json::Value) -> serde_json::Value {
    envelope
        .get("results")
        .cloned()
        .unwrap_or(serde_json::json!({}))
}

// ---------------------------------------------------------------------------
// Test-local helpers — copy the vex_in pattern from cli_bootstrap_test.rs.
// Each test file is its own binary; no shared common/ mod in this project.
// ---------------------------------------------------------------------------

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    // Scope the cache to the tempdir so tests never touch the user's real
    // global index.
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// Extract the hits array from a `vex search --format json` response.
///
/// The search envelope is:
///   { "results": [...], "protocol_version": "v1", "_meta": {...}, ... }
///
/// `results` is a flat array of symbol objects. Returns an empty vec if
/// the field is absent (e.g. old format or empty result).
fn search_results(json: &serde_json::Value) -> Vec<&serde_json::Value> {
    json.get("results")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// Write the standard fixture used by most tests in this file.
///
///   src/app.py
///   ----------
///   def create_app():
///       return None
///
///   app = create_app()   ← module-scope call site (line 4)
fn write_app_fixture(dir: &Path) {
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
// Test A: module symbol is emitted and surfaces in vex callers
// ---------------------------------------------------------------------------

/// `vex callers create_app --format json` must include exactly one caller
/// whose name is `<module:src/app.py>` at the module-scope call site.
///
/// The assertion on `line` checks for line 4 because `find_callers_fast`
/// emits `edge.line` (the call-site line), NOT the caller symbol's `line`
/// field (which is 1 for Module symbols).
#[test]
fn module_symbol_emitted_per_python_file() {
    let tmp = TempDir::new().unwrap();
    write_app_fixture(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["callers", "create_app", "--format", "json"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value =
        unwrap_results(serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!("`vex callers --format json` stdout not valid JSON: {e}\n---\n{stdout}")
        }));

    // The result must be an array of caller objects.
    let callers = json
        .as_array()
        .unwrap_or_else(|| panic!("expected JSON array from `vex callers`, got: {json}"));

    // There must be at least one Module caller.
    let module_caller = callers
        .iter()
        .find(|c| {
            c["name"]
                .as_str()
                .map(|n| n == "<module:src/app.py>")
                .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            panic!(
                "expected caller named `<module:src/app.py>` in callers output; got: {callers:?}"
            )
        });

    // The path reported for this caller must be the Python file.
    let path = module_caller["path"].as_str().unwrap_or("");
    assert!(
        path == "src/app.py" || path.ends_with("src/app.py"),
        "Module caller path should be `src/app.py`, got: {path}"
    );

    // The call-site line must be 4 (the `app = create_app()` statement).
    let line = module_caller["line"].as_u64().unwrap_or(0);
    assert_eq!(
        line, 4,
        "Module caller line should be 4 (the call-site), got: {line}"
    );
}

// ---------------------------------------------------------------------------
// Test B: module symbols excluded from search results
// ---------------------------------------------------------------------------

/// FST exclusion: searching for any query that would match the synthetic
/// `<module:…>` name must return no result with that name. Real symbols
/// (like `create_app`) must still be findable.
#[test]
fn module_symbol_excluded_from_search() {
    let tmp = TempDir::new().unwrap();
    write_app_fixture(tmp.path());

    // Searching for literal "module" must produce no `<module:` hit.
    let out_module = vex_in(tmp.path())
        .args(["search", "module", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json_module: serde_json::Value =
        serde_json::from_slice(&out_module).unwrap_or(serde_json::json!({}));
    for hit in search_results(&json_module) {
        let name = hit["name"].as_str().unwrap_or("");
        assert!(
            !name.starts_with("<module:"),
            "FST search for 'module' must not surface synthetic Module symbol; got: {name}"
        );
    }

    // Searching for the path fragment must also not return Module.
    let out_path = vex_in(tmp.path())
        .args(["search", "src/app", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json_path: serde_json::Value =
        serde_json::from_slice(&out_path).unwrap_or(serde_json::json!({}));
    for hit in search_results(&json_path) {
        let name = hit["name"].as_str().unwrap_or("");
        assert!(
            !name.starts_with("<module:"),
            "search for path fragment must not surface synthetic Module symbol; got: {name}"
        );
    }

    // Sanity: create_app IS findable via normal search.
    let out_real = vex_in(tmp.path())
        .args(["search", "create_app", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json_real: serde_json::Value = serde_json::from_slice(&out_real)
        .unwrap_or_else(|e| panic!("search for create_app returned non-JSON: {e}"));
    let hits = search_results(&json_real).len();
    assert!(
        hits >= 1,
        "expected at least one search hit for `create_app`, got: {json_real}"
    );
}

// ---------------------------------------------------------------------------
// Test C: module symbols excluded from outline
// ---------------------------------------------------------------------------

/// `vex outline src/app.py --format json` must list `create_app` but must
/// NOT contain any entry whose `kind` is `"module"` or whose name starts
/// with `<module:`.
#[test]
fn module_symbol_excluded_from_outline() {
    let tmp = TempDir::new().unwrap();
    write_app_fixture(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["outline", "src/app.py", "--format", "json"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value =
        unwrap_results(serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!("`vex outline --format json` returned non-JSON: {e}\n---\n{stdout}")
        }));

    let entries = json
        .as_array()
        .unwrap_or_else(|| panic!("expected JSON array from `vex outline`, got: {json}"));

    for entry in entries {
        let kind = entry["kind"].as_str().unwrap_or("");
        assert_ne!(
            kind, "module",
            "outline must not list entries with kind=module; entry: {entry}"
        );
        let name = entry["name"].as_str().unwrap_or("");
        assert!(
            !name.starts_with("<module:"),
            "outline must not list synthetic Module symbols; entry: {entry}"
        );
    }

    // `create_app` must still appear.
    let has_create_app = entries.iter().any(|e| {
        e["name"]
            .as_str()
            .map(|n| n == "create_app")
            .unwrap_or(false)
    });
    assert!(
        has_create_app,
        "outline must include `create_app`; entries: {entries:?}"
    );
}

// ---------------------------------------------------------------------------
// Test D: adversarial — two create_app definitions + module-scope call
// ---------------------------------------------------------------------------

/// Adversarial case (Q3): a file with two symbols named `create_app` (one
/// method, one top-level function) and a module-scope `app = create_app()`.
///
/// The module-scope call must attribute to `<module:src/factory.py>`, NOT
/// to either `create_app` definition. The test only asserts the Module
/// caller is present; it does not care about other callers.
#[test]
fn module_caller_adversarial_two_definitions() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    // Two definitions named `create_app`: one method, one module-level fn.
    // One module-scope call at the bottom of the file (inside the `if`
    // block — `if __name__ == "__main__":` is still module scope; there is
    // no enclosing function definition).
    std::fs::write(
        tmp.path().join("src").join("factory.py"),
        "class Factory:\n\
         \x20\x20\x20\x20def create_app(self):\n\
         \x20\x20\x20\x20\x20\x20\x20\x20return self\n\
         \n\
         def create_app():\n\
         \x20\x20\x20\x20return Factory()\n\
         \n\
         if __name__ == \"__main__\":\n\
         \x20\x20\x20\x20app = create_app()\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = vex_in(tmp.path())
        .args(["callers", "create_app", "--format", "json"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value =
        unwrap_results(serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!("`vex callers --format json` returned non-JSON: {e}\n---\n{stdout}")
        }));

    let callers = json
        .as_array()
        .unwrap_or_else(|| panic!("expected JSON array from `vex callers`, got: {json}"));

    // At least one caller must be the Module symbol for this file.
    let has_module_caller = callers.iter().any(|c| {
        c["name"]
            .as_str()
            .map(|n| n == "<module:src/factory.py>")
            .unwrap_or(false)
    });
    assert!(
        has_module_caller,
        "module-scope call inside `if __name__` block must surface as \
         `<module:src/factory.py>` caller; callers: {callers:?}"
    );
}

// ---------------------------------------------------------------------------
// Test E: vex usages --strict MUST NOT include module-call edges (negative)
// ---------------------------------------------------------------------------

/// `vex usages create_app --strict` reads the binder `ref_edges` section,
/// NOT call edges. The Module synthetic symbol must therefore never appear
/// in strict-mode usages output.
///
/// This is INTENTIONAL: mixing binder-ref edges and call-graph edges in
/// --strict mode would break the v1.8.x contract for strict usages.
#[test]
fn usages_strict_does_not_include_module_call() {
    let tmp = TempDir::new().unwrap();
    write_app_fixture(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["usages", "create_app", "--strict", "--format", "json"])
        .assert()
        .success(); // Python binder is wired in v1.8.x; the command must
                    // exit 0 so an empty stdout means "no strict refs" and
                    // not "command died before producing output".
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    // If --strict produces JSON, validate the absence of Module.
    if let Ok(envelope) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        let json = unwrap_results(envelope);
        if let Some(arr) = json.as_array() {
            for entry in arr {
                let name = entry["name"].as_str().unwrap_or("");
                assert!(
                    !name.starts_with("<module:"),
                    "usages --strict must not include Module caller edges; \
                     entry: {entry}"
                );
            }
        }
    }

    // If the output is plain text, check for the absence of the Module
    // name pattern regardless (covers both JSON and text --format paths).
    assert!(
        !stdout.contains("<module:src/app.py>"),
        "usages --strict must not surface `<module:src/app.py>`; stdout: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Test F: module symbol excluded from --kind module ranked search
// ---------------------------------------------------------------------------

/// `vex search "" --kind module` must return either an error (if "module"
/// is not a valid --kind argument) or an empty result set. Even if the CLI
/// accepts the filter string, ranked search must skip Module-kind symbols.
#[test]
fn module_symbol_excluded_from_kind_filter() {
    let tmp = TempDir::new().unwrap();
    write_app_fixture(tmp.path());

    // We do not assert success/failure of the command itself — `--kind
    // module` is accepted by FromStr (Phase 14.1 wires it), but ranked
    // search must skip all Module-kind records.
    let out = vex_in(tmp.path())
        .args(["search", "", "--kind", "module", "--format", "json"])
        .output()
        .expect("failed to spawn vex");

    // If the command succeeded, parse stdout and assert no Module hits.
    // The search response is an envelope: { "results": [...], ... }.
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
            for hit in search_results(&json) {
                let name = hit["name"].as_str().unwrap_or("");
                assert!(
                    !name.starts_with("<module:"),
                    "--kind module ranked search must not emit Module symbols; \
                     got: {name}"
                );
            }
        }
    }
    // If the command failed, that is also acceptable (error path for
    // unsupported kind filter). No further assertions needed.
}

// ---------------------------------------------------------------------------
// Test G: module symbol survives vex update (incremental round-trip)
// ---------------------------------------------------------------------------

/// After `vex index` on a single Python file, adding a second file and
/// running `vex update` must not lose the Module caller that was built
/// during the initial index. Incremental reconstruction at
/// `pipeline.rs:285` preserves existing SymbolRecord entries including
/// the synthetic Module records.
#[test]
fn module_symbol_survives_vex_update() {
    let tmp = TempDir::new().unwrap();
    write_app_fixture(tmp.path()); // index src/app.py

    // Add a second file and do an incremental update.
    std::fs::write(
        tmp.path().join("src").join("util.py"),
        "def util():\n    return 1\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["update"]).assert().success();

    // Module caller for `create_app` in `src/app.py` must still be present.
    let assert = vex_in(tmp.path())
        .args(["callers", "create_app", "--format", "json"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value =
        unwrap_results(serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!("`vex callers --format json` after update returned non-JSON: {e}\n---\n{stdout}")
        }));

    let callers = json.as_array().unwrap_or_else(|| {
        panic!("expected JSON array from `vex callers` after update, got: {json}")
    });

    let has_module_caller = callers.iter().any(|c| {
        c["name"]
            .as_str()
            .map(|n| n == "<module:src/app.py>")
            .unwrap_or(false)
    });
    assert!(
        has_module_caller,
        "Module caller for `create_app` must survive `vex update`; callers: {callers:?}"
    );
}

// ---------------------------------------------------------------------------
// Test I: module caller disappears when the module-scope call is removed
// ---------------------------------------------------------------------------

/// H2 regression: incremental update must drop the synthetic Module caller
/// when its only sentinel-producing call site is removed in a code edit.
/// Without this, a stale `<module:path>` row would linger in the call graph
/// after the call it represented is gone.
#[test]
fn module_caller_disappears_when_module_call_removed() {
    let tmp = TempDir::new().unwrap();
    write_app_fixture(tmp.path()); // src/app.py with module-scope `create_app()`

    // Sanity: Module caller present after initial index.
    let initial = vex_in(tmp.path())
        .args(["callers", "create_app", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let initial_json: serde_json::Value = unwrap_results(serde_json::from_slice(&initial).unwrap());
    let had_module = initial_json
        .as_array()
        .map(|a| {
            a.iter()
                .any(|c| c["name"].as_str() == Some("<module:src/app.py>"))
        })
        .unwrap_or(false);
    assert!(had_module, "fixture must start with Module caller present");

    // Edit the file to remove the module-scope call site.
    std::fs::write(
        tmp.path().join("src").join("app.py"),
        "def create_app():\n    return None\n", // no `app = create_app()`
    )
    .unwrap();
    vex_in(tmp.path()).args(["update"]).assert().success();

    // Module caller must be gone.
    let after = vex_in(tmp.path())
        .args(["callers", "create_app", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let after_json: serde_json::Value =
        unwrap_results(serde_json::from_slice(&after).unwrap_or(serde_json::json!({})));
    let still_has_module = after_json
        .as_array()
        .map(|a| {
            a.iter()
                .any(|c| c["name"].as_str() == Some("<module:src/app.py>"))
        })
        .unwrap_or(false);
    assert!(
        !still_has_module,
        "Module caller must disappear after the module-scope call is removed; \
         callers: {after_json:?}"
    );
}
