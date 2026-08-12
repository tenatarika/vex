#![cfg(test)]

use super::*;
use crate::parse::language::Language;

fn callers(src: &str, lang: Language, target: &str) -> Vec<CallMatch> {
    callers_in_source(src, lang, "test", target)
}

fn callees(src: &str, lang: Language, target: &str) -> Vec<CallMatch> {
    callees_in_source(src, lang, "test", target)
}

/// Shared-tree equivalence for call-edge extraction
/// (`.claude/Task/PERF-parse-once-shared-tree.md`, commit 4).
///
/// `parse_file` calls `extract_call_edges_with_tree` with a tree it parsed for
/// every language, including the 11 with no callgraph query at all. The split
/// happens at the private `extract_callgraph`, so the two live-scan entry points
/// (`callers_in_source` / `callees_in_source`) keep their self-parsing path —
/// this diffs the index-time core against the public self-parsing one.
#[test]
fn with_tree_matches_the_self_parsing_entry_point_for_every_language() {
    let fixtures: &[(Language, &str)] = &[
        (Language::Rust, "rs"),
        (Language::Kotlin, "kt"),
        (Language::TypeScript, "ts"),
        (Language::Python, "py"),
        (Language::Go, "go"),
        (Language::Java, "java"),
        (Language::CSharp, "cs"),
        (Language::Cpp, "cpp"),
        (Language::Ruby, "rb"),
        (Language::Swift, "swift"),
        (Language::Php, "php"),
        (Language::Sql, "sql"),
        (Language::Markdown, "md"),
        (Language::Css, "css"),
        (Language::Html, "html"),
        (Language::Bash, "sh"),
        (Language::Lua, "lua"),
        (Language::Yaml, "yaml"),
        (Language::Toml, "toml"),
    ];
    assert_eq!(
        fixtures.len(),
        Language::ALL.len(),
        "every language must be covered"
    );

    for &(lang, ext) in fixtures {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/sample.{ext}"));
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));

        let want = extract_call_edges(&content, lang);
        let tree = crate::parse::parser_pool::parse_text(lang, &content).expect("parse fixture");
        let got = extractor::extract_call_edges_with_tree(&tree, &content, lang);

        assert_eq!(
            got, want,
            "{lang:?}: shared-tree core disagrees with extract_call_edges"
        );
    }
}

#[test]
fn rust_callers() {
    let src = r#"
fn process() {
    validate();
    transform();
}

fn run() {
    process();
    cleanup();
}

fn validate() {}
fn transform() {}
fn cleanup() {}
"#;
    let matches = callers(src, Language::Rust, "process");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "run");
}

#[test]
fn rust_callees() {
    let src = r#"
fn process() {
    validate();
    transform();
    log_result();
}

fn validate() {}
fn transform() {}
fn log_result() {}
"#;
    let matches = callees(src, Language::Rust, "process");
    assert_eq!(matches.len(), 3);
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"validate"));
    assert!(names.contains(&"transform"));
    assert!(names.contains(&"log_result"));
}

#[test]
fn rust_method_calls() {
    let src = r#"
fn process(data: &Data) {
    data.validate();
    data.transform();
}
"#;
    let matches = callees(src, Language::Rust, "process");
    assert_eq!(matches.len(), 2);
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"validate"));
    assert!(names.contains(&"transform"));
}

#[test]
fn python_callers_and_callees() {
    let src = r#"
def process():
    validate()
    transform()

def run():
    process()
    cleanup()

def validate():
    pass

def transform():
    pass

def cleanup():
    pass
"#;
    let caller_matches = callers(src, Language::Python, "process");
    assert_eq!(caller_matches.len(), 1);
    assert_eq!(caller_matches[0].name, "run");

    let callee_matches = callees(src, Language::Python, "process");
    assert_eq!(callee_matches.len(), 2);
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"validate"));
    assert!(names.contains(&"transform"));
}

#[test]
fn go_callers() {
    let src = r#"
package main

func Process() {
    Validate()
}

func Run() {
    Process()
}

func Validate() {}
"#;
    let matches = callers(src, Language::Go, "Process");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "Run");
}

#[test]
fn no_callers_returns_empty() {
    let src = "fn main() {}\nfn unused() {}";
    let matches = callers(src, Language::Rust, "unused");
    assert!(matches.is_empty());
}

#[test]
fn callees_excludes_self_recursion() {
    let src = r#"
fn factorial(n: u32) -> u32 {
    if n == 0 { 1 } else { n * factorial(n - 1) }
}
"#;
    let matches = callees(src, Language::Rust, "factorial");
    // factorial calls itself — excluded by the callee != target filter
    assert!(matches.is_empty());
}

#[test]
fn typescript_class_methods() {
    let src = r#"
class Service {
    process() {
        this.validate();
        this.transform();
    }

    validate() {}
    transform() {}
}

function main() {
    const svc = new Service();
    svc.process();
}
"#;
    let caller_matches = callers(src, Language::TypeScript, "process");
    assert_eq!(caller_matches.len(), 1);
    assert_eq!(caller_matches[0].name, "main");

    let callee_matches = callees(src, Language::TypeScript, "process");
    assert_eq!(callee_matches.len(), 2);
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"validate"));
    assert!(names.contains(&"transform"));
}

#[test]
fn unsupported_language_returns_empty() {
    let matches = callers("CREATE TABLE foo (id INT);", Language::Sql, "foo");
    assert!(matches.is_empty());
}

// ---- Phase 14.2 decorator edges — Python (RED) -------------------------

/// `@app.get("/x")` decorator → `vex callees list_items` includes `get`
/// (rightmost identifier in the decorator target path) AND
/// `vex callers get` includes `list_items`.
#[test]
fn python_decorator_with_attribute_target() {
    let src = r#"
def list_items():
    pass

@app.get("/x")
def fetch_one():
    pass
"#;
    let callee_matches = callees(src, Language::Python, "fetch_one");
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"get"),
        "expected `get` in callees(fetch_one), got: {names:?}"
    );

    let caller_matches = callers(src, Language::Python, "get");
    let caller_names: Vec<&str> = caller_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        caller_names.contains(&"fetch_one"),
        "expected `fetch_one` in callers(get), got: {caller_names:?}"
    );
}

