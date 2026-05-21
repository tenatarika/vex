//! Python scope binder (11.1.5).
//!
//! Walks the tree-sitter-python AST building a [`ScopeTree`] and emits
//! a [`BoundRef`] per identifier reference. In-file resolution only —
//! `import` / `from x import y` resolution is a follow-up.
//!
//! ## What's handled
//!
//! - `function_definition` — name bound in parent scope; params + body
//!   walked in a child fn scope.
//! - `class_definition` — name bound in parent scope; body walked in a
//!   child class scope.
//! - `lambda` — anonymous fn scope.
//! - `assignment` — RHS walked first (sees pre-binding scope), LHS
//!   identifier bound.
//! - `block` — does NOT introduce a new scope; Python statement blocks
//!   share the enclosing function/module scope by language design.
//! - `decorated_definition` — descends into the inner def/class.
//!
//! ## What's deferred
//!
//! - `import` / `from x import y` — needs cross-file resolution like
//!   `use` in Rust (11.1.2c).
//! - `for` / `with` target bindings.
//! - Comprehension scope isolation (`[x for x in y]` in Py3 introduces
//!   an isolated scope — this binder treats it as the enclosing scope).
//! - Class-scope LEGB gotcha (Python class bodies do NOT participate in
//!   lookup for nested fns) — this binder treats class scope like any
//!   other, which over-resolves `class C: x = 1; def f(): return x` to
//!   `Local(class_scope)` instead of `Unresolved`. Documented limit.
//! - `global` / `nonlocal` declarations.

use anyhow::{Context, Result};

use super::{
    BindTarget, BoundRef, DefKind, LocalDef, RefKind, ScopeBinder, ScopeId, ScopeKind, ScopeTree,
};
use crate::index::symbols::ParsedSymbol;
use crate::parse::extractor::is_meaningful_identifier;
use crate::parse::language::Language;

pub struct PythonBinder;

impl ScopeBinder for PythonBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&Language::Python.ts_language())
            .context("set language for python binder")?;
        let tree = parser
            .parse(content, None)
            .context("tree-sitter parse failed in python binder")?;

        let mut by_name: std::collections::HashMap<&str, u32> =
            std::collections::HashMap::with_capacity(file_symbols.len());
        for (i, s) in file_symbols.iter().enumerate() {
            by_name.entry(s.name.as_str()).or_insert(i as u32);
        }

        let mut walker = Walker {
            content,
            file_symbols_by_name: by_name,
            tree: ScopeTree::new(),
            refs: Vec::new(),
        };
        let root = walker.tree.root();
        walker.walk(tree.root_node(), root);
        Ok(walker.refs)
    }
}

struct Walker<'a> {
    content: &'a str,
    file_symbols_by_name: std::collections::HashMap<&'a str, u32>,
    tree: ScopeTree,
    refs: Vec<BoundRef>,
}

