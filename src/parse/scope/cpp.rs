//! C++ scope binder (11.1.5 + `using` cross-file follow-up). Shares
//! scaffolding with the other binders via [`super::walker::Walker`].
//!
//! ## What's handled
//!
//! - `function_definition` / `declaration` (function prototype) — the
//!   name is buried under the `declarator` chain (`function_declarator
//!   → declarator → identifier / field_identifier / qualified_-
//!   identifier`); see [`extract_inner_identifier`].
//! - `class_specifier`, `struct_specifier`, `union_specifier`,
//!   `enum_specifier` — name in parent (Type kind).
//! - `namespace_definition` — name in parent, body in a child Module.
//! - `init_declarator` (local var with initializer) — bind name from
//!   inner declarator, walk value first.
//! - `parameter_declaration` — bind declarator's identifier.
//! - `compound_statement` — child block scope.
//! - `using_declaration` — `using std::vector;` → bind `vector` to
//!   `UsePath{[std, vector]}` as `DefKind::Import`. Pass-2 resolves
//!   the tail against the global name index.
//! - `alias_declaration` — `using V = std::vector<int>;` → bind `V` as
//!   `DefKind::Import` with the RHS type's qualified path; generic
//!   arguments are ignored (the type identifier is what Pass-2 looks
//!   up cross-file).
//! - `namespace_alias_definition` — `namespace alias = ns::sub;` →
//!   bind `alias` with `UsePath{[ns, sub]}`. The tail usually names a
//!   namespace (no symbol hit), but the binding keeps `alias` out of
//!   the `Unresolved` bucket if `sub` ever has a same-named type.
//!
//! ## What's deferred
//!
//! - `#include "foo.h"` / `#include <vector>` — preprocessor wildcard
//!   import with no single name binding. Resolving symbols brought in
//!   transitively would need a side-channel (header → file_id list)
//!   plus a Pass-2 fallback for `Unresolved` refs to scan included
//!   headers. Out of scope for the in-file binder.
//! - `using namespace std;` — same wildcard story; no name binding
//!   here. The `using_directive` node is matched explicitly and its
//!   children are dropped so the bare namespace identifier (e.g.,
//!   `std`) doesn't surface as a phantom ref.
//! - Templates and generic parameters.
//! - `operator==` and other operator overloads — `operator_name` is
//!   not an identifier kind so `extract_inner_identifier` returns None.
//! - Multiple declarators in one `declaration` (`int a, b, c;`).

use anyhow::Result;
use tree_sitter::Node;

use super::walker::{parse_with, Walker};
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind, UsePath};
use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;

pub struct CppBinder;

impl ScopeBinder for CppBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let tree = parse_with(Language::Cpp, content)?;
        Ok(Walker::new(content, file_symbols, dispatch).run(&tree))
    }
}

/// v1.14 — extract quoted `#include "…"` directives from a C++ file.
/// System headers (`#include <…>`), macro-named (`#include MY_HEADER`),
/// and function-like macro-style includes are deliberately skipped:
/// they have no in-file string we can resolve against the project tree.
/// The Pass-2 ref resolver in `store::writer` consumes the returned
/// list to walk the include graph for unresolved C++ refs.
///
/// Returns an empty vec on parse failure rather than `Err` — include
/// resolution is best-effort, never the trust boundary that should
/// abort indexing.
pub fn extract_cpp_includes(content: &str) -> Vec<String> {
    let Ok(tree) = parse_with(Language::Cpp, content) else {
        return Vec::new();
    };
    let bytes = content.as_bytes();
    let mut out: Vec<String> = Vec::new();
    visit_for_includes(tree.root_node(), bytes, &mut out);
    out
}

fn visit_for_includes(node: Node, bytes: &[u8], out: &mut Vec<String>) {
    if node.kind() == "preproc_include" {
        // `path` field is one of:
        //   string_literal     — `"foo.h"` (target)
        //   system_lib_string  — `<vector>` (skip)
        //   identifier         — `MY_HEADER` (macro, skip)
        //   call_expression    — `INCLUDE(x)` (macro, skip)
        if let Some(path_node) = node.child_by_field_name("path") {
            if path_node.kind() == "string_literal" {
                let raw = &bytes[path_node.byte_range()];
                // string_literal includes the surrounding quotes; strip
                // them and reject empties / non-utf8 defensively.
                if raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
                    if let Ok(s) = std::str::from_utf8(&raw[1..raw.len() - 1]) {
                        if !s.is_empty() {
                            out.push(s.to_string());
                        }
                    }
                }
            }
        }
        // preproc_include can't nest meaningfully; no need to recurse
        // into its children for further includes.
        return;
    }
    // Recurse to find includes nested inside `#ifdef` / `#if` branches —
    // tree-sitter-cpp models conditional regions as preproc_if/ifdef
    // wrapping inner statements (including preproc_include). Each
    // recursion uses its own TreeCursor so children-iterator lifetimes
    // don't entangle with the outer call's cursor.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_for_includes(child, bytes, out);
    }
}

