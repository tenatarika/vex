//! CLI integration tests for Phase 14.2 / 14.2.2 — decorator /
//! annotation / attribute edges.
//!
//! Each test indexes a tiny tempdir project containing a decorated
//! function (Python), annotated method (Java / Kotlin), or attributed
//! method (C#) and then asserts via `vex callers --format json` /
//! `vex callees --format json` that the forward edge
//! `decorated_fn → decorator_target` is reachable through the persistent
//! call-graph section.
//!
//! In-scope per task files: Python + Java (Phase 14.2) and Kotlin + C#
//! (Phase 14.2.2). TypeScript + Rust deferred to Phase 14.2.1 (sibling
//! adjacency model) — they'll get their own integration suite when
//! that lands.

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

// ---------------------------------------------------------------------------
// Kotlin — @JvmStatic annotation marker (Phase 14.2.2)
// ---------------------------------------------------------------------------

#[test]
fn kotlin_callers_of_jvm_static_lists_annotated_fn() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(tmp.path().join("Util.kt"), "@JvmStatic\nfun helper() {}\n").unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    // `vex callers JvmStatic` must surface `helper` — the annotated
    // function produces a forward edge `helper → JvmStatic`.
    let out = vex_in(tmp.path())
        .args(["callers", "JvmStatic", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let names = names_of(&parse_json_array(&out));
    // Guard against silent JSON-shape regressions: `parse_json_array`
    // falls back to `[]` on parse failure, which would otherwise let the
    // `any(...)` assertion pass on a broken output envelope.
    assert!(!names.is_empty(), "callers(JvmStatic) must not be empty");
    assert!(
        names.iter().any(|n| n == "helper"),
        "expected `helper` in callers(JvmStatic); got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Kotlin — qualified annotation @kotlin.jvm.JvmStatic (rightmost wins)
// ---------------------------------------------------------------------------

#[test]
fn kotlin_qualified_annotation_uses_rightmost_identifier() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("App.kt"),
        "@kotlin.jvm.JvmStatic\nfun foo() {}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    // Rightmost id `JvmStatic` must reach `foo`.
    let out_right = vex_in(tmp.path())
        .args(["callers", "JvmStatic", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let names_right = names_of(&parse_json_array(&out_right));
    assert!(
        !names_right.is_empty(),
        "callers(JvmStatic) must not be empty"
    );
    assert!(
        names_right.iter().any(|n| n == "foo"),
        "rightmost `JvmStatic` must list `foo` as caller; got: {names_right:?}"
    );

    // Intermediate segments `kotlin` / `jvm` must NOT have `foo` as caller —
    // pins the trailing `.` anchor on `(user_type (identifier) @x .)`.
    for intermediate in ["kotlin", "jvm"] {
        let out = vex_in(tmp.path())
            .args(["callers", intermediate, "--format", "json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let names = names_of(&parse_json_array(&out));
        assert!(
            !names.iter().any(|n| n == "foo"),
            "intermediate `{intermediate}` must NOT list `foo` as caller; got: {names:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Kotlin — member access call: obj.helper() → callees(caller) ∋ helper
// ---------------------------------------------------------------------------

#[test]
fn kotlin_callees_of_navigation_expression_uses_rightmost() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("Caller.kt"),
        "fun caller() {\n    obj.helper()\n}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out = vex_in(tmp.path())
        .args(["callees", "caller", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let names = names_of(&parse_json_array(&out));
    assert!(!names.is_empty(), "callees(caller) must not be empty");
    assert!(
        names.iter().any(|n| n == "helper"),
        "expected `helper` (rightmost of navigation_expression) in callees(caller); got: {names:?}"
    );
    // LOW from code-reviewer: pin that the receiver `obj` is NOT also
    // captured by the bare `(call_expression (identifier) @call.name)`
    // pattern. Currently safe because navigation_expression sits between
    // call_expression and obj, but worth fencing against grammar drift.
    assert!(
        !names.iter().any(|n| n == "obj"),
        "receiver identifier `obj` must NOT appear as a callee: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// C# — [HttpGet("/users")] attribute (Phase 14.2.2)
// ---------------------------------------------------------------------------

#[test]
fn csharp_callers_of_http_get_lists_handler_method() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("UsersController.cs"),
        "class UsersController {\n    [HttpGet(\"/users\")]\n    public Response GetUsers() {\n        return null;\n    }\n}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out = vex_in(tmp.path())
        .args(["callers", "HttpGet", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let names = names_of(&parse_json_array(&out));
    assert!(!names.is_empty(), "callers(HttpGet) must not be empty");
    assert!(
        names.iter().any(|n| n == "GetUsers"),
        "expected `GetUsers` in callers(HttpGet); got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// C# — qualified attribute [System.Web.Mvc.HttpGet] (rightmost wins)
// ---------------------------------------------------------------------------

#[test]
fn csharp_qualified_attribute_uses_rightmost_identifier() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("Api.cs"),
        "class Api {\n    [System.Web.Mvc.HttpGet]\n    public Response Get() {\n        return null;\n    }\n}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out_right = vex_in(tmp.path())
        .args(["callers", "HttpGet", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let names_right = names_of(&parse_json_array(&out_right));
    assert!(
        !names_right.is_empty(),
        "callers(HttpGet) must not be empty"
    );
    assert!(
        names_right.iter().any(|n| n == "Get"),
        "rightmost `HttpGet` must list `Get` as caller; got: {names_right:?}"
    );

    // Intermediate segments must NOT have `Get` as caller — pins the
    // `qualified_name name: (identifier)` rightmost-wins walk.
    for intermediate in ["System", "Web", "Mvc"] {
        let out = vex_in(tmp.path())
            .args(["callers", intermediate, "--format", "json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let names = names_of(&parse_json_array(&out));
        assert!(
            !names.iter().any(|n| n == "Get"),
            "intermediate `{intermediate}` must NOT list `Get` as caller; got: {names:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// C# — constructor declaration produces a FnDef caller
// ---------------------------------------------------------------------------

#[test]
fn csharp_constructor_appears_as_caller_of_body_call() {
    // Pins `constructor_declaration name: (identifier) @fn.name` —
    // without it, the constructor body would attribute call sites to
    // the synthetic <module:> sentinel instead.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("Service.cs"),
        "class Service {\n    public Service() {\n        Init();\n    }\n\n    void Init() {}\n}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out = vex_in(tmp.path())
        .args(["callers", "Init", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let names = names_of(&parse_json_array(&out));
    assert!(!names.is_empty(), "callers(Init) must not be empty");
    assert!(
        names.iter().any(|n| n == "Service"),
        "constructor `Service` must appear as caller of `Init`; got: {names:?}"
    );
}