/// Bare-name decorator without parens: `@login_required def view(): ...`
/// (no call wrapping; just an identifier reference).
#[test]
fn python_bare_decorator() {
    let src = r#"
@login_required
def view():
    pass
"#;
    let callee_matches = callees(src, Language::Python, "view");
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"login_required"),
        "expected `login_required` in callees(view), got: {names:?}"
    );
}

/// Bare-attribute decorator: `@app.router def f(): ...` — no call,
/// just an attribute expression. Rightmost identifier wins.
#[test]
fn python_bare_attribute_decorator() {
    let src = r#"
@app.router
def f():
    pass
"#;
    let callee_matches = callees(src, Language::Python, "f");
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"router"),
        "expected `router` in callees(f), got: {names:?}"
    );
}

/// Multi-decorator stack must produce exactly one edge per decorator.
/// Locks the OUTERMOST-FIRST ordering invariant from the task file —
/// when sorted by line, the outer decorator (smaller line number)
/// comes before the inner.
#[test]
fn python_multi_decorator_stack_outermost_first() {
    let src = r#"
@outer_dec
@inner_dec
def handler():
    pass
"#;
    let mut callee_matches = callees(src, Language::Python, "handler");
    callee_matches.sort_by_key(|m| m.line);
    let pairs: Vec<(&str, usize)> = callee_matches
        .iter()
        .map(|m| (m.name.as_str(), m.line))
        .collect();
    // Filter to just decorator edges (exclude any noise from grammar
    // quirks if extra captures appear).
    let decorator_pairs: Vec<&(&str, usize)> = pairs
        .iter()
        .filter(|(n, _)| *n == "outer_dec" || *n == "inner_dec")
        .collect();
    assert_eq!(
        decorator_pairs.len(),
        2,
        "expected exactly 2 decorator edges (outer_dec, inner_dec), got: {pairs:?}"
    );
    assert_eq!(
        decorator_pairs[0].0, "outer_dec",
        "outermost decorator must come first by line: {pairs:?}"
    );
    assert_eq!(decorator_pairs[1].0, "inner_dec");
    assert!(
        decorator_pairs[0].1 < decorator_pairs[1].1,
        "outer line must be < inner line: {pairs:?}"
    );
}

/// `edge.line` must point at the decorator's source line, NOT the
/// `def` line. Locks the per-task-file invariant.
#[test]
fn python_decorator_edge_line_is_decorator_not_def() {
    // Decorator on line 2, def on line 3. The edge line MUST be 2.
    let src = "\n@my_decorator\ndef target():\n    pass\n";
    let callee_matches = callees(src, Language::Python, "target");
    let dec_edge = callee_matches
        .iter()
        .find(|m| m.name == "my_decorator")
        .unwrap_or_else(|| {
            panic!("expected `my_decorator` edge from `target`, got: {callee_matches:?}")
        });
    assert_eq!(
        dec_edge.line, 2,
        "edge.line must be the decorator's row (2), not the def row (3); got: {dec_edge:?}"
    );
}

/// Method decorator inside a class body must attribute to the method,
/// NOT to the module sentinel (Phase 14.1 coexistence invariant).
/// The class-as-FnDef is out of scope (Phase 14.6) — but the method
/// IS a FnDef and must own its decorator edge.
#[test]
fn python_method_decorator_in_class_attributes_to_method() {
    let src = r#"
class C:
    @staticmethod
    def helper():
        pass
"#;
    let callee_matches = callees(src, Language::Python, "helper");
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"staticmethod"),
        "method decorator must attribute to enclosing method, got: {names:?}"
    );

    // Negative: `staticmethod` must NOT appear in any other fn's callees,
    // because there's only one decorated method here.
    let caller_matches = callers(src, Language::Python, "staticmethod");
    let caller_names: Vec<&str> = caller_matches.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        caller_names,
        vec!["helper"],
        "staticmethod must have exactly one caller (`helper`), got: {caller_names:?}"
    );
}

// ---- Phase 14.2 decorator edges — Java (RED) ---------------------------

/// `@Override` marker annotation → callees(run) includes `Override`,
/// callers(Override) includes `run`. No call args.
#[test]
fn java_marker_annotation() {
    let src = r#"
class Worker {
    @Override
    public void run() {
    }
}
"#;
    let callee_matches = callees(src, Language::Java, "run");
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"Override"),
        "expected `Override` in callees(run), got: {names:?}"
    );

    let caller_matches = callers(src, Language::Java, "Override");
    let caller_names: Vec<&str> = caller_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        caller_names.contains(&"run"),
        "expected `run` in callers(Override), got: {caller_names:?}"
    );
}

/// `@GetMapping("/x")` annotation with arguments → emits edge to
/// the rightmost identifier (`GetMapping`), same convention as
/// method calls.
#[test]
fn java_annotation_with_arguments() {
    let src = r#"
class Controller {
    @GetMapping("/items")
    public Response listItems() {
        return null;
    }
}
"#;
    let callee_matches = callees(src, Language::Java, "listItems");
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"GetMapping"),
        "expected `GetMapping` in callees(listItems), got: {names:?}"
    );
}

/// Qualified annotation name `@org.junit.Test` must capture the
/// rightmost identifier (`Test`), matching the existing method-call
/// convention.
#[test]
fn java_qualified_annotation_rightmost_identifier() {
    let src = r#"
class MyTest {
    @org.junit.Test
    public void shouldDoIt() {
    }
}
"#;
    let callee_matches = callees(src, Language::Java, "shouldDoIt");
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
            names.contains(&"Test"),
            "expected rightmost identifier `Test` (not `org` or `junit`) in callees(shouldDoIt), got: {names:?}"
        );
    assert!(
        !names.contains(&"org") && !names.contains(&"junit"),
        "intermediate path identifiers must not appear as edges: {names:?}"
    );
}