fn dispatch(w: &mut Walker, node: Node, scope: ScopeId) {
    let kind = node.kind();

    if is_cpp_comment_kind(kind) || is_cpp_plain_string_kind(kind) {
        return;
    }

    match kind {
        "function_definition" => walk_fn_def(w, node, scope),
        "namespace_definition" => walk_namespace(w, node, scope),
        "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier" => {
            walk_class_like(w, node, scope)
        }
        "init_declarator" => walk_init_declarator(w, node, scope),
        "parameter_declaration" => walk_parameter(w, node, scope),
        "using_declaration" => walk_using_declaration(w, node, scope),
        "alias_declaration" => walk_alias_declaration(w, node, scope),
        "namespace_alias_definition" => walk_namespace_alias(w, node, scope),
        // `using namespace std;` is a wildcard import the binder can't
        // express in the name-keyed UsePath model — skip its children
        // so the namespace name doesn't surface as a phantom ref.
        "using_directive" => {}
        "compound_statement" => {
            let s = w.push_scope(ScopeKind::Block, scope);
            w.walk_children(node, s);
        }
        "identifier" | "field_identifier" => w.emit_ref(node, scope, RefKind::Value),
        // `namespace_identifier` appears in `Gateway::Charge()` style
        // qualified expressions. The previous implementation dropped
        // these silently (walked into the leaf with no handler), which
        // meant `using app::Gateway;` followed by `Gateway::foo()`
        // produced zero refs for `Gateway`. Emit as a Value ref so the
        // import binding can resolve it.
        "namespace_identifier" => w.emit_ref(node, scope, RefKind::Value),
        "type_identifier" => w.emit_ref(node, scope, RefKind::Type),
        _ => w.walk_children(node, scope),
    }
}

fn walk_fn_def(w: &mut Walker, node: Node, parent: ScopeId) {
    let declarator = node.child_by_field_name("declarator");
    let name_node = declarator.and_then(extract_inner_identifier);
    if let Some(n) = name_node {
        w.add_binding(parent, n, DefKind::Function);
    }
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    if let Some(t) = node.child_by_field_name("type") {
        w.walk(t, fn_scope);
    }
    if let Some(fd) = declarator {
        if let Some(params) = fd.child_by_field_name("parameters") {
            w.walk(params, fn_scope);
        }
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, fn_scope);
    }
}

fn walk_namespace(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(parent, name_node, DefKind::Module);
    }
    let ns_scope = w.push_scope(ScopeKind::Module, parent);
    if let Some(body) = node.child_by_field_name("body") {
        w.walk_children(body, ns_scope);
    }
}

fn walk_class_like(w: &mut Walker, node: Node, parent: ScopeId) {
    let name_node = node.child_by_field_name("name");
    if let Some(n) = name_node {
        w.add_binding(parent, n, DefKind::Type);
    }
    let class_scope = w.push_scope(ScopeKind::Class, parent);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) == name_node.map(|n| n.id()) {
            continue;
        }
        match child.kind() {
            "base_class_clause" => w.walk(child, parent),
            _ => w.walk(child, class_scope),
        }
    }
}

fn walk_init_declarator(w: &mut Walker, node: Node, scope: ScopeId) {
    let name_node = node
        .child_by_field_name("declarator")
        .and_then(extract_inner_identifier);
    if let Some(v) = node.child_by_field_name("value") {
        w.walk(v, scope);
    }
    if let Some(n) = name_node {
        w.add_binding(scope, n, DefKind::Variable);
    }
}

fn walk_parameter(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(t) = node.child_by_field_name("type") {
        w.walk(t, scope);
    }
    if let Some(d) = node.child_by_field_name("declarator") {
        if let Some(n) = extract_inner_identifier(d) {
            w.add_binding(scope, n, DefKind::Param);
        }
    }
}

/// `using std::vector;` — extract the trailing identifier from the
/// `qualified_identifier` child and emit a `DefKind::Import` binding
/// with the full path. No refs are emitted for path segments —
/// they're a binding site, not a use site.
fn walk_using_declaration(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    // `using_declaration` carries either a bare `identifier` or a
    // `qualified_identifier`. Pick the first non-keyword child.
    let mut cursor = node.walk();
    let path_node = node.children(&mut cursor).find(|c| {
        matches!(
            c.kind(),
            "qualified_identifier" | "identifier" | "type_identifier"
        )
    });
    let Some(path_node) = path_node else { return };
    let segments = collect_path_segments(path_node, w.content);
    let Some(tail) = segments.last().cloned() else {
        return;
    };
    w.add_import_binding(scope, tail, line, UsePath { segments });
}

