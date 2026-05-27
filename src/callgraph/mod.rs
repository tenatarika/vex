use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use rayon::prelude::*;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::parse::language::Language;

pub mod bfs;
pub mod indegree;

/// Per-step cap when binding `find_callers_fast` as the BFS
/// `callers_of` closure. Far above any realistic fan-in but bounded
/// for safety; saturation should surface a stderr warning so an
/// incomplete walk is visible. Shared across `vex paths`,
/// `vex reachable`, and `vex bundle --mode pr-impact`.
pub const CALLERS_FETCH_CAP: usize = 1024;

/// A caller→callee relationship found in source code.
#[derive(Debug, Clone)]
pub struct CallMatch {
    /// Function that contains the call (caller) or is being called (callee)
    pub name: String,
    pub path: String,
    pub line: usize,
}

/// Find all functions that call `target_name`.
pub fn find_callers(
    root: &Path,
    target_name: &str,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<CallMatch>> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files: Vec<_> = crate::util::walk::discover_source_files(&root, excludes)?
        .into_iter()
        .filter(|(_, lang)| callgraph_query(*lang).is_some())
        .collect();

    let matches: Vec<CallMatch> = files
        .par_iter()
        .flat_map(|(path, lang)| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            callers_in_source(&content, *lang, &rel, target_name)
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

/// Find all functions called by `target_name`.
pub fn find_callees(
    root: &Path,
    target_name: &str,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<CallMatch>> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files: Vec<_> = crate::util::walk::discover_source_files(&root, excludes)?
        .into_iter()
        .filter(|(_, lang)| callgraph_query(*lang).is_some())
        .collect();

    let matches: Vec<CallMatch> = files
        .par_iter()
        .flat_map(|(path, lang)| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            callees_in_source(&content, *lang, &rel, target_name)
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

struct FnDef {
    name: String,
    line: usize,
    start_byte: usize,
    end_byte: usize,
}

struct Call {
    callee: String,
    line: usize,
    byte_offset: usize,
}

/// Find callers of `target` in a single source file.
fn callers_in_source(content: &str, lang: Language, path: &str, target: &str) -> Vec<CallMatch> {
    let (fns, calls) = match extract_callgraph(content, lang) {
        Some(r) => r,
        None => return Vec::new(),
    };

    // Find all calls to target
    let target_calls: Vec<&Call> = calls.iter().filter(|c| c.callee == target).collect();

    if target_calls.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for call in target_calls {
        // Find the innermost containing function
        if let Some(f) = fns
            .iter()
            .filter(|f| call.byte_offset >= f.start_byte && call.byte_offset < f.end_byte)
            .min_by_key(|f| f.end_byte - f.start_byte)
        {
            if seen.insert((f.name.as_str(), f.line)) {
                results.push(CallMatch {
                    name: f.name.clone(),
                    path: path.to_string(),
                    line: f.line,
                });
            }
        }
    }

    results
}

/// Find callees of `target` in a single source file.
fn callees_in_source(content: &str, lang: Language, path: &str, target: &str) -> Vec<CallMatch> {
    let (fns, calls) = match extract_callgraph(content, lang) {
        Some(r) => r,
        None => return Vec::new(),
    };

    // Find the target function definition. Phase 14.2 introduced a
    // "Double FnDef" invariant for Python: each decorated function
    // produces both an inner `function_definition` (small range, body
    // only) AND an outer `decorated_definition` (larger range, covers
    // both decorators and body). For `callees`, we want the LARGEST
    // range — that way decorator targets (`@app.get` → `get`) AND body
    // calls both attribute to the function. Picking the smallest range
    // would drop decorator edges because the decorator's byte_offset
    // sits outside the inner function_definition.
    //
    // Note: this is the OPPOSITE of `callers_in_source` / `extract_call_edges`,
    // which use `min_by_key` because there we ask "which fn contains
    // this call site?" — innermost enclosing scope wins. Here we ask
    // "which range covers all my callees?" — outermost FnDef for the
    // same name wins.
    let target_fn = match fns
        .iter()
        .filter(|f| f.name == target)
        .max_by_key(|f| f.end_byte - f.start_byte)
    {
        Some(f) => f,
        None => return Vec::new(),
    };

    // Find all calls within the target function's body
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for call in &calls {
        if call.byte_offset >= target_fn.start_byte
            && call.byte_offset < target_fn.end_byte
            && call.callee != target
            && seen.insert(call.callee.as_str())
        {
            results.push(CallMatch {
                name: call.callee.clone(),
                path: path.to_string(),
                line: call.line,
            });
        }
    }

    results
}

/// Extract resolved `(caller_fn_name, caller_fn_line, callee_name, call_line)`
/// edges from a single source file.
///
/// Each call is attributed to its innermost enclosing function definition.
/// The caller's **definition line** is returned alongside its name so that a
/// downstream resolver can disambiguate two functions with the same name in
/// the same file (overloaded methods, duplicate `impl` blocks, etc.).
///
/// Returns an empty vec when the language has no call-graph query, when
/// parsing fails, or when there are no calls inside a function.
///
/// Used by `index::pipeline` to build the persistent call-graph sections at
/// index time. Live-scan paths in this module still use the internal
/// [`extract_callgraph`].
pub fn extract_call_edges(content: &str, lang: Language) -> Vec<(String, usize, String, usize)> {
    let Some((fns, calls)) = extract_callgraph(content, lang) else {
        return Vec::new();
    };
    let mut edges = Vec::with_capacity(calls.len());
    for call in &calls {
        // Find the innermost containing function (smallest byte range).
        if let Some(f) = fns
            .iter()
            .filter(|f| call.byte_offset >= f.start_byte && call.byte_offset < f.end_byte)
            .min_by_key(|f| f.end_byte - f.start_byte)
        {
            edges.push((f.name.clone(), f.line, call.callee.clone(), call.line));
        } else {
            // Phase 14.1: module-scope call site (no enclosing fn). Emit a
            // sentinel edge — `pipeline::resolve_call_edges` rewrites it to
            // the synthetic `<module:path>` symbol injected by `parse_file`.
            edges.push((String::new(), 0, call.callee.clone(), call.line));
        }
    }
    edges
}

/// Per-language compiled callgraph `Query`. Phase 14.2 grew the Python
/// query from 3 patterns to 7 and Java from 1 to 5, which made the
/// per-file `Query::new` cost (called inside `par_iter` from
/// `find_callers` / `find_callees`) a real hot-path concern on
/// Python-heavy repos. The map is built lazily on first access; each
/// entry is compiled once and reused across every subsequent file.
/// `Query` is `Send + Sync` (read-only after compile).
static COMPILED_QUERIES: LazyLock<HashMap<Language, Query>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for lang in [
        Language::Rust,
        Language::Python,
        Language::Java,
        Language::TypeScript,
        Language::Go,
        Language::Cpp,
        Language::Kotlin,
        Language::CSharp,
    ] {
        if let Some(src) = callgraph_query(lang) {
            match Query::new(&lang.ts_language(), src) {
                Ok(q) => {
                    m.insert(lang, q);
                }
                Err(e) => {
                    tracing::error!(
                        lang = lang.as_str(),
                        error = %e,
                        "failed to compile callgraph query at startup; \
                         per-file extraction will return empty for this language"
                    );
                }
            }
        }
    }
    m
});

/// Extract function definitions and call expressions from source.
fn extract_callgraph(content: &str, lang: Language) -> Option<(Vec<FnDef>, Vec<Call>)> {
    let query = COMPILED_QUERIES.get(&lang)?;
    let ts_lang = lang.ts_language();

    let mut parser = Parser::new();
    parser.set_language(&ts_lang).ok()?;
    let tree = parser.parse(content, None)?;

    let fn_name_idx = query.capture_index_for_name("fn.name")?;
    let fn_body_idx = query.capture_index_for_name("fn.decl")?;
    let call_name_idx = query.capture_index_for_name("call.name")?;

    let mut cursor = QueryCursor::new();
    let mut query_matches = cursor.matches(query, tree.root_node(), content.as_bytes());

    let mut fns = Vec::new();
    let mut calls = Vec::new();
    // Multi-annotation / multi-decorator functions re-emit `@fn.decl` +
    // `@fn.name` for the same function once per matching pattern (Kotlin
    // `@Inject @Named("svc") fun foo()` → 3 matches with identical
    // ranges; Python `@a @b @c def f()` → 3 matches with identical
    // outer `decorated_definition` ranges). Same-range duplicates would
    // make `max_by_key` / `min_by_key` in `callers_in_source` /
    // `callees_in_source` iteration-order-dependent if a future grammar
    // tweak ever makes the ranges drift apart; collapse them up front.
    // The Python "Double FnDef" invariant (different ranges for the
    // inner `function_definition` and the outer `decorated_definition`)
    // is preserved — those entries have distinct `(start, end)` pairs.
    let mut seen_fns: HashSet<(usize, usize, String)> = HashSet::new();

    while let Some(m) = query_matches.next() {
        let mut fn_name = None;
        let mut fn_body_range = None;
        let mut fn_line = 0;
        let mut call_name = None;
        let mut call_line = 0;
        let mut call_offset = 0;

        for capture in m.captures {
            let text = &content[capture.node.byte_range()];
            if capture.index == fn_name_idx {
                fn_name = Some(text);
                fn_line = capture.node.start_position().row + 1;
            } else if capture.index == fn_body_idx {
                fn_body_range = Some((capture.node.start_byte(), capture.node.end_byte()));
            } else if capture.index == call_name_idx {
                call_name = Some(text);
                call_line = capture.node.start_position().row + 1;
                call_offset = capture.node.start_byte();
            }
        }

        if let (Some(name), Some((start, end))) = (fn_name, fn_body_range) {
            if seen_fns.insert((start, end, name.to_string())) {
                fns.push(FnDef {
                    name: name.to_string(),
                    line: fn_line,
                    start_byte: start,
                    end_byte: end,
                });
            }
        }

        if let Some(callee) = call_name {
            calls.push(Call {
                callee: callee.to_string(),
                line: call_line,
                byte_offset: call_offset,
            });
        }
    }

    Some((fns, calls))
}

fn callgraph_query(lang: Language) -> Option<&'static str> {
    match lang {
        Language::Rust => Some(
            r#"
            (function_item name: (identifier) @fn.name) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (scoped_identifier
                name: (identifier) @call.name))

            (call_expression
              function: (field_expression
                field: (field_identifier) @call.name))
            "#,
        ),
        Language::Python => Some(
            r#"
            (function_definition name: (identifier) @fn.name) @fn.decl

            (call function: (identifier) @call.name)

            (call function: (attribute
              attribute: (identifier) @call.name))

            ; Phase 14.2 — decorator edges. `@fn.decl` captures the OUTER
            ; `decorated_definition` so the decorator call site (which
            ; lives outside the inner function_definition byte range)
            ; attributes to the wrapped function via `min_by_key`. The
            ; existing `function_definition` pattern also fires for the
            ; inner span — the smaller range wins for in-body calls.
            ; Callee = rightmost identifier (consistent with method calls).

            ; @app.get("/x") — call with attribute target
            (decorated_definition
              (decorator
                (call function: (attribute
                  attribute: (identifier) @call.name)))
              definition: (function_definition
                name: (identifier) @fn.name)) @fn.decl

            ; @login_required() — call with bare-identifier target
            (decorated_definition
              (decorator
                (call function: (identifier) @call.name))
              definition: (function_definition
                name: (identifier) @fn.name)) @fn.decl

            ; @login_required — bare identifier, no parens
            (decorated_definition
              (decorator (identifier) @call.name)
              definition: (function_definition
                name: (identifier) @fn.name)) @fn.decl

            ; @app.router — bare attribute, no parens
            (decorated_definition
              (decorator
                (attribute attribute: (identifier) @call.name))
              definition: (function_definition
                name: (identifier) @fn.name)) @fn.decl
            "#,
        ),
        Language::Java => Some(
            r#"
            (method_declaration name: (identifier) @fn.name) @fn.decl
            (constructor_declaration name: (identifier) @fn.name) @fn.decl

            (method_invocation name: (identifier) @call.name)

            ; Phase 14.2 — annotation edges. `@fn.decl` is the
            ; method_declaration itself: the `modifiers` child (which
            ; carries the annotations) is already INSIDE the method's
            ; byte range, so the inner-fn attribution works without a
            ; wider capture. Callee = rightmost identifier of the
            ; annotation name (consistent with method-call convention).

            ; @Override / @Deprecated — marker_annotation, bare identifier
            (method_declaration
              (modifiers (marker_annotation name: (identifier) @call.name))
              name: (identifier) @fn.name) @fn.decl

            ; @org.junit.Test — marker_annotation with scoped name (rightmost)
            (method_declaration
              (modifiers (marker_annotation name: (scoped_identifier
                name: (identifier) @call.name)))
              name: (identifier) @fn.name) @fn.decl

            ; @GetMapping("/x") — annotation with arguments, bare name
            (method_declaration
              (modifiers (annotation name: (identifier) @call.name))
              name: (identifier) @fn.name) @fn.decl

            ; @org.springframework.web.bind.annotation.GetMapping(...) —
            ; annotation with arguments + scoped name (rightmost)
            (method_declaration
              (modifiers (annotation name: (scoped_identifier
                name: (identifier) @call.name)))
              name: (identifier) @fn.name) @fn.decl
            "#,
        ),
        Language::TypeScript => Some(
            r#"
            (function_declaration name: (identifier) @fn.name) @fn.decl
            (method_definition name: (property_identifier) @fn.name) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (member_expression
                property: (property_identifier) @call.name))
            "#,
        ),
        Language::Go => Some(
            r#"
            (function_declaration name: (identifier) @fn.name) @fn.decl
            (method_declaration name: (field_identifier) @fn.name) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (selector_expression
                field: (field_identifier) @call.name))
            "#,
        ),
        Language::Cpp => Some(
            r#"
            (function_definition
              declarator: (function_declarator
                declarator: (identifier) @fn.name)) @fn.decl

            (function_definition
              declarator: (function_declarator
                declarator: (qualified_identifier
                  name: (identifier) @fn.name))) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (qualified_identifier
                name: (identifier) @call.name))

            (call_expression
              function: (field_expression
                field: (field_identifier) @call.name))
            "#,
        ),
        Language::Kotlin => Some(
            r#"
            ; Function declaration (Phase 14.2.2).
            ; NOTE: `init { ... }` blocks, `getter`/`setter` accessors, and
            ; lambda invocations are intentionally NOT indexed as FnDef.
            ; Calls from those sites fall to the Phase 14.1 synthetic
            ; `<module:path>` caller — documented in docs/LIMITATIONS.md.
            (function_declaration name: (identifier) @fn.name) @fn.decl

            ; Bare call: foo()
            (call_expression (identifier) @call.name)

            ; Member access call: obj.method() — trailing identifier wins.
            ; navigation_expression has two `identifier` children separated
            ; by a literal `.` token; the SECOND identifier is the callee.
            (call_expression
              (navigation_expression
                (identifier)
                (identifier) @call.name))

            ; Annotation edges (Phase 14.2.2).
            ; @JvmStatic — bare type. tree-sitter-kotlin-ng uses
            ; `identifier` (not `type_identifier`) inside `user_type`.
            ; For qualified annotations like @kotlin.jvm.JvmStatic the
            ; user_type contains multiple identifiers separated by `.`
            ; tokens; the trailing `.` anchor matches only the LAST
            ; named child (rightmost wins).
            (function_declaration
              (modifiers (annotation
                (user_type (identifier) @call.name .)))
              name: (identifier) @fn.name) @fn.decl

            ; @Named("svc") — constructor_invocation (annotation with args).
            ; `constructor_invocation` has no fields; the first child is a
            ; `user_type` (concrete subtype of the `type` supertype).
            (function_declaration
              (modifiers (annotation
                (constructor_invocation
                  (user_type (identifier) @call.name .))))
              name: (identifier) @fn.name) @fn.decl
            "#,
        ),
        Language::CSharp => Some(
            r#"
            ; Method + constructor declarations (Phase 14.2.2).
            ; NOTE: property accessors (`get =>`, `set { ... }`), local
            ; functions, indexer / event accessors, and lambda invocations
            ; are intentionally NOT indexed as FnDef. Calls from those
            ; sites fall to the Phase 14.1 synthetic `<module:path>`
            ; caller — documented in docs/LIMITATIONS.md.
            (method_declaration name: (identifier) @fn.name) @fn.decl
            (constructor_declaration name: (identifier) @fn.name) @fn.decl

            ; Bare invocation: Foo()
            (invocation_expression function: (identifier) @call.name)

            ; Member access invocation: obj.Method()
            ; `member_access_expression` has a `name:` field that gives
            ; the trailing identifier — same convention as Java/TS.
            (invocation_expression
              function: (member_access_expression
                name: (identifier) @call.name))

            ; Attribute edges (Phase 14.2.2).
            ; [HttpGet("/x")] / [Authorize] — bare attribute name.
            (method_declaration
              (attribute_list (attribute name: (identifier) @call.name))
              name: (identifier) @fn.name) @fn.decl

            (constructor_declaration
              (attribute_list (attribute name: (identifier) @call.name))
              name: (identifier) @fn.name) @fn.decl

            ; [System.Web.Mvc.HttpGet] — qualified attribute, rightmost
            ; identifier wins. In tree-sitter-c-sharp `qualified_name`
            ; recurses: outer `name:` field walks toward the trailing
            ; identifier leaf.
            (method_declaration
              (attribute_list (attribute name: (qualified_name
                name: (identifier) @call.name)))
              name: (identifier) @fn.name) @fn.decl

            (constructor_declaration
              (attribute_list (attribute name: (qualified_name
                name: (identifier) @call.name)))
              name: (identifier) @fn.name) @fn.decl
            "#,
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callers(src: &str, lang: Language, target: &str) -> Vec<CallMatch> {
        callers_in_source(src, lang, "test", target)
    }

    fn callees(src: &str, lang: Language, target: &str) -> Vec<CallMatch> {
        callees_in_source(src, lang, "test", target)
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
}
