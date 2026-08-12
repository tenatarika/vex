//! Callgraph extraction engine — tree-sitter walking, query compilation,
//! and edge-resolution helpers shared by every callgraph code path.
//!
//! `extract_call_edges_with_tree` is the seam used by `parse::parse_file` at
//! index time to populate the persistent call-graph sections, off the single
//! tree that function parses. `extract_call_edges` is the same logic with its own
//! parse, kept as the public API for callers without a tree.
//! `callers_in_source` / `callees_in_source` are the live-scan helpers
//! invoked by the public `find_callers` / `find_callees` query API in
//! `super`. The compiled-`Query` cache (`CG_QUERY_CELLS`) compiles each
//! language's query lazily on the first file of that language and reuses it
//! across every subsequent file — `tree_sitter::Query` is `Send + Sync` so a
//! map of `OnceLock` cells is the right shape for cross-thread reuse via
//! `rayon`.
//!
//! Isolated from the query SCM (`super::queries`) so adding a language
//! is a queries-only change once the walker covers the necessary node
//! kinds; isolated from the public query API in `super` so external
//! callers cross a single module boundary.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, OnceLock};

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
/// Parses `content` itself. Index-time extraction instead goes through
/// [`extract_call_edges_with_tree`], which reuses `parse_file`'s tree; live-scan
/// paths in this module use the internal `extract_callgraph` (private — no
/// intra-doc link).
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
// Bin-target artifact: see the `#[allow]` note on the re-export in `mod.rs`.
#[allow(dead_code)]
pub fn extract_call_edges(content: &str, lang: Language) -> Vec<(String, usize, String, usize)> {
    let Some((fns, calls)) = extract_callgraph(content, lang) else {
        return Vec::new();
    };
    edges_from(fns, calls)
}

/// [`extract_call_edges`] over a tree the caller already parsed.
///
/// The `compiled_query` short-circuit lives in
/// [`extract_callgraph_with_tree`], so languages without a callgraph query
/// still get an empty vec off a tree they are handed — same contract as the
/// refs and skeleton cores.
pub(crate) fn extract_call_edges_with_tree(
    tree: &tree_sitter::Tree,
    content: &str,
    lang: Language,
) -> Vec<(String, usize, String, usize)> {
    let Some((fns, calls)) = extract_callgraph_with_tree(tree, content, lang) else {
        return Vec::new();
    };
    edges_from(fns, calls)
}

/// Attribute each call to its innermost enclosing function definition.
fn edges_from(fns: Vec<FnDef>, calls: Vec<Call>) -> Vec<(String, usize, String, usize)> {
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

/// Per-language compiled callgraph `Query`, compiled **lazily per language on
/// the first file of that language**. Phase 14.2 grew the Python query from 3
/// patterns to 7 and Java from 1 to 5, which made the per-file `Query::new`
/// cost (called inside `par_iter` from `find_callers` / `find_callees`) a real
/// hot-path concern on Python-heavy repos — hence caching.
///
/// The cache is a map of **per-language `OnceLock` cells**, not an eagerly
/// populated map. The previous `LazyLock<HashMap<Language, Query>>` compiled
/// the callgraph query for ALL ~19 languages on first access, so a `vex update`
/// touching a single Rust file paid ~69 ms compiling 18 irrelevant grammars'
/// queries on every process (measured — see `docs/STORAGE-RESEARCH.md`
/// §"parse_files init attribution"). Now each language's query compiles only
/// when a file of that language is first parsed; a single-language repo pays
/// for one grammar.
///
/// The outer map still enumerates `Language::ALL` and probes `callgraph_query`
/// (cheap — it returns a `&'static str`, no compilation), so its keyset stays
/// the canonical "this grammar contributes to the persistent call graph"
/// registry: adding a language remains a queries-only change (S10, v1.12.0 —
/// closes the S3 review-finding). Each `OnceLock<Option<Query>>` holds
/// `Some(query)` on success or `None` on compile failure (logged once, never
/// retried). `Query` / `OnceLock` are `Send + Sync`, so `get_or_init` is safe
/// under the `rayon` parse fan-out.
static CG_QUERY_CELLS: LazyLock<HashMap<Language, OnceLock<Option<Query>>>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for &lang in Language::ALL {
        if callgraph_query(lang).is_some() {
            m.insert(lang, OnceLock::new());
        }
    }
    m
});