/// Multi-annotation stack on a single method produces one edge per
/// annotation. Java has no defined stacking order beyond source-order,
/// so we assert presence + outer-first by line.
#[test]
fn java_multi_annotation_stack() {
    let src = r#"
class Worker {
    @Override
    @Deprecated
    public void run() {
    }
}
"#;
    let mut callee_matches = callees(src, Language::Java, "run");
    callee_matches.sort_by_key(|m| m.line);
    let annotation_pairs: Vec<(&str, usize)> = callee_matches
        .iter()
        .filter(|m| m.name == "Override" || m.name == "Deprecated")
        .map(|m| (m.name.as_str(), m.line))
        .collect();
    assert_eq!(
        annotation_pairs.len(),
        2,
        "expected exactly 2 annotation edges, got: {callee_matches:?}"
    );
    assert_eq!(annotation_pairs[0].0, "Override");
    assert_eq!(annotation_pairs[1].0, "Deprecated");
}

// ---- Phase 14.2 decorator edges — back to Python -----------------------

/// Phase 14.2 review regression: `callees_in_source` MUST use the
/// LARGEST FnDef range when the same name has multiple entries
/// (the Double FnDef invariant). With min/find-first selection,
/// the inner `function_definition` span would be picked — but its
/// byte range starts AFTER the decorator, so decorator calls
/// would fall outside and be silently dropped from callees output.
/// Pin both: decorator edge IS in callees, AND body call IS too.
#[test]
fn python_callees_includes_both_decorator_and_body_calls() {
    let src = r#"
@app.get("/x")
def handler():
    validate()
"#;
    let matches = callees(src, Language::Python, "handler");
    let callee_names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        callee_names.contains(&"get"),
        "decorator edge `handler → get` must appear in callees (max-range FnDef \
             covers the decorator byte range): {callee_names:?}"
    );
    assert!(
        callee_names.contains(&"validate"),
        "body call `handler → validate` must coexist with decorator edge: {callee_names:?}"
    );
}

/// Phase 14.1 ↔ 14.2 coexistence: a module-level call to a
/// module-level decorated function must hit BOTH:
/// - the decorator edge `handler → get` (14.2)
/// - the sentinel edge `<empty> → handler` from the module-scope
///   call site `result = handler()` (14.1 sentinel; pipeline
///   rewrites the empty caller to `<module:path>`)
///
/// Both edges must be emitted independently — pinned by
/// `extract_call_edges` direct inspection.
#[test]
fn python_module_level_call_to_decorated_fn_coexists_with_decorator_edge() {
    let src = r#"
@app.get("/x")
def handler():
    pass

result = handler()
"#;
    let edges = extract_call_edges(src, Language::Python);
    // Decorator edge from 14.2: handler → get
    assert!(
        edges
            .iter()
            .any(|(caller, _, callee, _)| caller == "handler" && callee == "get"),
        "expected decorator edge `handler → get`, got: {edges:?}"
    );
    // Sentinel edge from 14.1: module-scope call → handler
    // (sentinel = caller_fn_name.is_empty() && caller_fn_line == 0)
    assert!(
        edges
            .iter()
            .any(|(caller, line, callee, _)| caller.is_empty()
                && *line == 0
                && callee == "handler"),
        "expected module-scope sentinel edge `<empty> → handler`, got: {edges:?}"
    );
}

/// Decorator factory (`@functools.lru_cache(maxsize=128)`) — the
/// outermost call wraps a real-decorator-returning function. Per
/// the rightmost-identifier convention, we emit edge `f → lru_cache`.
/// Pinned because it's a common pattern (`@click.command()`,
/// `@retry(max=3)`, `@app.route("/x")`).
#[test]
fn python_decorator_factory_with_kwarg() {
    let src = r#"
@functools.lru_cache(maxsize=128)
def memoized():
    return None
"#;
    let matches = callees(src, Language::Python, "memoized");
    let callee_names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        callee_names.contains(&"lru_cache"),
        "decorator factory edge `memoized → lru_cache` expected: {callee_names:?}"
    );
}

/// Calls inside the decorated function's BODY must still attribute to
/// the function (not the outer decorated_definition wrapping range).
/// This pins the "double FnDef invariant" from the task file:
/// `min_by_key(end - start)` picks the inner function_definition span
/// over the outer decorated_definition span for body calls.
#[test]
fn python_calls_in_decorated_body_attribute_to_inner_fn() {
    let src = r#"
@app.get("/x")
def handler():
    validate()
    transform()
"#;
    let validate_callers = callers(src, Language::Python, "validate");
    let names: Vec<&str> = validate_callers.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
            names,
            vec!["handler"],
            "body call must attribute to handler exactly once (inner FnDef wins via min_by_key): {names:?}"
        );

    // The decorator edge for `get` is independent — still present.
    let get_callers = callers(src, Language::Python, "get");
    let get_names: Vec<&str> = get_callers.iter().map(|m| m.name.as_str()).collect();
    assert!(
        get_names.contains(&"handler"),
        "decorator edge for `get` must coexist with body-call edges: {get_names:?}"
    );
}

// ---- Phase 14.6 — class-level decorator / annotation edges -----------

/// `@dataclass class Foo:` — bare-identifier class decorator. The
/// callee `dataclass` must be captured and attributed to the
/// module-scope sentinel (caller_fn_name="", caller_fn_line=0)
/// because no FnDef encloses the decorator. The pipeline rewrites
/// the sentinel to the synthetic `<module:path>` symbol via
/// `parse_file`.
#[test]
fn python_class_decorator_bare_identifier_emits_module_scope_edge() {
    let src = r#"
@dataclass
class Foo:
    pass
"#;
    let edges = extract_call_edges(src, Language::Python);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "dataclass"
        }),
        "expected sentinel edge for `dataclass` (caller=\"\", line=0); got: {edges:?}"
    );
}