/// `using V = std::vector<int>;` — bind the LHS `name:` field to the
/// RHS type's qualified path. Template arguments are stripped (the
/// path Pass-2 looks up is the type identifier, not the parameterised
/// shape).
fn walk_alias_declaration(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let alias = name_node
        .utf8_text(w.content.as_bytes())
        .unwrap_or("")
        .trim()
        .to_string();
    if alias.is_empty() {
        return;
    }
    let segments = node
        .child_by_field_name("type")
        .map(|t| extract_alias_target_path(t, w.content))
        .unwrap_or_default();
    if segments.is_empty() {
        // Plain alias to a primitive (`using Byte = unsigned char;`)
        // can't resolve cross-file — bind as a local Type instead so
        // the in-file resolver still treats it as defined.
        w.add_binding(scope, name_node, DefKind::Type);
        return;
    }
    w.add_import_binding(scope, alias, line, UsePath { segments });
}

/// `namespace alias = ns::sub;` — bind the alias name with the path
/// from the `nested_namespace_specifier` RHS.
fn walk_namespace_alias(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let alias = name_node
        .utf8_text(w.content.as_bytes())
        .unwrap_or("")
        .trim()
        .to_string();
    if alias.is_empty() {
        return;
    }
    // The target sits as a positional `nested_namespace_specifier`
    // or `namespace_identifier` *after* the `=` token. Walk children
    // in order and pick the first match that isn't the `name:` field
    // (the LHS alias name itself).
    let mut cursor = node.walk();
    let target = node.children(&mut cursor).find(|c| {
        c.id() != name_node.id()
            && matches!(
                c.kind(),
                "nested_namespace_specifier" | "namespace_identifier"
            )
    });
    let segments = match target {
        Some(t) => collect_path_segments(t, w.content),
        None => return,
    };
    if segments.is_empty() {
        return;
    }
    w.add_import_binding(scope, alias, line, UsePath { segments });
}

/// Walk a `qualified_identifier` / `nested_namespace_specifier` /
/// bare identifier and collect its segments. Recurses through the
/// `scope:` / `name:` fields of `qualified_identifier`, and strips
/// template-argument lists when a segment is `template_type`.
fn collect_path_segments(node: Node, content: &str) -> Vec<String> {
    let mut segs = Vec::new();
    push_path_segments(node, content, &mut segs);
    segs
}

fn push_path_segments(node: Node, content: &str, out: &mut Vec<String>) {
    match node.kind() {
        "qualified_identifier" | "nested_namespace_specifier" => {
            let name_node = node.child_by_field_name("name");
            if let Some(scope) = node.child_by_field_name("scope") {
                push_path_segments(scope, content, out);
            } else {
                // `nested_namespace_specifier` uses positional children;
                // `qualified_identifier` for root-qualified shapes like
                // `::Foo` has no `scope:` but still carries `name:`. We
                // must skip the `name:` child here — it's emitted below
                // — otherwise `::Foo` lands as `["Foo", "Foo"]`.
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if Some(child.id()) == name_node.map(|n| n.id()) {
                        continue;
                    }
                    if matches!(
                        child.kind(),
                        "namespace_identifier" | "identifier" | "type_identifier"
                    ) {
                        push_text_segment(child, content, out);
                    }
                }
            }
            if let Some(name) = name_node {
                push_path_segments(name, content, out);
            }
        }
        "template_type" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_text_segment(name, content, out);
            }
        }
        "namespace_identifier" | "identifier" | "type_identifier" => {
            push_text_segment(node, content, out);
        }
        _ => {}
    }
}

fn push_text_segment(node: Node, content: &str, out: &mut Vec<String>) {
    let s = node
        .utf8_text(content.as_bytes())
        .unwrap_or("")
        .trim()
        .to_string();
    if !s.is_empty() {
        out.push(s);
    }
}

