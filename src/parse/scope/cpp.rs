//! C++ scope binder (11.1.5).
//!
//! Walks the tree-sitter-cpp AST building a [`ScopeTree`] and emits a
//! [`BoundRef`] per identifier reference. In-file resolution only —
//! `#include` resolution and namespace traversal are follow-ups.
//!
//! ## What's handled
//!
//! - `function_definition`, `declaration` (function prototype) — the
//!   name is buried under the `declarator` chain (`function_declarator
//!   → declarator → identifier / field_identifier / qualified_-
//!   identifier`); see [`extract_inner_identifier`].
//! - `class_specifier`, `struct_specifier`, `union_specifier`,
//!   `enum_specifier` — name bound in parent (Type kind).
//! - `namespace_definition` — name bound in parent, body walked in a
//!   child Module scope.
//! - `init_declarator` (local var with initializer) — bind name from
//!   inner declarator, walk value first.
//! - `parameter_declaration` — bind declarator's identifier.
//! - `compound_statement` — block scope.
//!
//! ## What's deferred
//!
//! - Templates and generic parameters.
//! - `using namespace` / `using ns::Name` resolution.
//! - Qualified names spanning headers.
//! - Multiple declarators in one `declaration` (`int a, b, c;`).

use anyhow::{Context, Result};

use super::{
    BindTarget, BoundRef, DefKind, LocalDef, RefKind, ScopeBinder, ScopeId, ScopeKind, ScopeTree,
};
use crate::index::symbols::ParsedSymbol;
use crate::parse::extractor::is_meaningful_identifier;
use crate::parse::language::Language;

pub struct CppBinder;

impl ScopeBinder for CppBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&Language::Cpp.ts_language())
            .context("set language for c++ binder")?;
        let tree = parser
            .parse(content, None)
            .context("tree-sitter parse failed in c++ binder")?;

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

        if is_cpp_comment_kind(kind) || is_cpp_plain_string_kind(kind) {
            return;
        }

        match kind {
            "function_definition" => self.walk_fn_def(node, scope),
            "namespace_definition" => self.walk_namespace(node, scope),
            "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier" => {
                self.walk_class_like(node, scope)
            }
            "init_declarator" => self.walk_init_declarator(node, scope),
            "parameter_declaration" => self.walk_parameter(node, scope),
            "compound_statement" => {
                let s = self.tree.push_scope(ScopeKind::Block, scope);
                self.walk_children(node, s);
            }
            "identifier" | "type_identifier" | "field_identifier" => self.emit_ref(node, scope),
            _ => self.walk_children(node, scope),
        }
    }

    fn walk_children(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, scope);
        }
    }

    fn walk_fn_def(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        let declarator = node.child_by_field_name("declarator");
        let name_node = declarator.and_then(extract_inner_identifier);
        if let Some(n) = name_node {
            self.add_binding(parent, n, DefKind::Function);
        }
        let fn_scope = self.tree.push_scope(ScopeKind::Function, parent);
        // Return type is a ref-bearing position.
        if let Some(t) = node.child_by_field_name("type") {
            self.walk(t, fn_scope);
        }
        // Walk parameters via the function_declarator's `parameters` field.
        if let Some(fd) = declarator {
            if let Some(params) = fd.child_by_field_name("parameters") {
                self.walk(params, fn_scope);
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, fn_scope);
        }
    }

    fn walk_namespace(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(parent, name_node, DefKind::Module);
        }
        let ns_scope = self.tree.push_scope(ScopeKind::Module, parent);
        if let Some(body) = node.child_by_field_name("body") {
            self.walk_children(body, ns_scope);
        }
    }

    fn walk_class_like(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        let name_node = node.child_by_field_name("name");
        if let Some(n) = name_node {
            self.add_binding(parent, n, DefKind::Type);
        }
        let class_scope = self.tree.push_scope(ScopeKind::Class, parent);
        // Heritage clauses (`base_class_clause`) live in the parent
        // scope; the body is the field_declaration_list.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if Some(child.id()) == name_node.map(|n| n.id()) {
                continue;
            }
            match child.kind() {
                "base_class_clause" => self.walk(child, parent),
                _ => self.walk(child, class_scope),
            }
        }
    }

    fn walk_init_declarator(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        let name_node = node
            .child_by_field_name("declarator")
            .and_then(extract_inner_identifier);
        // Value sees the pre-binding scope so `int x = x;` resolves
        // the RHS against an outer `x`.
        if let Some(v) = node.child_by_field_name("value") {
            self.walk(v, scope);
        }
        if let Some(n) = name_node {
            self.add_binding(scope, n, DefKind::Variable);
        }
    }

    fn walk_parameter(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        if let Some(t) = node.child_by_field_name("type") {
            self.walk(t, scope);
        }
        if let Some(d) = node.child_by_field_name("declarator") {
            if let Some(n) = extract_inner_identifier(d) {
                self.add_binding(scope, n, DefKind::Param);
            }
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
        let kind = if node.kind() == "type_identifier" {
            RefKind::Type
        } else {
            RefKind::Value
        };
        self.refs.push(BoundRef {
            name: text.to_string(),
            line,
            col,
            target,
            kind,
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

/// Descend a C++ declarator chain looking for the innermost identifier
/// that names the entity being declared. Handles wrappers like
/// `pointer_declarator`, `reference_declarator`, `array_declarator`,
/// `function_declarator`, and `parenthesized_declarator` by following
/// their `declarator` field. For `qualified_identifier` we take the
/// `name` field — e.g. `Foo::bar` returns the `bar` identifier.
fn extract_inner_identifier(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut cur = node;
    // Bound the loop so a malformed AST cycle can't hang.
    for _ in 0..32 {
        match cur.kind() {
            "identifier" | "type_identifier" | "field_identifier" => return Some(cur),
            "qualified_identifier" => {
                // `Outer::Inner::name` parses as nested
                // `qualified_identifier` nodes whose `name` field is
                // itself another `qualified_identifier`. Peel and
                // re-enter the loop until we reach the terminal
                // identifier — a single `child_by_field_name("name")`
                // call only handles one level of qualification.
                cur = cur.child_by_field_name("name")?;
            }
            _ => {
                cur = cur.child_by_field_name("declarator")?;
            }
        }
    }
    None
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