/// `@routes.cbv class Foo:` — bare-attribute class decorator.
/// Rightmost identifier wins, attributed to module scope.
#[test]
fn python_class_decorator_bare_attribute_emits_module_scope_edge() {
    let src = r#"
@routes.cbv
class Foo:
    pass
"#;
    let edges = extract_call_edges(src, Language::Python);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "cbv"
        }),
        "expected sentinel edge for `cbv` (caller=\"\", line=0); got: {edges:?}"
    );
}

/// `@app.route("/x") class Bar:` — call-shape class decorator was
/// ALREADY caught by the generic `(call function: ...)` patterns
/// before Phase 14.6. This test pins that the existing behaviour
/// survives the 14.6 patterns being added: the rightmost identifier
/// `route` must still emit a sentinel edge.
#[test]
fn python_class_decorator_call_shape_still_attributes_to_module_scope() {
    let src = r#"
@app.route("/x")
class Bar:
    pass
"#;
    let edges = extract_call_edges(src, Language::Python);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "route"
        }),
        "expected sentinel edge for `route` (caller=\"\", line=0); got: {edges:?}"
    );
}

/// Class-level decorators must NOT spuriously emit a `<module:path>`
/// edge for the function-decorator case — that scenario already has
/// a proper FnDef anchor via Phase 14.2's `@fn.decl`. Locks the
/// invariant that function-decorator and class-decorator patterns
/// stay disjoint after the 14.6 additions.
#[test]
fn python_function_decorator_still_attributes_to_function_not_module() {
    let src = r#"
@login_required
def view():
    pass
"#;
    let edges = extract_call_edges(src, Language::Python);
    let view_edges: Vec<_> = edges
        .iter()
        .filter(|(_, _, callee, _)| callee == "login_required")
        .collect();
    assert!(
        !view_edges.is_empty(),
        "function decorator edge must still fire: {edges:?}"
    );
    assert!(
        view_edges.iter().all(|(caller, _, _, _)| caller == "view"),
        "function decorator must attribute to `view`, not module scope: {view_edges:?}"
    );
}

/// Java `@Component class Foo {}` — bare marker_annotation on class
/// emits a module-scope sentinel edge for `Component`.
#[test]
fn java_class_annotation_bare_marker_emits_module_scope_edge() {
    let src = r#"
@Component
class Foo {}
"#;
    let edges = extract_call_edges(src, Language::Java);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "Component"
        }),
        "expected sentinel edge for `Component`; got: {edges:?}"
    );
}

/// Java `@org.springframework.RestController class Foo {}` — scoped
/// marker_annotation, rightmost identifier wins.
#[test]
fn java_class_annotation_scoped_marker_emits_module_scope_edge() {
    let src = r#"
@org.springframework.RestController
class Foo {}
"#;
    let edges = extract_call_edges(src, Language::Java);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "RestController"
        }),
        "expected sentinel edge for `RestController` (rightmost); got: {edges:?}"
    );
}

/// Java `@Service("x") class Foo {}` — annotation-with-args on class.
#[test]
fn java_class_annotation_with_args_emits_module_scope_edge() {
    let src = r#"
@Service("x")
class Foo {}
"#;
    let edges = extract_call_edges(src, Language::Java);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "Service"
        }),
        "expected sentinel edge for `Service`; got: {edges:?}"
    );
}

/// Java mixing class- and method-level annotations: the method-level
/// `@Override` must still attribute to the method (Phase 14.2), while
/// the class-level `@Component` falls to module scope (Phase 14.6).
/// Locks the disjointness of the two patterns.
#[test]
fn java_class_and_method_annotations_have_disjoint_attribution() {
    let src = r#"
@Component
class Foo {
    @Override
    public String toString() { return ""; }
}
"#;
    let edges = extract_call_edges(src, Language::Java);

    let component_edges: Vec<_> = edges
        .iter()
        .filter(|(_, _, callee, _)| callee == "Component")
        .collect();
    assert!(
        component_edges
            .iter()
            .all(|(caller, _, _, _)| caller.is_empty()),
        "class-level @Component must attribute to module scope; got: {component_edges:?}"
    );

    let override_edges: Vec<_> = edges
        .iter()
        .filter(|(_, _, callee, _)| callee == "Override")
        .collect();
    assert!(
        override_edges
            .iter()
            .all(|(caller, _, _, _)| caller == "toString"),
        "method-level @Override must attribute to `toString`; got: {override_edges:?}"
    );
}

/// C# `[ApiController] class Foo {}` — bare attribute on class.
#[test]
fn csharp_class_attribute_bare_emits_module_scope_edge() {
    let src = r#"
[ApiController]
class Foo {}
"#;
    let edges = extract_call_edges(src, Language::CSharp);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "ApiController"
        }),
        "expected sentinel edge for `ApiController`; got: {edges:?}"
    );
}

/// C# `[System.Web.Mvc.Authorize] class Foo {}` — qualified attribute,
/// rightmost identifier wins.
#[test]
fn csharp_class_attribute_qualified_emits_module_scope_edge() {
    let src = r#"
[System.Web.Mvc.Authorize]
class Foo {}
"#;
    let edges = extract_call_edges(src, Language::CSharp);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "Authorize"
        }),
        "expected sentinel edge for `Authorize` (rightmost); got: {edges:?}"
    );
}

/// Kotlin `@JvmStatic class Foo` — bare annotation on class.
#[test]
fn kotlin_class_annotation_bare_emits_module_scope_edge() {
    let src = r#"
@JvmStatic
class Foo
"#;
    let edges = extract_call_edges(src, Language::Kotlin);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "JvmStatic"
        }),
        "expected sentinel edge for `JvmStatic`; got: {edges:?}"
    );
}

