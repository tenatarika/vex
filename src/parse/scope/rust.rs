//! Rust scope binder (11.1.2b/c). Shares scaffolding with the four
//! other per-language binders via [`super::walker::Walker`].
//!
//! ## What's handled
//!
//! - `function_item` — name bound in parent scope; params bound in fn
//!   scope; return type + body walked as ref-bearing subtrees.
//! - `let_declaration` — type + value + `let-else` alternative walked
//!   first (refs see the pre-binding scope), then the pattern is bound.
//! - `struct_item` / `enum_item` / `trait_item` / `type_item` /
//!   `const_item` / `static_item` — name bound in parent scope.
//! - `mod_item` — name bound in parent scope, body walked in a child
//!   module scope.
//! - `impl_item` — body walked in a child impl scope; type / trait
//!   refs live in the parent scope.
//! - `use_declaration` — bindings tagged `DefKind::Import` so the
//!   writer's Pass-2 cross-file resolution can rewrite them.
//! - `block` — opens a child block scope.
//!
//! ## What's deferred
//!
//! - Destructuring patterns (tuple, struct, tuple-struct, slice,
//!   or-pat). `bind_pattern` only handles plain `identifier` and the
//!   `mut_pattern` / `ref_pattern` / `reference_pattern` wrappers.
//! - `closure_expression`, `for_expression`, `while_let_expression`,
//!   `if_let_expression`, `match_arm` pattern bindings.
//! - Generic parameters and lifetimes.
//! - Macros — refs inside expand-positions stay flat.

use anyhow::Result;
use tree_sitter::Node;

use super::walker::{parse_with, Walker};
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind, UsePath};
use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;

pub struct RustBinder;

impl ScopeBinder for RustBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let tree = parse_with(Language::Rust, content)?;
        Ok(Walker::new(content, file_symbols, dispatch).run(&tree))
    }
}

fn dispatch(w: &mut Walker, node: Node, scope: ScopeId) {
    let kind = node.kind();

    // 11.1.1 noise filter — comments and string literals never produce
    // binder-visible refs.
    if is_rust_comment_kind(kind) || is_rust_string_kind(kind) {
        return;
    }

    match kind {
        "function_item" => walk_fn(w, node, scope),
        "let_declaration" => walk_let(w, node, scope),
        "struct_item" | "enum_item" | "trait_item" | "type_item" => {
            bind_named_decl(w, node, scope, DefKind::Type)
        }
        "const_item" | "static_item" => walk_const_like(w, node, scope),
        "mod_item" => walk_mod(w, node, scope),
        "impl_item" => walk_impl(w, node, scope),
        "use_declaration" => walk_use(w, node, scope),
        "block" => {
            let s = w.push_scope(ScopeKind::Block, scope);
            w.walk_children(node, s);
        }
        "identifier" => w.emit_ref(node, scope, RefKind::Value),
        "type_identifier" => w.emit_ref(node, scope, RefKind::Type),
        _ => w.walk_children(node, scope),
    }
}

fn walk_fn(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(parent, name_node, DefKind::Function);
    }
    let fn_scope = w.push_scope(ScopeKind::Function, parent);

    if let Some(params) = node.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            if child.kind() == "parameter" {
                if let Some(pat) = child.child_by_field_name("pattern") {
                    bind_pattern(w, pat, fn_scope, DefKind::Param);
                }
                if let Some(ty) = child.child_by_field_name("type") {
                    w.walk(ty, fn_scope);
                }
            }
            // `self_parameter` is implicit; no binding needed here.
        }
    }

    if let Some(ret) = node.child_by_field_name("return_type") {
        w.walk(ret, fn_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, fn_scope);
    }
}

fn walk_let(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, scope);
    }
    if let Some(value) = node.child_by_field_name("value") {
        w.walk(value, scope);
    }
    // `let Ok(x) = foo() else { return default; }` — the `else` block
    // is a regular ref-bearing subtree.
    if let Some(alt) = node.child_by_field_name("alternative") {
        w.walk(alt, scope);
    }
    if let Some(pat) = node.child_by_field_name("pattern") {
        bind_pattern(w, pat, scope, DefKind::Variable);
    }
}

fn walk_const_like(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(scope, name_node, DefKind::Variable);
    }
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, scope);
    }
    if let Some(value) = node.child_by_field_name("value") {
        w.walk(value, scope);
    }
}

fn walk_mod(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(parent, name_node, DefKind::Module);
    }
    let mod_scope = w.push_scope(ScopeKind::Module, parent);
    if let Some(body) = node.child_by_field_name("body") {
        w.walk_children(body, mod_scope);
    }
}

