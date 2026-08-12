//! Python scope binder (11.1.5 + 11.1.6 imports). Shares scaffolding
//! with the other binders via [`super::walker::Walker`].
//!
//! ## What's handled
//!
//! - `function_definition` — name in parent scope; params + body in a
//!   child fn scope.
//! - `class_definition` — name in parent scope; body in a child class
//!   scope.
//! - `lambda` — anonymous fn scope.
//! - `assignment` — RHS walked first (sees pre-binding scope), LHS
//!   identifier bound.
//! - `block` does NOT introduce a new scope (Python statement blocks
//!   share the enclosing function/module by design).
//! - `import_statement` / `import_from_statement` — binds the imported
//!   names (or aliases) tagged `DefKind::Import` so Pass-2 resolves
//!   `BindTarget::Imported` cross-file. `from x import *` adds none.
//!
//! ## What's deferred
//!
//! - `for` / `with` target bindings.
//! - Comprehension scope isolation (`[x for x in y]` in Py3 introduces
//!   an isolated scope — this binder treats it as the enclosing scope).
//! - Class-scope LEGB gotcha — Python class bodies do NOT participate
//!   in lookup for nested fns; this binder over-resolves them.
//! - `global` / `nonlocal` declarations.

use tree_sitter::{Node, Tree};

use super::walker::Walker;
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind, UsePath};
use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;
use crate::parse::NodeTextExt;

pub struct PythonBinder;

impl ScopeBinder for PythonBinder {
    fn lang(&self) -> Language {
        Language::Python
    }

    fn bind_with_tree(
        &self,
        tree: &Tree,
        content: &str,
        file_symbols: &[ParsedSymbol],
    ) -> Vec<BoundRef> {
        Walker::new(content, file_symbols, dispatch).run(tree)
    }
}

fn dispatch(w: &mut Walker, node: Node, scope: ScopeId) {
    let kind = node.kind();

    if is_py_comment_kind(kind) {
        return;
    }
    // Python `string` covers regular strings AND f-strings — descend
    // only into `interpolation` children so `{expr}` refs survive
    // while literal text doesn't.
    if kind == "string" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "interpolation" {
                w.walk(child, scope);
            }
        }
        return;
    }

    match kind {
        "function_definition" => walk_fn(w, node, scope),
        "lambda" => walk_lambda(w, node, scope),
        "class_definition" => walk_class(w, node, scope),
        "assignment" => walk_assignment(w, node, scope),
        "import_statement" => walk_import_statement(w, node, scope),
        "import_from_statement" => walk_import_from(w, node, scope),
        "identifier" => w.emit_ref(node, scope, RefKind::Value),
        _ => w.walk_children(node, scope),
    }
}

fn walk_fn(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(parent, name_node, DefKind::Function);
    }
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    if let Some(params) = node.child_by_field_name("parameters") {
        walk_param_list(w, params, fn_scope);
    }
    if let Some(ret) = node.child_by_field_name("return_type") {
        w.walk(ret, fn_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk_children(body, fn_scope);
    }
}

fn walk_lambda(w: &mut Walker, node: Node, parent: ScopeId) {
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    if let Some(params) = node.child_by_field_name("parameters") {
        walk_param_list(w, params, fn_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, fn_scope);
    }
}