/// Kotlin `@kotlin.jvm.JvmStatic class Foo` — qualified annotation on
/// class. Rightmost identifier wins (`JvmStatic`); intermediate segments
/// `kotlin` / `jvm` MUST NOT leak as separate sentinel edges. Mirrors
/// `csharp_class_attribute_qualified_emits_module_scope_edge` and is the
/// class-level analogue of `kotlin_qualified_annotation_rightmost`
/// (which covers the same shape on a function).
#[test]
fn kotlin_class_annotation_qualified_emits_module_scope_edge() {
    let src = r#"
@kotlin.jvm.JvmStatic
class Foo
"#;
    let edges = extract_call_edges(src, Language::Kotlin);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "JvmStatic"
        }),
        "expected sentinel edge for rightmost `JvmStatic`; got: {edges:?}"
    );
    for leak in ["kotlin", "jvm"] {
        assert!(
            !edges
                .iter()
                .any(|(_, _, callee, _)| callee.as_str() == leak),
            "intermediate path identifier `{leak}` must not leak as a separate edge: {edges:?}"
        );
    }
}

/// Kotlin `@Component("x") class Foo` — constructor_invocation form.
#[test]
fn kotlin_class_annotation_with_args_emits_module_scope_edge() {
    let src = r#"
@Component("x")
class Foo
"#;
    let edges = extract_call_edges(src, Language::Kotlin);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "Component"
        }),
        "expected sentinel edge for `Component`; got: {edges:?}"
    );
}

/// TypeScript `@Component class Foo {}` — bare identifier class
/// decorator emits module-scope sentinel.
#[test]
fn typescript_class_decorator_bare_emits_module_scope_edge() {
    let src = r#"
@Component
class Foo {}
"#;
    let edges = extract_call_edges(src, Language::TypeScript);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "Component"
        }),
        "expected sentinel edge for `Component`; got: {edges:?}"
    );
}

/// TypeScript `@Module({...}) class Foo {}` — call-shape with
/// identifier function.
#[test]
fn typescript_class_decorator_call_shape_emits_module_scope_edge() {
    let src = r#"
@Module({})
class Foo {}
"#;
    let edges = extract_call_edges(src, Language::TypeScript);
    assert!(
        edges.iter().any(|(caller, line, callee, _)| {
            caller.is_empty() && *line == 0 && callee == "Module"
        }),
        "expected sentinel edge for `Module`; got: {edges:?}"
    );
}

// ---- Phase 14.2.2 — Kotlin base callgraph + annotations (RED) ---------

/// Basic Kotlin call: `fun foo() { bar() }` — caller is `foo`,
/// callee is `bar`. Pins the `function_declaration` capture +
/// `call_expression`-via-`identifier` capture.
#[test]
fn kotlin_basic_callers_and_callees() {
    let src = r#"
fun bar() {}

fun foo() {
    bar()
}
"#;
    let caller_matches = callers(src, Language::Kotlin, "bar");
    assert_eq!(caller_matches.len(), 1, "callers(bar): {caller_matches:?}");
    assert_eq!(caller_matches[0].name, "foo");

    let callee_matches = callees(src, Language::Kotlin, "foo");
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"bar"), "callees(foo): {names:?}");
}

/// `obj.method()` — rightmost identifier in `navigation_expression`
/// wins. Mirrors the convention used by Java / TypeScript.
#[test]
fn kotlin_member_access_call() {
    let src = r#"
fun caller() {
    obj.helper()
}
"#;
    let matches = callees(src, Language::Kotlin, "caller");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"helper"),
        "navigation_expression rightmost id must be captured: {names:?}"
    );
    // Receiver `obj` must NOT appear as a callee. The bare
    // `(call_expression (identifier) @call.name)` pattern only fires
    // when the direct child of `call_expression` is an `identifier`;
    // for `obj.helper()` the direct child is `navigation_expression`
    // so the bare pattern is skipped. Pin this against grammar drift.
    assert!(
        !names.contains(&"obj"),
        "receiver identifier `obj` must not appear as callee: {names:?}"
    );
}

/// `@JvmStatic fun foo()` — marker annotation. Edge `foo → JvmStatic`.
#[test]
fn kotlin_annotation_marker() {
    let src = r#"
@JvmStatic
fun foo() {}
"#;
    let matches = callees(src, Language::Kotlin, "foo");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"JvmStatic"),
        "callees(foo) must include `JvmStatic`: {names:?}"
    );
}

/// Multi-annotation stack. Two edges, outer-first by line.
#[test]
fn kotlin_multi_annotation_stack() {
    let src = r#"
@Inject
@Named("svc")
fun setService() {}
"#;
    let mut matches = callees(src, Language::Kotlin, "setService");
    matches.sort_by_key(|m| m.line);
    let pairs: Vec<(&str, usize)> = matches
        .iter()
        .filter(|m| m.name == "Inject" || m.name == "Named")
        .map(|m| (m.name.as_str(), m.line))
        .collect();
    assert_eq!(
        pairs.len(),
        2,
        "expected exactly 2 annotation edges, got: {matches:?}"
    );
    assert_eq!(pairs[0].0, "Inject", "outermost annotation must come first");
}

/// Qualified annotation `@kotlin.jvm.JvmStatic` — rightmost wins
/// (`JvmStatic`), intermediate `kotlin`/`jvm` MUST NOT appear.
#[test]
fn kotlin_qualified_annotation_rightmost() {
    let src = r#"
@kotlin.jvm.JvmStatic
fun foo() {}
"#;
    let matches = callees(src, Language::Kotlin, "foo");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"JvmStatic"),
        "rightmost `JvmStatic` must appear: {names:?}"
    );
    assert!(
        !names.contains(&"kotlin") && !names.contains(&"jvm"),
        "intermediate path identifiers must not leak: {names:?}"
    );
}

