//! Callgraph extraction engine — tree-sitter walking, query compilation,
//! and edge-resolution helpers shared by every callgraph code path.
//!
//! `extract_call_edges` is the public seam used by `index::pipeline` at
//! index time to populate the persistent call-graph sections.
//! `callers_in_source` / `callees_in_source` are the live-scan helpers
//! invoked by the public `find_callers` / `find_callees` query API in
//! `super`. The compiled-`Query` map (`COMPILED_QUERIES`) is initialised
//! lazily at first call and reused across every subsequent file —
//! `tree_sitter::Query` is `Send + Sync` so a `LazyLock<HashMap>` is the
//! right shape for cross-thread reuse via `rayon`.
//!
//! Isolated from the query SCM (`super::queries`) so adding a language
//! is a queries-only change once the walker covers the necessary node
//! kinds; isolated from the public query API in `super` so external
//! callers cross a single module boundary.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryCursor};

use crate::parse::language::Language;

use super::queries::callgraph_query;
use super::CallMatch;

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
pub(super) fn callers_in_source(
    content: &str,
    lang: Language,
    path: &str,
    target: &str,
) -> Vec<CallMatch> {
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
    let mut seen = HashSet::new();

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
pub(super) fn callees_in_source(
    content: &str,
    lang: Language,
    path: &str,
    target: &str,
) -> Vec<CallMatch> {
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
    let mut seen = HashSet::new();

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
/// `extract_callgraph` (private — no intra-doc link).
///
/// ```
/// use vex::parse::language::Language;
/// use vex::callgraph::extract_call_edges;
///
/// let src = "fn caller() { callee(); }\nfn callee() {}\n";
/// let edges = extract_call_edges(src, Language::Rust);
/// assert!(edges.iter().any(|(caller, _line, callee, _)| {
///     caller == "caller" && callee == "callee"
/// }));
///
/// // Language without a callgraph query returns empty.
/// assert!(extract_call_edges("# heading", Language::Markdown).is_empty());
/// ```
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
    // Iterate every supported language and probe `callgraph_query`; the
    // returned `Some(_)` set is the canonical "this grammar contributes
    // to the persistent call graph" registry. Adding a new language is
    // now a queries-only change — no risk of forgetting to register it
    // here. (S10, v1.12.0 — closes the S3 review-finding.)
    for &lang in Language::ALL {
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

/// Phase 14.6 capture name for class-level decorator / annotation
/// targets that should attribute to module scope. Bypasses the
/// `call_capture_inside_sibling_host` dedup filter — see the comment at
/// the use-site in `extract_callgraph` (same file). A future contributor
/// adding or renaming this string must update BOTH the SCM patterns in
/// `super::queries::callgraph_query` AND the dispatch arm here; the
/// const is the single source of truth (grep across the `callgraph`
/// module to find every occurrence).
const MODULE_CALL_CAPTURE: &str = "module_call.name";

/// Extract function definitions and call expressions from source.
fn extract_callgraph(content: &str, lang: Language) -> Option<(Vec<FnDef>, Vec<Call>)> {
    let query = COMPILED_QUERIES.get(&lang)?;

    // v1.12.0 P3 — pooled per-thread parser; v1.23.0 — guarded by the
    // shared `parse_text` budget.
    let tree = crate::parse::parser_pool::parse_text(lang, content).ok()?;

    let fn_name_idx = query.capture_index_for_name("fn.name")?;
    let fn_body_idx = query.capture_index_for_name("fn.decl")?;
    let call_name_idx = query.capture_index_for_name("call.name")?;
    // Phase 14.2.1 — sibling-adjacency captures (TypeScript decorators,
    // Rust attribute_items). May or may not be present in this language's
    // query — `None` cleanly disables the sibling-pair branch.
    let sibling_target_idx = query.capture_index_for_name("sibling.target");
    let sibling_host_idx = query.capture_index_for_name("sibling.host");
    // Phase 14.6 — class-level decorator / annotation targets that should
    // attribute to module scope (no enclosing FnDef → Phase 14.1 sentinel
    // rewrites to `<module:path>`). Distinct capture name so the
    // TypeScript `call_capture_inside_sibling_host` filter does NOT
    // suppress them — that filter was written to dedupe generic
    // `call_expression` captures that fire inside decorator arguments,
    // but Phase 14.6 deliberately captures the decorator's *direct*
    // target identifier. The capture string is centralised in the
    // [`MODULE_CALL_CAPTURE`] const so a `grep` finds both the SCM
    // patterns and the dispatch arm in one shot.
    let module_call_name_idx = query.capture_index_for_name(MODULE_CALL_CAPTURE);

    // Phase 14.2.1 perf gate. The per-`@call.name` ancestor walk inside
    // `call_capture_inside_sibling_host` only does work when this file
    // actually contains a decorator/attribute host. Most files don't —
    // a one-shot `content.contains()` byte-scan over the source short-
    // circuits the walks for the common case. The marker is the SOURCE
    // syntax (`@` in TS/JS, `#[` in Rust), not an AST kind, so the
    // check stays cheap (no extra tree walk, no extra parse).
    let has_sibling_host = match lang {
        Language::TypeScript => content.contains('@'),
        Language::Rust => content.contains("#["),
        _ => false,
    };

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
        let mut sibling_target: Option<&str> = None;
        let mut sibling_host_node: Option<tree_sitter::Node> = None;

        for capture in m.captures {
            let text = &content[capture.node.byte_range()];
            if capture.index == fn_name_idx {
                fn_name = Some(text);
                fn_line = capture.node.start_position().row + 1;
            } else if capture.index == fn_body_idx {
                fn_body_range = Some((capture.node.start_byte(), capture.node.end_byte()));
            } else if capture.index == call_name_idx {
                // Phase 14.2.1 — skip standard `call_expression` /
                // `(member_expression property: ...)` captures whose
                // ancestor is a `decorator` (TypeScript) or
                // `attribute_item` / `attribute` (Rust). Those identifiers
                // are already covered by the sibling-adjacency pattern
                // with `byte_offset` remapped onto the next fn's start,
                // so emitting them here too would land a duplicate Call
                // whose byte_offset sits OUTSIDE any fn body — the
                // Phase 14.1 sentinel path would then attribute it to
                // `<module:path>`, producing a phantom caller alongside
                // the correct decorator edge. Filter at capture-time.
                // `has_sibling_host` short-circuits the walk on files
                // that contain no decorators/attributes at all.
                if has_sibling_host && call_capture_inside_sibling_host(capture.node, lang) {
                    continue;
                }
                call_name = Some(text);
                call_line = capture.node.start_position().row + 1;
                call_offset = capture.node.start_byte();
            } else if Some(capture.index) == sibling_target_idx {
                sibling_target = Some(text);
            } else if Some(capture.index) == sibling_host_idx {
                sibling_host_node = Some(capture.node);
            } else if Some(capture.index) == module_call_name_idx {
                // Phase 14.6 — class-level decorator target. Records
                // a call at the capture's byte position with no enclosing
                // FnDef; `extract_call_edges` falls to the Phase 14.1
                // sentinel (`<module:path>` caller). Bypasses the
                // sibling-host filter intentionally — see capture's
                // declaration above.
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

        // Phase 14.2.1 — sibling-adjacency emission. When a match has
        // both `@sibling.host` (the decorator / attribute_item node) and
        // `@sibling.target` (the rightmost identifier of the path),
        // walk the host's next named siblings to find the next function-
        // shaped node, then synthesise a `Call` with `byte_offset =
        // next_fn.start_byte` so the standard FnDef attribution picks it
        // up. The Rust-derive filter runs here before emit.
        if let (Some(target), Some(host)) = (sibling_target, sibling_host_node) {
            if !rust_derive_filter_skip(lang, host, content) {
                if let Some(next_fn) = next_function_sibling(lang, host) {
                    calls.push(Call {
                        callee: target.to_string(),
                        line: host.start_position().row + 1,
                        byte_offset: next_fn.start_byte(),
                    });
                }
            }
        }
    }

    Some((fns, calls))
}

/// Walk `host`'s next named siblings under the same parent and return
/// the first node that is a function-shaped declaration for `lang`.
/// Used by Phase 14.2.1 sibling-adjacency edges to pair a decorator /
/// `attribute_item` with the function it decorates. Intermediate
/// decorators or `attribute_item`s are tolerated (multi-stack support).
fn next_function_sibling(lang: Language, host: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let parent = host.parent()?;
    let target_kinds: &[&str] = match lang {
        Language::TypeScript => &["method_definition"],
        Language::Rust => &["function_item"],
        _ => return None,
    };
    let mut cursor = parent.walk();
    let mut found_host = false;
    for child in parent.named_children(&mut cursor) {
        if !found_host {
            if child == host {
                found_host = true;
            }
            continue;
        }
        if target_kinds.contains(&child.kind()) {
            return Some(child);
        }
    }
    None
}

/// Returns true if `node` (a captured `@call.name` identifier) has an
/// ancestor that is the host of a sibling-adjacency decorator/attribute
/// pattern. Used to suppress spurious duplicate edges that the standard
/// `call_expression`-based callgraph patterns would otherwise emit for
/// identifiers living INSIDE a decorator's call_expression (TypeScript)
/// or — defensively, for future grammars — inside Rust `attribute_item`
/// nodes (Rust attribute paths are not call_expressions today, so this
/// is a no-op for Rust).
///
/// **Depth bound — why 8 is enough.** The capture patterns that feed
/// this filter only fire on `function:` slots of `call_expression`
/// (TS) and on `function:`/scoped name slots of `call_expression` /
/// `field_expression` (Rust). A captured identifier is always
/// `parent_call_expression → identifier` (depth 2 from the call's
/// own parent). Each additional level of decorator-argument nesting
/// (`@d(a(b(c())))`) adds two AST levels (`call_expression` +
/// `arguments`). At 8 hops we accommodate ~3 levels of nested calls
/// inside a decorator argument list, which exceeds anything seen in
/// real codebases. The bound holds because we never capture
/// identifiers in argument position — only function-slot identifiers,
/// which are by construction shallow under their enclosing call.
///
/// **PERF.** Called once per `@call.name` capture during indexing.
/// On a TypeScript-heavy monorepo this is the dominant cost of the
/// 14.2.1 work (~+6% wall time on vex self-repo). A future
/// optimisation could memoise "this subtree contains no decorator
/// ancestor" once per outer scope (class_body / source_file /
/// declaration_list) — tracked informally as a follow-up; not on
/// the 14.x train.
fn call_capture_inside_sibling_host(node: tree_sitter::Node, lang: Language) -> bool {
    let host_kinds: &[&str] = match lang {
        Language::TypeScript => &["decorator"],
        Language::Rust => &["attribute_item", "attribute"],
        _ => return false,
    };
    let mut current = node.parent();
    for _ in 0..8 {
        let Some(p) = current else { return false };
        if host_kinds.contains(&p.kind()) {
            return true;
        }
        current = p.parent();
    }
    false
}

/// Rust-specific: drop `#[derive(...)]` attributes entirely. The filter
/// runs at attribute-extraction time, BEFORE the rightmost-id projection.
/// It looks at the FIRST identifier descendant of the `attribute` child
/// of the `attribute_item` host — i.e. the path HEAD — and skips when
/// that text is exactly `"derive"`. Arguments inside the `token_tree`
/// child are never inspected, so an attribute like
/// `#[some_attr(derive = "x")]` still emits an edge to `some_attr`.
fn rust_derive_filter_skip(lang: Language, host: tree_sitter::Node, content: &str) -> bool {
    if lang != Language::Rust {
        return false;
    }
    if host.kind() != "attribute_item" {
        return false;
    }
    let Some(attribute) = host.named_child(0) else {
        return false;
    };
    // First named child of `attribute` is either an `identifier` (bare
    // path: `#[derive(...)]`) or a `scoped_identifier` (qualified path:
    // `#[serde::Serialize]`). For `derive`, only the bare form is real —
    // there's no `std::derive`. Anything else short-circuits to "keep".
    let Some(first) = attribute.named_child(0) else {
        return false;
    };
    if first.kind() != "identifier" {
        return false;
    }
    &content[first.byte_range()] == "derive"
}
