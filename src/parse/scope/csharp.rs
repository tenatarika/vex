//! C# scope binder (11.1.5 + `using` cross-file follow-up). Shares
//! scaffolding with the other binders via [`super::walker::Walker`].
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
//! - `using_directive` — three forms emit a `DefKind::Import` binding
//!   the writer's Pass-2 resolves cross-file:
//!   * `using A.B.C;` → bind `C` to `UsePath{[A,B,C]}`.
//!   * `using static A.B.C;` → bind `C` to `UsePath{[A,B,C]}`.
//!   * `using Alias = A.B.C;` → bind `Alias` to `UsePath{[A,B,C]}`.
//!
//!   `global using` is treated the same as plain `using` (the modifier
//!   only changes scope semantics, not the bound name). The directive's
//!   path identifiers are NOT walked as refs — same convention as
//!   Rust `use_declaration` / Python `import_statement`.
//!
//! ## What's deferred
//!
//! - Wildcard namespace resolution: in C# `using A.B;` semantically
//!   imports every member of namespace `B` unqualified, so a bare
//!   `Foo` ref might mean `A.B.Foo`. This binder only handles the
//!   *name* binding side — `Foo` itself stays `Unresolved` unless
//!   explicitly imported via `using static` or aliased. Pass-2's
//!   `name_to_global` is name-keyed; multi-segment wildcard
//!   resolution would need a separate side-channel.
//! - Destructuring patterns; tuple deconstruction emits spurious refs
//!   to the names rather than binding them.
//! - Generic type parameters.

use anyhow::Result;
use tree_sitter::Node;

use super::walker::{parse_with, Walker};
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind, UsePath};
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
        "using_directive" => walk_using(w, node, scope),
        // `extern alias MyAlias;` is a declaration of an external
        // assembly alias, not a reference to one. Suppress its
        // children so the alias name doesn't leak in as a phantom
        // `Unresolved` ref at the directive line.
        "extern_alias_directive" => {}
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

/// Walk a `using_directive` node. Children are not emitted as refs —
/// the directive is a binding site for the imported tail name (or the
/// alias when the `name:` field is present).
///
/// The path is collected by walking the AST recursively (see
/// [`collect_cs_path`]) rather than text-splitting the path node:
/// text splitting trips on `global::App.Lib.Gateway`, where `::` is
/// embedded in the path text and would corrupt the first segment.
fn walk_using(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    let mut alias: Option<Node> = None;
    let mut path: Option<Node> = None;
    let mut cursor = node.walk();
    for (i, child) in node.children(&mut cursor).enumerate() {
        match node.field_name_for_child(i as u32) {
            // `using Alias = A.B.C;` — `name:` is the alias.
            Some("name") if child.kind() == "identifier" => alias = Some(child),
            _ => match child.kind() {
                "qualified_name" | "alias_qualified_name" | "identifier" => {
                    path = Some(child);
                }
                _ => {}
            },
        }
    }
    let Some(path_node) = path else { return };
    let mut segments = Vec::new();
    collect_cs_path(path_node, w.content, &mut segments);
    if segments.is_empty() {
        return;
    }
    let bind_name = match alias {
        Some(a) => a
            .utf8_text(w.content.as_bytes())
            .unwrap_or("")
            .trim()
            .to_string(),
        None => segments
            .last()
            .cloned()
            .expect("segments non-empty checked above"),
    };
    if bind_name.is_empty() {
        return;
    }
    w.add_import_binding(scope, bind_name, line, UsePath { segments });
}

/// Recursively flatten a C# path node into dotted segments. Handles:
///   * `qualified_name` (`qualifier:` + `name:`)
///   * `alias_qualified_name` (`alias:` is dropped; only the `name:`
///     side is a real path segment — `global::App` becomes `[App]`).
///   * `identifier` leaf.
fn collect_cs_path(node: Node, content: &str, out: &mut Vec<String>) {
    match node.kind() {
        "qualified_name" => {
            if let Some(q) = node.child_by_field_name("qualifier") {
                collect_cs_path(q, content, out);
            }
            if let Some(n) = node.child_by_field_name("name") {
                collect_cs_path(n, content, out);
            }
        }
        "alias_qualified_name" => {
            // The `alias:` field (`global`, `MyExternAlias`, …) is a
            // namespace qualifier, not a path root. Drop it so the
            // tail-lookup-driven Pass-2 sees only real segments.
            if let Some(n) = node.child_by_field_name("name") {
                collect_cs_path(n, content, out);
            }
        }
        "identifier" => {
            let text = node
                .utf8_text(content.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string();
            if !text.is_empty() {
                out.push(text);
            }
        }
        _ => {}
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
