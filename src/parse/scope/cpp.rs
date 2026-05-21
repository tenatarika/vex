//! C++ scope binder (11.1.5). Shares scaffolding with the other
//! binders via [`super::walker::Walker`]. No `#include` resolution.
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
//!
//! ## What's deferred
//!
//! - Templates and generic parameters.
//! - `using namespace` / `using ns::Name`.
//! - `operator==` and other operator overloads — `operator_name` is
//!   not an identifier kind so `extract_inner_identifier` returns None.
//! - Multiple declarators in one `declaration` (`int a, b, c;`).

use anyhow::Result;
use tree_sitter::Node;

use super::walker::{parse_with, Walker};
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind};
use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;

pub struct CppBinder;

impl ScopeBinder for CppBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let tree = parse_with(Language::Cpp, content)?;
        Ok(Walker::new(content, file_symbols, dispatch).run(&tree))
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
        "compound_statement" => {
            let s = w.push_scope(ScopeKind::Block, scope);
            w.walk_children(node, s);
        }
        "identifier" | "field_identifier" => w.emit_ref(node, scope, RefKind::Value),
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
/// identifier is reached.
fn extract_inner_identifier(node: Node) -> Option<Node> {
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