/// For `using V = T;` — pull the qualified path out of `T`. `T` is
/// a `type_descriptor` whose `type:` field is the actual type
/// expression (`qualified_identifier`, `template_type`,
/// `primitive_type`, etc.). Returns an empty vec for primitives /
/// shapes that don't have a qualified name — the caller falls back
/// to a local Type binding.
fn extract_alias_target_path(type_descriptor: Node, content: &str) -> Vec<String> {
    let ty = type_descriptor
        .child_by_field_name("type")
        .unwrap_or(type_descriptor);
    match ty.kind() {
        "qualified_identifier" | "template_type" | "type_identifier" | "identifier" => {
            collect_path_segments(ty, content)
        }
        // Primitives, `sized_type_specifier` (`unsigned char`), and
        // anything else can't resolve cross-file.
        _ => Vec::new(),
    }
}

fn is_cpp_comment_kind(kind: &str) -> bool {
    kind == "comment"
}

fn is_cpp_plain_string_kind(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal" | "raw_string_literal" | "char_literal"
    )
}

/// Descend a C++ declarator chain looking for the innermost identifier
/// that names the entity being declared. Handles wrappers like
/// `pointer_declarator`, `reference_declarator`, `array_declarator`,
/// `function_declarator`, and `parenthesized_declarator` by following
/// their `declarator` field. `qualified_identifier` chains (`Outer::
/// Inner::name`) recurse through `name:` field until a terminal
/// identifier is reached. `operator_name`, `destructor_name`, and
/// `abstract_function_declarator` (e.g. `typedef int (*FuncPtr)();`)
/// terminate the walk with `None` since they aren't identifier-shaped.
///
/// `pub(crate)` so `pattern::skeleton` can reuse the same walk —
/// keeping both call sites in lockstep prevents silent drift between
/// the scope binder's notion of "the name of this declaration" and
/// the pattern prefilter's notion of it.
pub(crate) fn extract_inner_identifier(node: Node) -> Option<Node> {
    let mut cur = node;
    // Bound the loop so a malformed AST cycle can't hang.
    for _ in 0..32 {
        match cur.kind() {
            "identifier" | "type_identifier" | "field_identifier" => return Some(cur),
            "qualified_identifier" => {
                // Peel one level — nested qualifiers re-enter the loop.
                cur = cur.child_by_field_name("name")?;
            }
            _ => {
                cur = cur.child_by_field_name("declarator")?;
            }
        }
    }
    None
}

#[cfg(test)]
mod include_extraction_tests {
    use super::*;

    #[test]
    fn extracts_quoted_include() {
        let out = extract_cpp_includes("#include \"foo.h\"\n");
        assert_eq!(out, vec!["foo.h"]);
    }

    #[test]
    fn extracts_path_bearing_quoted_include() {
        let out = extract_cpp_includes("#include \"util/bar.h\"\n");
        assert_eq!(out, vec!["util/bar.h"]);
    }

    #[test]
    fn skips_system_lib_includes() {
        // `<vector>` is `system_lib_string`, not `string_literal` — must
        // not appear in output. If the binder regressed to including
        // system headers, this test surfaces it before users hit
        // resolution failures looking for `std::*` symbols that the
        // project doesn't define.
        let out = extract_cpp_includes("#include <vector>\n#include <string>\n");
        assert!(out.is_empty(), "unexpected system includes: {out:?}");
    }

    #[test]
    fn skips_macro_named_includes() {
        // `#include MY_HEADER` — path is `identifier`. Skip.
        let out = extract_cpp_includes("#include MY_HEADER\n");
        assert!(out.is_empty(), "macro include leaked: {out:?}");
    }

    #[test]
    fn extracts_from_inside_ifdef() {
        // Conditional regions are real in C++ — extracting only the
        // top-level includes would silently miss platform-specific
        // headers. Walker recurses into preproc_if children.
        let src = "\
#ifdef WIN32\n\
#include \"win_only.h\"\n\
#else\n\
#include \"posix.h\"\n\
#endif\n";
        let mut out = extract_cpp_includes(src);
        out.sort();
        assert_eq!(out, vec!["posix.h", "win_only.h"]);
    }

    #[test]
    fn mixed_quoted_and_system_keeps_only_quoted() {
        let src = "\
#include \"a.h\"\n\
#include <iostream>\n\
#include \"b.h\"\n\
#include <map>\n";
        let mut out = extract_cpp_includes(src);
        out.sort();
        assert_eq!(out, vec!["a.h", "b.h"]);
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert!(extract_cpp_includes("").is_empty());
        assert!(extract_cpp_includes("int main() { return 0; }").is_empty());
    }

    #[test]
    fn deduplication_is_caller_concern() {
        // Same include twice → emitted twice. The Pass-2 resolver dedupes
        // by file_id during BFS; the parser keeps the raw shape.
        let out = extract_cpp_includes("#include \"a.h\"\n#include \"a.h\"\n");
        assert_eq!(out, vec!["a.h", "a.h"]);
    }
}