// ---- Phase 14.2.2 — C# base callgraph + attributes (RED) --------------

/// Basic C# invocation: `void Foo() { Bar(); }`. Caller is `Foo`,
/// callee `Bar`.
#[test]
fn csharp_basic_callers_and_callees() {
    let src = r#"
class App {
    void Bar() {}

    void Foo() {
        Bar();
    }
}
"#;
    let caller_matches = callers(src, Language::CSharp, "Bar");
    assert_eq!(caller_matches.len(), 1, "callers(Bar): {caller_matches:?}");
    assert_eq!(caller_matches[0].name, "Foo");

    let callee_matches = callees(src, Language::CSharp, "Foo");
    let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"Bar"), "callees(Foo): {names:?}");
}

/// `obj.Method()` — `member_access_expression name:` field gives
/// the rightmost identifier as the callee.
#[test]
fn csharp_member_access_invocation() {
    let src = r#"
class App {
    void Caller() {
        obj.Helper();
    }
}
"#;
    let matches = callees(src, Language::CSharp, "Caller");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"Helper"),
        "rightmost id of member_access_expression: {names:?}"
    );
}

/// Constructor declarations produce a FnDef. `Foo` constructor
/// calling `Init()` from its body is captured via `callees(Foo)`.
#[test]
fn csharp_constructor_indexed() {
    let src = r#"
class App {
    public App() {
        Init();
    }

    void Init() {}
}
"#;
    let matches = callees(src, Language::CSharp, "App");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"Init"),
        "constructor body call must be a callee of the constructor: {names:?}"
    );
}

/// `[HttpGet("/users")]` attribute → callee `HttpGet`.
#[test]
fn csharp_attribute_marker() {
    let src = r#"
class Controller {
    [HttpGet("/users")]
    public Response GetUsers() {
        return null;
    }
}
"#;
    let matches = callees(src, Language::CSharp, "GetUsers");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"HttpGet"),
        "method attribute must produce edge to `HttpGet`: {names:?}"
    );
}

/// Multi-attribute on one method: `[Route("/x")] [Authorize]`.
#[test]
fn csharp_multi_attribute_stack() {
    let src = r#"
class Controller {
    [Route("/x")]
    [Authorize]
    public Response Handle() {
        return null;
    }
}
"#;
    let matches = callees(src, Language::CSharp, "Handle");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"Route") && names.contains(&"Authorize"),
        "both attribute edges expected: {names:?}"
    );
}

/// 2-level qualified attribute `[System.HttpGet]` — at this depth
/// the outer `attribute` `name:` field IS a `qualified_name` (so the
/// bare-identifier branch does NOT fire), but the qualified branch's
/// `name: (identifier)` capture still resolves to the rightmost leaf
/// `HttpGet`. Pins the qualified branch on the minimum-depth case
/// because the 3-level test below has more redundancy.
#[test]
fn csharp_two_level_qualified_attribute_rightmost() {
    let src = r#"
class Controller {
    [System.HttpGet]
    public Response Handle() {
        return null;
    }
}
"#;
    let matches = callees(src, Language::CSharp, "Handle");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"HttpGet"),
        "rightmost `HttpGet` of 2-level qualified attribute must appear: {names:?}"
    );
    assert!(
        !names.contains(&"System"),
        "intermediate `System` must not leak: {names:?}"
    );
}

/// Qualified attribute name `[System.Web.Mvc.HttpGet]` — rightmost
/// wins, intermediates do NOT appear.
#[test]
fn csharp_qualified_attribute_rightmost() {
    let src = r#"
class Controller {
    [System.Web.Mvc.HttpGet]
    public Response Get() {
        return null;
    }
}
"#;
    let matches = callees(src, Language::CSharp, "Get");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"HttpGet"),
        "rightmost `HttpGet` must appear: {names:?}"
    );
    assert!(
        !names.contains(&"System") && !names.contains(&"Web") && !names.contains(&"Mvc"),
        "intermediate qualified-name segments must not leak: {names:?}"
    );
}

// ---- Phase 14.2.1 — TypeScript decorator edges (RED) -----------------

/// Persistent-index path equivalence: `extract_call_edges` must
/// produce a `("handler", _, "Get", _)` tuple for a TS decorated
/// method, NOT a `("", 0, ...)` sentinel. Pins parity between live
/// `callers_in_source` and the indexed `extract_call_edges` path.
#[test]
fn typescript_decorator_via_extract_call_edges() {
    let src = "class C {\n    @Get(\"/items\")\n    handler() {}\n}\n";
    let edges = extract_call_edges(src, Language::TypeScript);
    eprintln!("TS edges: {edges:?}");
    assert!(
        edges
            .iter()
            .any(|(caller, _, callee, _)| caller == "handler" && callee == "Get"),
        "expected ('handler', _, 'Get', _) in edges: {edges:?}"
    );
    assert!(
        !edges
            .iter()
            .any(|(caller, line, callee, _)| caller.is_empty() && *line == 0 && callee == "Get"),
        "must NOT emit a sentinel edge for `Get`: {edges:?}"
    );
}

/// `class C { @Get("/x") handler() {} }` — method-level decorator
/// produces forward edge `handler → Get`.
#[test]
fn typescript_decorator_with_call_target() {
    let src = r#"
class C {
    @Get("/x")
    handler() {}
}
"#;
    let matches = callees(src, Language::TypeScript, "handler");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"Get"),
        "expected `Get` in callees(handler): {names:?}"
    );

    let caller_matches = callers(src, Language::TypeScript, "Get");
    let caller_names: Vec<&str> = caller_matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        caller_names.contains(&"handler"),
        "expected `handler` in callers(Get): {caller_names:?}"
    );
}