fn walk_param_list(w: &mut Walker, params: Node, fn_scope: ScopeId) {
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        match child.kind() {
            "identifier" => w.add_binding(fn_scope, child, DefKind::Param),
            "default_parameter" | "typed_default_parameter" => {
                if let Some(v) = child.child_by_field_name("value") {
                    w.walk(v, fn_scope);
                }
                if let Some(t) = child.child_by_field_name("type") {
                    w.walk(t, fn_scope);
                }
                if let Some(name) = child.child_by_field_name("name") {
                    if name.kind() == "identifier" {
                        w.add_binding(fn_scope, name, DefKind::Param);
                    }
                }
            }
            "typed_parameter" => {
                if let Some(t) = child.child_by_field_name("type") {
                    w.walk(t, fn_scope);
                }
                let mut inner = child.walk();
                for grand in child.children(&mut inner) {
                    if grand.kind() == "identifier" {
                        w.add_binding(fn_scope, grand, DefKind::Param);
                        break;
                    }
                }
            }
            "list_splat_pattern" | "dictionary_splat_pattern" => {
                let mut inner = child.walk();
                for grand in child.children(&mut inner) {
                    if grand.kind() == "identifier" {
                        w.add_binding(fn_scope, grand, DefKind::Param);
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

fn walk_class(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name_node) = node.child_by_field_name("name") {
        w.add_binding(parent, name_node, DefKind::Type);
    }
    if let Some(supers) = node.child_by_field_name("superclasses") {
        w.walk(supers, parent);
    }
    let class_scope = w.push_scope(ScopeKind::Class, parent);
    if let Some(body) = node.child_by_field_name("body") {
        w.walk_children(body, class_scope);
    }
}

fn walk_assignment(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(right) = node.child_by_field_name("right") {
        w.walk(right, scope);
    }
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, scope);
    }
    if let Some(left) = node.child_by_field_name("left") {
        bind_target(w, left, scope);
    }
}

fn bind_target(w: &mut Walker, node: Node, scope: ScopeId) {
    match node.kind() {
        "identifier" => w.add_binding(scope, node, DefKind::Variable),
        "tuple_pattern" | "list_pattern" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                bind_target(w, child, scope);
            }
        }
        // `obj.attr = ...` is not a new binding — walk so refs to
        // `obj` get captured.
        _ => w.walk(node, scope),
    }
}

/// `import x`, `import x.y`, `import x as y`. Children are NOT emitted
/// as refs — the binding lives at the top-level name (or alias) and
/// the dotted path is preserved for Pass-2 cross-file resolution.
fn walk_import_statement(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        if cursor.field_name() == Some("name") {
            let n = cursor.node();
            match n.kind() {
                "dotted_name" => {
                    let segs = dotted_name_segments(n, w.content);
                    if let Some(top) = segs.first().cloned() {
                        // `import os.path` binds only `os` — dotted
                        // access (`os.path.X`) is property lookup,
                        // not a separate binding.
                        w.add_import_binding(
                            scope,
                            top.clone(),
                            line,
                            UsePath {
                                segments: vec![top],
                            },
                        );
                    }
                }
                "aliased_import" => {
                    let dotted = n.child_by_field_name("name");
                    let alias = n.child_by_field_name("alias");
                    let segs = dotted
                        .map(|d| dotted_name_segments(d, w.content))
                        .unwrap_or_default();
                    if let Some(a) = alias {
                        let alias_text = a.node_text(w.content.as_bytes()).to_string();
                        if !alias_text.is_empty() && !segs.is_empty() {
                            w.add_import_binding(
                                scope,
                                alias_text,
                                line,
                                UsePath { segments: segs },
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// `from x import y`, `from x import y as z`, `from x import *`.
/// Star imports add no binding — names they bring in cannot be
/// enumerated without parsing the target module.
fn walk_import_from(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    let module_segs = node
        .child_by_field_name("module_name")
        .map(|m| dotted_name_segments(m, w.content))
        .unwrap_or_default();
    if module_segs.is_empty() {
        return;
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        if cursor.field_name() == Some("name") {
            let n = cursor.node();
            match n.kind() {
                "dotted_name" => {
                    let local_segs = dotted_name_segments(n, w.content);
                    if let Some(local_name) = local_segs.first().cloned() {
                        let mut full = module_segs.clone();
                        full.push(local_name.clone());
                        w.add_import_binding(scope, local_name, line, UsePath { segments: full });
                    }
                }
                "aliased_import" => {
                    let dotted = n.child_by_field_name("name");
                    let alias = n.child_by_field_name("alias");
                    let local_segs = dotted
                        .map(|d| dotted_name_segments(d, w.content))
                        .unwrap_or_default();
                    if let (Some(a), Some(orig)) = (alias, local_segs.first().cloned()) {
                        let alias_text = a.node_text(w.content.as_bytes()).to_string();
                        if !alias_text.is_empty() {
                            let mut full = module_segs.clone();
                            full.push(orig);
                            w.add_import_binding(
                                scope,
                                alias_text,
                                line,
                                UsePath { segments: full },
                            );
                        }
                    }
                }
                _ => {} // wildcard_import has no `name:` field
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn is_py_comment_kind(kind: &str) -> bool {
    kind == "comment"
}

/// Flatten a `dotted_name` node (`a.b.c`) into its constituent segments.
fn dotted_name_segments(node: Node, content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            let text = child.node_text(content.as_bytes());
            if !text.is_empty() {
                out.push(text.to_string());
            }
        }
    }
    out
}