impl<'a> Walker<'a> {
    fn walk(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        let kind = node.kind();

        if is_py_comment_kind(kind) {
            return;
        }
        // Python `string` covers regular strings AND f-strings — the
        // `interpolation` children carry real refs, the literal text
        // does not. Descend only into interpolation.
        if kind == "string" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "interpolation" {
                    self.walk(child, scope);
                }
            }
            return;
        }

        match kind {
            "function_definition" => self.walk_fn(node, scope),
            "lambda" => self.walk_lambda(node, scope),
            "class_definition" => self.walk_class(node, scope),
            "assignment" => self.walk_assignment(node, scope),
            "identifier" => self.emit_ref(node, scope),
            _ => self.walk_children(node, scope),
        }
    }

    fn walk_children(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, scope);
        }
    }

    fn walk_fn(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(parent, name_node, DefKind::Function);
        }
        let fn_scope = self.tree.push_scope(ScopeKind::Function, parent);
        if let Some(params) = node.child_by_field_name("parameters") {
            self.walk_param_list(params, fn_scope);
        }
        if let Some(ret) = node.child_by_field_name("return_type") {
            self.walk(ret, fn_scope);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk_children(body, fn_scope);
        }
    }

    fn walk_lambda(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        let fn_scope = self.tree.push_scope(ScopeKind::Function, parent);
        if let Some(params) = node.child_by_field_name("parameters") {
            self.walk_param_list(params, fn_scope);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, fn_scope);
        }
    }

    fn walk_param_list(&mut self, params: tree_sitter::Node, fn_scope: ScopeId) {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            match child.kind() {
                "identifier" => self.add_binding(fn_scope, child, DefKind::Param),
                "default_parameter" | "typed_default_parameter" => {
                    // `x = expr` — value sees the outer scope, then
                    // the name is bound.
                    if let Some(v) = child.child_by_field_name("value") {
                        self.walk(v, fn_scope);
                    }
                    if let Some(t) = child.child_by_field_name("type") {
                        self.walk(t, fn_scope);
                    }
                    if let Some(name) = child.child_by_field_name("name") {
                        if name.kind() == "identifier" {
                            self.add_binding(fn_scope, name, DefKind::Param);
                        }
                    }
                }
                "typed_parameter" => {
                    // `x: int` — first child is identifier (no field
                    // name in this grammar version), then `:` and
                    // `type` field.
                    if let Some(t) = child.child_by_field_name("type") {
                        self.walk(t, fn_scope);
                    }
                    let mut inner = child.walk();
                    for grand in child.children(&mut inner) {
                        if grand.kind() == "identifier" {
                            self.add_binding(fn_scope, grand, DefKind::Param);
                            break;
                        }
                    }
                }
                "list_splat_pattern" | "dictionary_splat_pattern" => {
                    let mut inner = child.walk();
                    for grand in child.children(&mut inner) {
                        if grand.kind() == "identifier" {
                            self.add_binding(fn_scope, grand, DefKind::Param);
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_class(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(parent, name_node, DefKind::Type);
        }
        // Walk superclasses in the parent scope (ref-bearing position).
        if let Some(supers) = node.child_by_field_name("superclasses") {
            self.walk(supers, parent);
        }
        // Body opens a class scope. Python's LEGB-class-quirk (class
        // body names are NOT visible to nested defs) is intentionally
        // not modelled here — see the module-level doc comment.
        let class_scope = self.tree.push_scope(ScopeKind::Class, parent);
        if let Some(body) = node.child_by_field_name("body") {
            self.walk_children(body, class_scope);
        }
    }

    fn walk_assignment(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        // RHS first so `x = x + 1` sees the outer `x` before binding
        // a new one in the same scope.
        if let Some(right) = node.child_by_field_name("right") {
            self.walk(right, scope);
        }
        if let Some(ty) = node.child_by_field_name("type") {
            self.walk(ty, scope);
        }
        if let Some(left) = node.child_by_field_name("left") {
            self.bind_target(left, scope);
        }
    }

    fn bind_target(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        match node.kind() {
            "identifier" => self.add_binding(scope, node, DefKind::Variable),
            "tuple_pattern" | "list_pattern" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.bind_target(child, scope);
                }
            }
            // `obj.attr = ...` is not a new binding; `attr` is an
            // attribute access on `obj`. Walk so refs to `obj` get
            // captured.
            _ => self.walk(node, scope),
        }
    }

    fn add_binding(&mut self, scope: ScopeId, name_node: tree_sitter::Node, kind: DefKind) {
        let name = name_node.utf8_text(self.content.as_bytes()).unwrap_or("");
        if name.is_empty() {
            return;
        }
        let line = name_node.start_position().row + 1;
        self.tree.add_binding(
            scope,
            name.to_string(),
            LocalDef {
                line,
                kind,
                import_path: None,
            },
        );
    }

    fn emit_ref(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        let text = node.utf8_text(self.content.as_bytes()).unwrap_or("");
        if !is_meaningful_identifier(text) {
            return;
        }
        let line = node.start_position().row + 1;
        let col = node.start_position().column + 1;
        let target = self.resolve(scope, text);
        self.refs.push(BoundRef {
            name: text.to_string(),
            line,
            col,
            target,
            kind: RefKind::Value,
        });
    }

    fn resolve(&self, scope: ScopeId, name: &str) -> BindTarget {
        match self.tree.resolve(scope, name) {
            Some((sid, def)) => {
                if def.kind == DefKind::Import {
                    if let Some(path) = &def.import_path {
                        return BindTarget::Imported(path.clone());
                    }
                }
                if sid == self.tree.root() {
                    if let Some(&idx) = self.file_symbols_by_name.get(name) {
                        return BindTarget::ModuleSymbol(idx);
                    }
                    BindTarget::Local(sid)
                } else {
                    BindTarget::Local(sid)
                }
            }
            None => BindTarget::Unresolved,
        }
    }
}

fn is_py_comment_kind(kind: &str) -> bool {
    kind == "comment"
}