/// Lazily-compiled callgraph query for `lang`, or `None` when the language has
/// no callgraph query registered or its compilation failed. Compiles on the
/// first call per language, then returns the cached result for every
/// subsequent file (shared read-only across `rayon` workers).
fn compiled_query(lang: Language) -> Option<&'static Query> {
    CG_QUERY_CELLS
        .get(&lang)?
        .get_or_init(|| {
            let src = callgraph_query(lang)?;
            match Query::new(&lang.ts_language(), src) {
                Ok(q) => Some(q),
                Err(e) => {
                    tracing::error!(
                        lang = lang.as_str(),
                        error = %e,
                        "failed to compile callgraph query; \
                         per-file extraction will return empty for this language"
                    );
                    None
                }
            }
        })
        .as_ref()
}

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
///
/// Self-parsing entry point, used by the live-scan paths in this module
/// (`callers_in_source` / `callees_in_source`). Index-time extraction goes
/// through [`extract_call_edges_with_tree`] instead.
fn extract_callgraph(content: &str, lang: Language) -> Option<(Vec<FnDef>, Vec<Call>)> {
    // The query probe stays BEFORE the parse: a language with no callgraph
    // query must not pay a parse here. `find_callers` / `find_callees` already
    // pre-filter on `callgraph_query(lang).is_some()`, so this is unobservable
    // today — but reordering it would make the wrapper diverge from the shape
    // it replaced, for no gain.
    compiled_query(lang)?;

    // v1.12.0 P3 — pooled per-thread parser; v1.23.0 — guarded by the
    // shared `parse_text` budget.
    let tree = crate::parse::parser_pool::parse_text(lang, content).ok()?;
    extract_callgraph_with_tree(&tree, content, lang)
}

/// [`extract_callgraph`] over a tree the caller already parsed.
fn extract_callgraph_with_tree(
    tree: &tree_sitter::Tree,
    content: &str,
    lang: Language,
) -> Option<(Vec<FnDef>, Vec<Call>)> {
    let query = compiled_query(lang)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Registry safety: every language whose `callgraph_query` returns a
    /// pattern MUST actually compile against its grammar. The old eager
    /// `COMPILED_QUERIES` surfaced a broken `.scm` (or a grammar ABI drift)
    /// as a `tracing::error!` at first-access; the lazy per-language cache
    /// would surface it only when a file of that language is first parsed.
    /// This test pulls that failure back to compile-of-the-test-suite time,
    /// preserving the "adding a language is a queries-only change" contract
    /// without the per-process all-langs compilation cost.
    #[test]
    fn every_registered_callgraph_query_compiles() {
        for &lang in Language::ALL {
            if callgraph_query(lang).is_some() {
                assert!(
                    compiled_query(lang).is_some(),
                    "callgraph query for {lang:?} is registered but failed to compile"
                );
            }
        }
    }

    /// A language with no registered callgraph query resolves to `None`
    /// (not a panic / not an empty-but-present entry) — the same
    /// `extract_callgraph` short-circuit the old `HashMap::get` gave.
    #[test]
    fn unregistered_language_has_no_compiled_query() {
        for &lang in Language::ALL {
            if callgraph_query(lang).is_none() {
                assert!(
                    compiled_query(lang).is_none(),
                    "{lang:?} has no callgraph query yet compiled_query returned Some"
                );
            }
        }
    }

    /// The whole point of the `OnceLock` cell is that a language's query is
    /// compiled ONCE and reused — repeated calls must hand back the *same*
    /// `&'static Query`, not recompile per call. Pin pointer identity so a
    /// future "simplification" that drops the cache (recompiling on every
    /// `compiled_query`) is caught here instead of silently regressing the
    /// `vex update` latency win this cache exists for.
    #[test]
    fn compiled_query_is_cached_same_pointer() {
        // Rust is always registered (has a callgraph query); use it as the probe.
        let a = compiled_query(Language::Rust).expect("rust callgraph query compiles");
        let b = compiled_query(Language::Rust).expect("rust callgraph query compiles");
        assert!(
            std::ptr::eq(a, b),
            "compiled_query returned distinct pointers → query is being recompiled, \
             not cached"
        );
    }
}
