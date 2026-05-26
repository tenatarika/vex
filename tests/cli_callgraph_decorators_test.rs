//! CLI integration tests for Phase 14.2 — decorator / annotation edges.
//!
//! Each test indexes a tiny tempdir project containing a decorated
//! function (Python) or annotated method (Java) and then asserts via
//! `vex callers --format json` / `vex callees --format json` that the
//! forward edge `decorated_fn → decorator_target` is reachable.
//!
//! In-scope per task file: Python + Java. Kotlin / C# / TypeScript /
//! Rust are deferred to phases 14.2.1 / 14.2.2 — covered by their
//! own integration suites when those land.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// Parse a flat JSON array from `vex callers` / `vex callees`
/// `--format json` stdout. Returns an empty array on parse error so
/// per-test assertions surface as missing entries, not as panics here.
fn parse_json_array(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).unwrap_or(serde_json::json!([]))
}

fn names_of(json: &serde_json::Value) -> Vec<String> {
    json.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Python — FastAPI-style decorator with arguments
// ---------------------------------------------------------------------------

#[test]
fn python_callers_of_get_decorator_lists_handler() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("routes.py"),
        "def setup():\n    pass\n\n@app.get(\"/items\")\ndef list_items():\n    return []\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    // `vex callers get` must surface `list_items` (the decorated function)
    // because `@app.get(...)` produces a forward edge list_items → get.
    let out = vex_in(tmp.path())
        .args(["callers", "get", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse_json_array(&out);
    let names = names_of(&json);
    assert!(
        names.iter().any(|n| n == "list_items"),
        "expected `list_items` in callers(get); got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Python — bare decorator without parens
// ---------------------------------------------------------------------------

#[test]
fn python_callers_of_bare_decorator_lists_handler() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("views.py"),
        "@login_required\ndef profile():\n    return None\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out = vex_in(tmp.path())
        .args(["callers", "login_required", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse_json_array(&out);
    let names = names_of(&json);
    assert!(
        names.iter().any(|n| n == "profile"),
        "expected `profile` in callers(login_required); got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Python — callees of a decorated function include the decorator target
// ---------------------------------------------------------------------------

#[test]
fn python_callees_of_decorated_fn_includes_decorator_target() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("api.py"),
        "@router.post(\"/users\")\ndef create_user():\n    return None\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out = vex_in(tmp.path())
        .args(["callees", "create_user", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse_json_array(&out);
    let names = names_of(&json);
    assert!(
        names.iter().any(|n| n == "post"),
        "expected `post` (rightmost identifier of `@router.post`) in callees(create_user); got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Python — method decorator inside class attributes to the method, not Module
// ---------------------------------------------------------------------------

#[test]
fn python_method_decorator_does_not_leak_to_module_sentinel() {
    // Phase 14.1 coexistence: a method decorator inside a class body
    // must attribute to the method, NOT to the synthetic
    // `<module:path>` caller emitted for module-scope expressions.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("svc.py"),
        "class Service:\n    @staticmethod\n    def helper():\n        return None\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out = vex_in(tmp.path())
        .args(["callers", "staticmethod", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse_json_array(&out);
    let names = names_of(&json);
    assert!(
        names.iter().any(|n| n == "helper"),
        "expected `helper` as caller of staticmethod; got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("<module:")),
        "method decorator must NOT leak to <module:> sentinel; got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Java — Spring-style @GetMapping annotation
// ---------------------------------------------------------------------------

#[test]
fn java_callers_of_get_mapping_lists_handler_method() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("Controller.java"),
        "class Controller {\n    @GetMapping(\"/items\")\n    public Response listItems() {\n        return null;\n    }\n}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out = vex_in(tmp.path())
        .args(["callers", "GetMapping", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse_json_array(&out);
    let names = names_of(&json);
    assert!(
        names.iter().any(|n| n == "listItems"),
        "expected `listItems` in callers(GetMapping); got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Java — @Override marker annotation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Python — module-level decorated fn + module-level caller (14.1 ↔ 14.2)
// ---------------------------------------------------------------------------

/// End-to-end coexistence: a module-level decorated function called
/// at module scope must produce BOTH the 14.2 decorator edge AND the
/// 14.1 sentinel edge in the same index. `vex callers handler` should
/// surface `<module:path>` (14.1); `vex callers get` should surface
/// `handler` (14.2). Pinned at the CLI layer because the two phases
/// touch separate code paths (pipeline.rs sentinel resolution vs
/// callgraph_query decorator pattern) and a regression in either
/// would only surface in production.
#[test]
fn python_module_level_decorated_fn_coexists_at_cli_layer() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("main.py"),
        "@app.get(\"/x\")\ndef handler():\n    return None\n\nresult = handler()\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    // 14.1 sentinel: module-scope call to handler → <module:> caller
    let out_handler = vex_in(tmp.path())
        .args(["callers", "handler", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let names_handler = names_of(&parse_json_array(&out_handler));
    assert!(
        names_handler.iter().any(|n| n.starts_with("<module:")),
        "14.1 sentinel must surface module-scope caller for handler; got: {names_handler:?}"
    );

    // 14.2 decorator edge: handler → get
    let out_get = vex_in(tmp.path())
        .args(["callers", "get", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let names_get = names_of(&parse_json_array(&out_get));
    assert!(
        names_get.iter().any(|n| n == "handler"),
        "14.2 decorator edge must surface handler in callers(get); got: {names_get:?}"
    );
}

// ---------------------------------------------------------------------------
// Java — @Override marker annotation
// ---------------------------------------------------------------------------

#[test]
fn java_callees_of_overridden_method_includes_override() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("Worker.java"),
        "class Worker {\n    @Override\n    public void run() {\n    }\n}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out = vex_in(tmp.path())
        .args(["callees", "run", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse_json_array(&out);
    let names = names_of(&json);
    assert!(
        names.iter().any(|n| n == "Override"),
        "expected `Override` in callees(run); got: {names:?}"
    );
}
