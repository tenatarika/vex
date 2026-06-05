//! TypeScript scope binder (11.1.4a + 11.1.6 imports). Shares
//! scaffolding with the other binders via [`super::walker::Walker`].
//!
//! ## What's handled
//!
//! - `function_declaration`, `method_definition` — name in parent
//!   scope; params + return type + body walked in a child fn scope.
//! - `arrow_function`, `function_expression` — anonymous fn scope.
//! - `variable_declarator` (under `lexical_declaration` /
//!   `variable_declaration`) — bind pattern names; value walked first.
//! - `class_declaration`, `interface_declaration`,
//!   `type_alias_declaration`, `enum_declaration` — name in parent.
//! - `statement_block` — opens a child block scope.
//! - `import_statement` — named (`{ Foo, Bar as Baz }`), default,
//!   namespace, and `import type` clauses all stamp `DefKind::Import`
//!   bindings carrying `UsePath { segments: [source, original_name] }`
//!   so Pass-2 can resolve them cross-file.
//!
//! ## What's deferred
//!
//! - JSX `<Foo />` and declaration merging (interface + class same
//!   name) — 11.1.4c.
//! - Destructuring patterns.
//! - Generic type parameters.

use anyhow::Result;
use tree_sitter::Node;

use super::walker::{parse_with, Walker};
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind, UsePath};
use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;

pub struct TypeScriptBinder;

impl ScopeBinder for TypeScriptBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let tree = parse_with(Language::TypeScript, content)?;
        Ok(Walker::new(content, file_symbols, dispatch).run(&tree))
    }
}

fn dispatch(w: &mut Walker, node: Node, scope: ScopeId) {
    let kind = node.kind();

    if is_ts_comment_kind(kind) || is_ts_plain_string_kind(kind) {
        return;
    }
    if kind == "template_string" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "template_substitution" {
                w.walk(child, scope);
            }
        }
        return;
    }

    match kind {
        "function_declaration" | "method_definition" => walk_named_fn(w, node, scope),
        "arrow_function" | "function_expression" => walk_anonymous_fn(w, node, scope),
        "variable_declarator" => walk_var_declarator(w, node, scope),
        "class_declaration" | "interface_declaration" => walk_class_like(w, node, scope),
        "type_alias_declaration" | "enum_declaration" => bind_named_decl(w, node, scope),
        "import_statement" => walk_import_statement(w, node, scope),
        "statement_block" => {
            let s = w.push_scope(ScopeKind::Block, scope);
            w.walk_children(node, s);
        }
        "identifier" => w.emit_ref(node, scope, RefKind::Value),
        "type_identifier" => w.emit_ref(node, scope, RefKind::Type),
        // v1.14.1 — member access (`obj.foo` / `obj?.foo` / `obj.foo()`).
        // The property side is `property_identifier`; without this branch
        // `do_charge` in `gw.do_charge()` never produced a ref, so the
        // v1.14.1 Pass-2 single-candidate fallback couldn't resolve it
        // cross-file. We dispatch on `member_expression` (and the
        // optional-chaining variant) instead of matching `property_identifier`
        // globally — that would also fire on `{ key: value }` object-literal
        // pair keys (which use `property_identifier` but are bindings, not
        // usages) and produce phantom refs from definition sites.
        "member_expression" | "subscript_expression" => walk_member_expression(w, node, scope),
        _ => w.walk_children(node, scope),
    }
}

/// Emit refs for both sides of a member access. Tree-sitter-typescript
/// uses `member_expression` with field `object` (lhs) and `property`
/// (rhs, a `property_identifier`). Recursion onto the object handles
/// the regular value-ref path; we explicitly emit the property when
/// it's a `property_identifier` so the v1.14.1 Pass-2 single-candidate
/// fallback can resolve `do_charge` in `gw.do_charge()`.
///
/// Also handles `subscript_expression` (`obj[expr]`) for the object side
/// only — the bracketed `expr` is already a value position and recurses
/// via `walk_children` style.
fn walk_member_expression(w: &mut Walker, node: Node, scope: ScopeId) {
    // Cache the field-name lookups: each `child_by_field_name` call does
    // a linear scan over `node.children`. Calling it twice (once to
    // process, once to fetch ID for the dedup filter below) walks the
    // child list twice — code-reviewer flagged this both as cost and as
    // a logic trap if a future grammar version returns a different
    // node instance on the second call.
    let object = node.child_by_field_name("object");
    let property = node.child_by_field_name("property");
    if let Some(o) = object {
        w.walk(o, scope);
    }
    if let Some(p) = property {
        if p.kind() == "property_identifier" {
            w.emit_ref(p, scope, RefKind::Value);
        } else {
            // `subscript_expression`'s "property" is an arbitrary
            // expression node — recurse so its sub-identifiers still
            // get emitted as refs.
            w.walk(p, scope);
        }
    }
    // Tree-sitter-typescript also exposes positional children on some
    // `subscript_expression` grammar versions. Walk every remaining
    // child that isn't object/property already handled so we don't
    // drop identifiers in the bracketed expression.
    let object_id = object.map(|n| n.id());
    let prop_id = property.map(|n| n.id());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) == object_id || Some(child.id()) == prop_id {
            continue;
        }
        // Skip punctuation/syntax tokens by their kind shape; recurse
        // into actual expression children.
        if matches!(child.kind(), "." | "[" | "]" | "?." | "(" | ")") {
            continue;
        }
        w.walk(child, scope);
    }
}

