//! C# scope binder (11.1.5). Shares scaffolding with the other
//! binders via [`super::walker::Walker`]. No `using` resolution yet.
//!
//! ## What's handled
//!
//! - `method_declaration`, `constructor_declaration`,
//!   `local_function_statement` — name in parent scope; params + body
//!   in a child fn scope.
//! - `class_declaration`, `interface_declaration`, `struct_declaration`,
//!   `enum_declaration`, `record_declaration` — name in parent;
//!   `attribute_list` siblings are NOT walked (they bloat the ref
//!   table with annotation identifiers that never resolve).
//! - `variable_declarator` — bind name; value walked first.
//! - `block` — child block scope.
//!
//! ## What's deferred
//!
//! - `using` directives (cross-file resolution similar to Rust `use`).
//! - Destructuring patterns; tuple deconstruction emits spurious refs
//!   to the names rather than binding them.
//! - Generic type parameters.

use anyhow::Result;
use tree_sitter::Node;

use super::walker::{parse_with, Walker};
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind};
use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;

pub struct CSharpBinder;

impl ScopeBinder for CSharpBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let tree = parse_with(Language::CSharp, content)?;
        Ok(Walker::new(content, file_symbols, dispatch).run(&tree))
    }
}

fn dispatch(w: &mut Walker, node: Node, scope: ScopeId) {
    let kind = node.kind();

    if is_cs_comment_kind(kind) || is_cs_plain_string_kind(kind) {
        return;
    }
    if kind == "interpolated_string_expression" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "interpolation" {
                w.walk(child, scope);
            }
        }
        return;
    }

    match kind {
        "method_declaration"
        | "constructor_declaration"
        | "destructor_declaration"
        | "local_function_statement" => walk_named_fn(w, node, scope),
        "variable_declarator" => walk_var_declarator(w, node, scope),
        "class_declaration"
        | "interface_declaration"
        | "struct_declaration"
        | "enum_declaration"
        | "record_declaration" => walk_class_like(w, node, scope),
        "delegate_declaration" => bind_named_decl(w, node, scope),
        "block" => {
            let s = w.push_scope(ScopeKind::Block, scope);
            w.walk_children(node, s);
        }
        "identifier" => w.emit_ref(node, scope, RefKind::Value),
        _ => w.walk_children(node, scope),
    }
}

fn walk_named_fn(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(parent, name_node, DefKind::Function);
    }
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    if let Some(params) = node.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            if child.kind() == "parameter" {
                if let Some(name_node) = child.child_by_field_name("name") {
                    w.add_binding(fn_scope, name_node, DefKind::Param);
                }
                if let Some(ty) = child.child_by_field_name("type") {
                    w.walk(ty, fn_scope);
                }
            }
        }
    }
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, fn_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, fn_scope);
    }
}

fn walk_var_declarator(w: &mut Walker, node: Node, scope: ScopeId) {
    // `var x = expr;` — tree-sitter-c-sharp 0.23 only fields the
    // `name`; the value sits as a positional child after `=`. Iterate
    // children and walk anything that's neither the name nor `=`.
    let name_node = node.child_by_field_name("name");
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) == name_node.map(|n| n.id()) {
            continue;
        }
        if child.kind() == "=" {
            continue;
        }
        w.walk(child, scope);
    }
    if let Some(name) = name_node {
        w.add_binding(scope, name, DefKind::Variable);
    }
}

fn walk_class_like(w: &mut Walker, node: Node, parent: ScopeId) {
    let name_node = node.child_by_field_name("name");
    if let Some(n) = name_node {
        w.add_binding(parent, n, DefKind::Type);
    }
    let class_scope = w.push_scope(ScopeKind::Class, parent);
    // Walk every child under the class scope EXCEPT the `name` field
    // (already bound — re-emitting it would produce a phantom
    // self-ref), the heritage list (whose refs live in the parent
    // scope), and attribute lists (`[Serializable]`, `[JsonProperty]`
    // etc. — annotation labels rather than ref-bearing positions).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) == name_node.map(|n| n.id()) {
            continue;
        }
        match child.kind() {
            "base_list" | "type_parameter_constraints_clause" => w.walk(child, parent),
            "attribute_list" => {}
            _ => w.walk(child, class_scope),
        }
    }
}

fn bind_named_decl(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(scope, name_node, DefKind::Type);
    }
}

fn is_cs_comment_kind(kind: &str) -> bool {
    kind == "comment"
}

fn is_cs_plain_string_kind(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal" | "verbatim_string_literal" | "raw_string_literal" | "character_literal"
    )
}