/// `@nest.Get("/x")` — qualified decorator path; rightmost
/// identifier wins, intermediate `nest` must NOT appear.
#[test]
fn typescript_qualified_decorator_rightmost() {
    let src = r#"
class C {
    @nest.Get("/x")
    handler() {}
}
"#;
    let matches = callees(src, Language::TypeScript, "handler");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"Get"),
        "rightmost `Get` must appear: {names:?}"
    );
    assert!(
        !names.contains(&"nest"),
        "intermediate `nest` must NOT leak: {names:?}"
    );
}

/// Bare-identifier decorator `@bound method()` — no `()` invocation,
/// edge resolves to the identifier itself.
#[test]
fn typescript_bare_identifier_decorator() {
    let src = r#"
class C {
    @bound
    method() {}
}
"#;
    let matches = callees(src, Language::TypeScript, "method");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"bound"),
        "bare-identifier decorator must produce edge `method → bound`: {names:?}"
    );
}

/// Multi-decorator stack `@d1() @d2() m()` — exactly two edges,
/// outermost-first by line. Pins the source-order emission via
/// tree-sitter cursor walk.
#[test]
fn typescript_multi_decorator_stack_outermost_first() {
    let src = r#"
class C {
    @d1()
    @d2()
    m() {}
}
"#;
    let mut matches = callees(src, Language::TypeScript, "m");
    matches.sort_by_key(|m| m.line);
    let pairs: Vec<(&str, usize)> = matches
        .iter()
        .filter(|m| m.name == "d1" || m.name == "d2")
        .map(|m| (m.name.as_str(), m.line))
        .collect();
    assert_eq!(
        pairs.len(),
        2,
        "expected exactly 2 decorator edges: {matches:?}"
    );
    assert_eq!(pairs[0].0, "d1", "outermost decorator must come first");
    assert_eq!(pairs[1].0, "d2");
    assert!(
        pairs[0].1 < pairs[1].1,
        "outer line must be < inner line: {pairs:?}"
    );
}

/// Class-level decorator `@Controller() class Foo {}` — Phase 14.6
/// (v1.12.0) now emits a module-scope sentinel edge for the
/// decorator's target (`Controller` here). The class itself is still
/// NOT a FnDef, so the decorator must NOT leak into the method's
/// callees (`handler` does not call `Controller`), and the edge that
/// DOES exist for `Controller` must have an empty `caller`
/// (Phase 14.1 sentinel → `<module:path>` at resolve time), not the
/// inner method. Pre-14.6 this test asserted "zero edges"; the
/// updated contract is "exactly one edge, and it attributes to
/// module scope".
#[test]
fn typescript_class_level_decorator_attributes_to_module_scope() {
    let src = r#"
@Controller()
class Foo {
    handler() {}
}
"#;
    // Invariant #1: `handler` callees must not include `Controller` —
    // method-level callees must stay disjoint from class-level
    // decorators.
    let matches = callees(src, Language::TypeScript, "handler");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        !names.contains(&"Controller"),
        "class-level `Controller` must not leak to method `handler`: {names:?}"
    );

    // Invariant #2: the edge for `Controller` is attributed to module
    // scope (caller="") via the Phase 14.1 sentinel. Pinning the
    // exact shape catches a regression that would attribute to any
    // method or fall to a different fallback.
    let edges = extract_call_edges(src, Language::TypeScript);
    let controller_edges: Vec<_> = edges
        .iter()
        .filter(|(_, _, callee, _)| callee == "Controller")
        .collect();
    assert!(
        !controller_edges.is_empty(),
        "Phase 14.6 should emit at least one `Controller` edge: {edges:?}"
    );
    assert!(
            controller_edges
                .iter()
                .all(|(caller, line, _, _)| caller.is_empty() && *line == 0),
            "every `Controller` edge must attribute to module scope (caller=\"\", line=0): {controller_edges:?}"
        );
}

/// Property decorator `@inject() svc: Svc` — properties are NOT
/// indexed as FnDefs, so the decorator has no anchor. Negative
/// pin: the SCM must not synthesise a sentinel edge.
#[test]
fn typescript_property_decorator_not_indexed() {
    let src = r#"
class C {
    @inject()
    svc: Svc;

    handler() {}
}
"#;
    // `handler` is a real method but its callees should NOT
    // include `inject` (which belongs to the property, not
    // to handler).
    let matches = callees(src, Language::TypeScript, "handler");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        !names.contains(&"inject"),
        "property decorator must not leak to adjacent method: {names:?}"
    );
    // `inject` should also have no callers — there's no method
    // to attribute the property's decorator to.
    let inject_callers = callers(src, Language::TypeScript, "inject");
    let inject_caller_names: Vec<&str> = inject_callers.iter().map(|m| m.name.as_str()).collect();
    assert!(
        !inject_caller_names.contains(&"handler"),
        "property decorator must not attribute to neighbouring method: {inject_caller_names:?}"
    );
}

/// `edge.line` for a TS decorator edge must point at the decorator
/// line, NOT the method line. Same invariant as Phase 14.2 Python.
#[test]
fn typescript_decorator_edge_line_is_decorator_not_method() {
    // Decorator on line 3, method on line 4 (1-indexed,
    // accounting for the leading empty line in the r#"..."# string).
    let src = "\nclass C {\n    @MyDecorator()\n    handler() {}\n}\n";
    let matches = callees(src, Language::TypeScript, "handler");
    let dec_edge = matches
        .iter()
        .find(|m| m.name == "MyDecorator")
        .unwrap_or_else(|| panic!("expected `MyDecorator` edge from `handler`, got: {matches:?}"));
    assert_eq!(
        dec_edge.line, 3,
        "edge.line must be decorator row (3), not method row (4); got: {dec_edge:?}"
    );
}

// ---- Phase 14.2.1 — Rust attribute edges (RED) -----------------------