fn walk_named_fn(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(parent, name_node, DefKind::Function);
    }
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    walk_fn_params(w, node, fn_scope);
    if let Some(ret) = node.child_by_field_name("return_type") {
        w.walk(ret, fn_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, fn_scope);
    }
}

fn walk_anonymous_fn(w: &mut Walker, node: Node, parent: ScopeId) {
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    walk_fn_params(w, node, fn_scope);
    if let Some(ret) = node.child_by_field_name("return_type") {
        w.walk(ret, fn_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, fn_scope);
    }
}

fn walk_fn_params(w: &mut Walker, node: Node, fn_scope: ScopeId) {
    let Some(params) = node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        match child.kind() {
            "required_parameter" | "optional_parameter" => {
                if let Some(pat) = child.child_by_field_name("pattern") {
                    bind_pattern(w, pat, fn_scope, DefKind::Param);
                }
                if let Some(ty) = child.child_by_field_name("type") {
                    w.walk(ty, fn_scope);
                }
                if let Some(value) = child.child_by_field_name("value") {
                    w.walk(value, fn_scope);
                }
            }
            _ => {}
        }
    }
}

fn walk_var_declarator(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, scope);
    }
    if let Some(value) = node.child_by_field_name("value") {
        w.walk(value, scope);
    }
    if let Some(name) = node.child_by_field_name("name") {
        bind_pattern(w, name, scope, DefKind::Variable);
    }
}

fn walk_class_like(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(parent, name_node, DefKind::Type);
    }
    let class_scope = w.push_scope(ScopeKind::Class, parent);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_heritage" | "extends_clause" | "implements_clause" => {
                w.walk(child, parent);
            }
            "class_body" | "interface_body" | "object_type" => {
                w.walk_children(child, class_scope);
            }
            _ => {}
        }
    }
}

fn bind_named_decl(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(scope, name_node, DefKind::Type);
    }
}

fn bind_pattern(w: &mut Walker, pat: Node, scope: ScopeId, kind: DefKind) {
    if pat.kind() == "identifier" {
        w.add_binding(scope, pat, kind);
    }
    // Destructuring patterns are deferred — names inside an object
    // or array pattern silently stay unbound until 11.1.4c.
}

/// Process an `import_statement`. Children are NOT emitted as refs
/// (the import path is a binding site). Bindings get tagged
/// `DefKind::Import` with the dotted module path so Pass-2
/// cross-file resolution can rewrite them into `to_sym_idx`.
fn walk_import_statement(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    let Some(source) = node
        .child_by_field_name("source")
        .and_then(|s| extract_string_text(s, w.content))
    else {
        return;
    };
    if source.is_empty() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "import_clause" {
            walk_import_clause(w, child, scope, line, &source);
        }
    }
}

fn walk_import_clause(w: &mut Walker, clause: Node, scope: ScopeId, line: usize, source: &str) {
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                let name = child.utf8_text(w.content.as_bytes()).unwrap_or("");
                if !name.is_empty() {
                    w.add_import_binding(
                        scope,
                        name,
                        line,
                        UsePath {
                            segments: vec![source.to_string()],
                        },
                    );
                }
            }
            "namespace_import" => {
                let mut inner = child.walk();
                for grand in child.children(&mut inner) {
                    if grand.kind() == "identifier" {
                        let alias = grand.utf8_text(w.content.as_bytes()).unwrap_or("");
                        if !alias.is_empty() {
                            w.add_import_binding(
                                scope,
                                alias,
                                line,
                                UsePath {
                                    segments: vec![source.to_string()],
                                },
                            );
                        }
                        break;
                    }
                }
            }
            "named_imports" => {
                let mut inner = child.walk();
                for grand in child.children(&mut inner) {
                    if grand.kind() == "import_specifier" {
                        let original = grand
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(w.content.as_bytes()).ok())
                            .unwrap_or("");
                        let local = grand
                            .child_by_field_name("alias")
                            .and_then(|n| n.utf8_text(w.content.as_bytes()).ok())
                            .unwrap_or(original);
                        if !original.is_empty() && !local.is_empty() {
                            w.add_import_binding(
                                scope,
                                local,
                                line,
                                UsePath {
                                    segments: vec![source.to_string(), original.to_string()],
                                },
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_ts_comment_kind(kind: &str) -> bool {
    kind == "comment"
}

fn is_ts_plain_string_kind(kind: &str) -> bool {
    matches!(kind, "string" | "regex")
}

/// Return the textual content of a TypeScript `string` node — the
/// `string_fragment` child without the surrounding quote tokens.
fn extract_string_text(node: Node, content: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string_fragment" {
            return child.utf8_text(content.as_bytes()).ok().map(String::from);
        }
    }
    None
}