fn walk_impl(w: &mut Walker, node: Node, parent: ScopeId) {
    let impl_scope = w.push_scope(ScopeKind::Impl, parent);
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, parent);
    }
    if let Some(tr) = node.child_by_field_name("trait") {
        w.walk(tr, parent);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk_children(body, impl_scope);
    }
}

fn bind_named_decl(w: &mut Walker, node: Node, scope: ScopeId, kind: DefKind) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(scope, name_node, kind);
    }
    // Don't recurse into the body for 11.1.2b — generic params, field
    // types, and trait method bodies have edge cases the simple walker
    // would mishandle.
}

fn bind_pattern(w: &mut Walker, pat: Node, scope: ScopeId, kind: DefKind) {
    match pat.kind() {
        "identifier" => w.add_binding(scope, pat, kind),
        "mut_pattern" | "ref_pattern" | "reference_pattern" => {
            let mut cursor = pat.walk();
            for child in pat.children(&mut cursor) {
                bind_pattern(w, child, scope, kind);
            }
        }
        // Destructuring patterns: deferred.
        _ => {}
    }
}

/// Process a `use_declaration` node. Children are not emitted as refs
/// (the use path is a binding site). Bindings get tagged
/// `DefKind::Import` so Pass-2 can rewrite into a `to_sym_idx`.
fn walk_use(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    let mut imports: Vec<(String, UsePath)> = Vec::new();
    if let Some(arg) = node.child_by_field_name("argument") {
        collect_use_imports(arg, &[], w.content, &mut imports);
    }
    for (name, path) in imports {
        w.add_import_binding(scope, name, line, path);
    }
}

fn is_rust_comment_kind(kind: &str) -> bool {
    matches!(kind, "line_comment" | "block_comment")
}

fn is_rust_string_kind(kind: &str) -> bool {
    // tree-sitter-rust 0.24 parses `b"..."` as `string_literal` too,
    // so the three kinds below cover every string-shaped literal.
    matches!(
        kind,
        "string_literal" | "raw_string_literal" | "char_literal"
    )
}

/// Recursively flatten the path argument of a `use_declaration` into a
/// list of `(local_name, full_use_path)` pairs.
///
/// Handled shapes:
///   `use a;`                          → `[(a, [a])]`
///   `use a::b;`                       → `[(b, [a, b])]`
///   `use a::b::C;`                    → `[(C, [a, b, C])]`
///   `use a::{b, c};`                  → `[(b, [a, b]), (c, [a, c])]`
///   `use a::{b::C, d as E};`          → `[(C, [a, b, C]), (E, [a, d])]`
///   `use a as alias;`                 → `[(alias, [a])]`
///   `use a::*;`                       → no bindings (glob deferred).
fn collect_use_imports(
    node: Node,
    prefix: &[String],
    content: &str,
    output: &mut Vec<(String, UsePath)>,
) {
    match node.kind() {
        "identifier" => {
            let name = node.utf8_text(content.as_bytes()).unwrap_or("").to_string();
            if name.is_empty() {
                return;
            }
            let mut segments = prefix.to_vec();
            segments.push(name.clone());
            output.push((name, UsePath { segments }));
        }
        "scoped_identifier" => {
            let text = node.utf8_text(content.as_bytes()).unwrap_or("");
            let mut segments = prefix.to_vec();
            segments.extend(
                text.split("::")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
            if let Some(last) = segments.last().cloned() {
                output.push((last, UsePath { segments }));
            }
        }
        "use_as_clause" => {
            let path_text = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(content.as_bytes()).ok())
                .unwrap_or("");
            let mut segments = prefix.to_vec();
            segments.extend(
                path_text
                    .split("::")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
            if let Some(alias) = node.child_by_field_name("alias") {
                let alias_name = alias
                    .utf8_text(content.as_bytes())
                    .unwrap_or("")
                    .to_string();
                if !alias_name.is_empty() {
                    output.push((alias_name, UsePath { segments }));
                }
            }
        }
        "scoped_use_list" => {
            let path_text = node
                .child_by_field_name("path")
                .and_then(|n| n.utf8_text(content.as_bytes()).ok())
                .unwrap_or("");
            let mut new_prefix = prefix.to_vec();
            new_prefix.extend(
                path_text
                    .split("::")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
            if let Some(list) = node.child_by_field_name("list") {
                let mut cursor = list.walk();
                for child in list.children(&mut cursor) {
                    collect_use_imports(child, &new_prefix, content, output);
                }
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_use_imports(child, prefix, content, output);
            }
        }
        // `use a::*;` — glob. No binding here; cross-file pass would
        // need to follow the prefix into the target module's FST.
        "use_wildcard" => {}
        _ => {}
    }
}