/// `#[tokio::test] fn it_works()` — scoped attribute path, rightmost
/// identifier (`test`) wins.
#[test]
fn rust_scoped_attribute_rightmost() {
    let src = r#"
#[tokio::test]
fn it_works() {}
"#;
    let matches = callees(src, Language::Rust, "it_works");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"test"),
        "rightmost `test` of #[tokio::test] must appear: {names:?}"
    );
    assert!(
        !names.contains(&"tokio"),
        "intermediate `tokio` must NOT leak: {names:?}"
    );
}

/// `#[wasm_bindgen] fn foo()` — bare-identifier attribute path.
#[test]
fn rust_bare_identifier_attribute() {
    let src = r#"
#[wasm_bindgen]
fn foo() {}
"#;
    let matches = callees(src, Language::Rust, "foo");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"wasm_bindgen"),
        "bare-id attribute must produce edge `foo → wasm_bindgen`: {names:?}"
    );
}

/// `#[serde(rename = "x")] fn bar()` — args are in `token_tree`,
/// the identifier `rename` inside the token_tree MUST NOT be
/// captured as a callee. Path head `serde` is the only callee.
#[test]
fn rust_attribute_args_not_captured_as_callees() {
    let src = r#"
#[serde(rename = "x")]
fn bar() {}
"#;
    let matches = callees(src, Language::Rust, "bar");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"serde"),
        "path-head `serde` must appear: {names:?}"
    );
    assert!(
        !names.contains(&"rename"),
        "arg identifier `rename` (inside token_tree) must NOT be captured: {names:?}"
    );
}

/// `#[derive(Debug, Clone)] fn buggy()` — derive filter drops the
/// whole attribute; neither `derive` nor the derived trait names
/// `Debug`/`Clone` may appear as callees.
#[test]
fn rust_derive_filtered_out() {
    let src = r#"
#[derive(Debug, Clone)]
fn buggy() {}
"#;
    let matches = callees(src, Language::Rust, "buggy");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        !names.contains(&"derive"),
        "derive filter must skip the attribute entirely: {names:?}"
    );
    assert!(
        !names.contains(&"Debug") && !names.contains(&"Clone"),
        "derived trait names must NOT appear as callees: {names:?}"
    );
}

/// Multi-attribute stack `#[a] #[b] fn f()` — exactly 2 edges,
/// outermost-first by line.
#[test]
fn rust_multi_attribute_outermost_first() {
    let src = r#"
#[a]
#[b]
fn f() {}
"#;
    let mut matches = callees(src, Language::Rust, "f");
    matches.sort_by_key(|m| m.line);
    let pairs: Vec<(&str, usize)> = matches
        .iter()
        .filter(|m| m.name == "a" || m.name == "b")
        .map(|m| (m.name.as_str(), m.line))
        .collect();
    assert_eq!(
        pairs.len(),
        2,
        "expected exactly 2 attribute edges: {matches:?}"
    );
    assert_eq!(pairs[0].0, "a", "outermost attribute must come first");
    assert_eq!(pairs[1].0, "b");
    assert!(
        pairs[0].1 < pairs[1].1,
        "outer line must be < inner line: {pairs:?}"
    );
}

/// Attribute on method inside `impl` block — the `attribute_item`
/// sits under `declaration_list`, NOT directly under `impl_item`.
/// SCM patterns must root on BOTH `source_file` (for top-level fns)
/// AND `declaration_list` (for `impl` methods).
#[test]
fn rust_attribute_inside_impl_method() {
    let src = r#"
struct Foo;

impl Foo {
    #[tokio::test]
    fn bar() {}
}
"#;
    let matches = callees(src, Language::Rust, "bar");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"test"),
        "attribute on `impl` method must produce edge `bar → test`: {names:?}"
    );
}

/// Phase 14.1 ↔ 14.2.1 coexistence: a top-level attributed fn must
/// produce the decorator edge (`fn → attr_target`) and MUST NOT
/// produce a Phase 14.1 sentinel edge (`<empty> → attr_target`) for
/// the synthetic call. The byte_offset remap in `extract_callgraph`
/// ensures the call falls inside the fn body range, so `min_by_key`
/// attribution picks the fn — not the module sentinel. Uses
/// `fn run` (not `fn main`) so the assertion focuses on the
/// sentinel-absence invariant and isn't entangled with the
/// fn-name-equals-attr-rightmost self-edge artifact documented in
/// `docs/LIMITATIONS.md` (rightmost-id collision section).
#[test]
fn rust_attribute_does_not_leak_to_module_sentinel() {
    let src = r#"
#[tokio::main]
fn run() {}
"#;
    let edges = extract_call_edges(src, Language::Rust);
    // Decorator edge: run → main (rightmost of #[tokio::main]).
    assert!(
        edges
            .iter()
            .any(|(caller, _, callee, _)| caller == "run" && callee == "main"),
        "expected attribute edge `run → main`, got: {edges:?}"
    );
    // Sentinel guard: NO edge with empty caller for any callee
    // produced by the attribute path (`tokio` or `main`).
    for (caller, line, callee, _) in &edges {
        assert!(
            !(caller.is_empty() && *line == 0 && (callee == "main" || callee == "tokio")),
            "attribute call must not leak to <module:> sentinel: {edges:?}"
        );
    }
}

/// `edge.line` for a Rust attribute edge must point at the attribute
/// line, NOT the `fn` line.
#[test]
fn rust_attribute_edge_line_is_attribute_not_fn() {
    // Attribute on line 2, fn on line 3 (1-indexed, accounting
    // for the leading "\n" in the source literal).
    let src = "\n#[MyAttr]\nfn target() {}\n";
    let matches = callees(src, Language::Rust, "target");
    let edge = matches
        .iter()
        .find(|m| m.name == "MyAttr")
        .unwrap_or_else(|| panic!("expected `MyAttr` edge from `target`, got: {matches:?}"));
    assert_eq!(
        edge.line, 2,
        "edge.line must be attribute row (2), not fn row (3); got: {edge:?}"
    );
}
