//! TypeScript scope binder (11.1.4a).
//!
//! Walks the tree-sitter-typescript (TSX) AST building a [`ScopeTree`]
//! and emits a [`BoundRef`] per identifier reference. In-file resolution
//! only — module imports land in 11.1.4b.
//!
//! ## What's handled
//!
//! - `function_declaration`, `method_definition` — name bound in parent
//!   scope; params bound in fn scope; body walked.
//! - `arrow_function` — anonymous, opens a fn scope; params + body
//!   walked.
//! - `variable_declarator` (under `lexical_declaration` / `variable_-
//!   declaration`) — bind pattern names; the `value` expression is
//!   walked first so refs see the pre-binding scope.
//! - `class_declaration`, `interface_declaration`,
//!   `type_alias_declaration`, `enum_declaration` — name bound in
//!   parent scope.
//! - `statement_block` — opens a child block scope.
//!
//! ## What's deferred
//!
//! - JSX `<Foo />` and declaration merging (interface + class same
//!   name) — 11.1.4c.
//! - Destructuring patterns (`{ a, b }`, `[x, y]`, rest).
//! - Generic type parameters — refs to a generic `T` stay
//!   `Unresolved`.
//! - Module imports — 11.1.4b.

use anyhow::{Context, Result};

use super::{
    BindTarget, BoundRef, DefKind, LocalDef, RefKind, ScopeBinder, ScopeId, ScopeKind, ScopeTree,
    UsePath,
};
use crate::index::symbols::ParsedSymbol;
use crate::parse::extractor::is_meaningful_identifier;
use crate::parse::language::Language;

pub struct TypeScriptBinder;

impl ScopeBinder for TypeScriptBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&Language::TypeScript.ts_language())
            .context("set language for typescript binder")?;
        let tree = parser
            .parse(content, None)
            .context("tree-sitter parse failed in typescript binder")?;

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

        if is_ts_comment_kind(kind) || is_ts_plain_string_kind(kind) {
            return;
        }
        if kind == "template_string" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "template_substitution" {
                    self.walk(child, scope);
                }
            }
            return;
        }

        match kind {
            "function_declaration" | "method_definition" => self.walk_named_fn(node, scope),
            "arrow_function" | "function_expression" => self.walk_anonymous_fn(node, scope),
            "variable_declarator" => self.walk_var_declarator(node, scope),
            "class_declaration" | "interface_declaration" => {
                self.walk_class_like(node, scope, DefKind::Type)
            }
            "type_alias_declaration" => self.bind_named_decl(node, scope, DefKind::Type),
            "enum_declaration" => self.bind_named_decl(node, scope, DefKind::Type),
            "import_statement" => self.walk_import_statement(node, scope),
            "statement_block" => {
                let s = self.tree.push_scope(ScopeKind::Block, scope);
                self.walk_children(node, s);
            }
            "identifier" | "type_identifier" => self.emit_ref(node, scope),
            _ => self.walk_children(node, scope),
        }
    }

    fn walk_children(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, scope);
        }
    }

    fn walk_named_fn(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(parent, name_node, DefKind::Function);
        }
        let fn_scope = self.tree.push_scope(ScopeKind::Function, parent);
        self.walk_fn_params(node, fn_scope);
        self.walk_optional_field(node, "return_type", fn_scope);
        self.walk_optional_field(node, "body", fn_scope);
    }

    fn walk_anonymous_fn(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        let fn_scope = self.tree.push_scope(ScopeKind::Function, parent);
        self.walk_fn_params(node, fn_scope);
        self.walk_optional_field(node, "return_type", fn_scope);
        self.walk_optional_field(node, "body", fn_scope);
    }

    fn walk_fn_params(&mut self, node: tree_sitter::Node, fn_scope: ScopeId) {
        let Some(params) = node.child_by_field_name("parameters") else {
            return;
        };
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            match child.kind() {
                "required_parameter" | "optional_parameter" => {
                    if let Some(pat) = child.child_by_field_name("pattern") {
                        self.bind_pattern(pat, fn_scope, DefKind::Param);
                    }
                    if let Some(ty) = child.child_by_field_name("type") {
                        self.walk(ty, fn_scope);
                    }
                    if let Some(value) = child.child_by_field_name("value") {
                        self.walk(value, fn_scope);
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_var_declarator(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        // Same ordering as rust.rs::walk_let — the value sees the
        // pre-binding scope so `const x = x;` correctly resolves the
        // RHS to the outer `x` rather than the about-to-bind one.
        if let Some(ty) = node.child_by_field_name("type") {
            self.walk(ty, scope);
        }
        if let Some(value) = node.child_by_field_name("value") {
            self.walk(value, scope);
        }
        if let Some(name) = node.child_by_field_name("name") {
            self.bind_pattern(name, scope, DefKind::Variable);
        }
    }

    fn walk_class_like(&mut self, node: tree_sitter::Node, parent: ScopeId, kind: DefKind) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(parent, name_node, kind);
        }
        let class_scope = self.tree.push_scope(ScopeKind::Class, parent);
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_heritage" | "extends_clause" | "implements_clause" => {
                    self.walk(child, parent);
                }
                "class_body" | "interface_body" | "object_type" => {
                    self.walk_children(child, class_scope);
                }
                _ => {}
            }
        }
    }

    fn bind_named_decl(&mut self, node: tree_sitter::Node, scope: ScopeId, kind: DefKind) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(scope, name_node, kind);
        }
    }

    /// Process an `import_statement`. Children are NOT emitted as refs
    /// (the import path is a binding site). Bindings get tagged
    /// `DefKind::Import` with the dotted module path so Pass-2
    /// cross-file resolution can rewrite them into `to_sym_idx`.
    fn walk_import_statement(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        let line = node.start_position().row + 1;
        let Some(source) = node
            .child_by_field_name("source")
            .and_then(|s| extract_string_text(s, self.content))
        else {
            return;
        };
        if source.is_empty() {
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "import_clause" {
                self.walk_import_clause(child, scope, line, &source);
            }
        }
    }

    fn walk_import_clause(
        &mut self,
        clause: tree_sitter::Node,
        scope: ScopeId,
        line: usize,
        source: &str,
    ) {
        let mut cursor = clause.walk();
        for child in clause.children(&mut cursor) {
            match child.kind() {
                "identifier" => {
                    let name = child.utf8_text(self.content.as_bytes()).unwrap_or("");
                    if !name.is_empty() {
                        self.tree.add_binding(
                            scope,
                            name.to_string(),
                            LocalDef {
                                line,
                                kind: DefKind::Import,
                                import_path: Some(UsePath {
                                    segments: vec![source.to_string()],
                                }),
                            },
                        );
                    }
                }
                "namespace_import" => {
                    let mut inner = child.walk();
                    for grand in child.children(&mut inner) {
                        if grand.kind() == "identifier" {
                            let alias = grand.utf8_text(self.content.as_bytes()).unwrap_or("");
                            if !alias.is_empty() {
                                self.tree.add_binding(
                                    scope,
                                    alias.to_string(),
                                    LocalDef {
                                        line,
                                        kind: DefKind::Import,
                                        import_path: Some(UsePath {
                                            segments: vec![source.to_string()],
                                        }),
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
                                .and_then(|n| n.utf8_text(self.content.as_bytes()).ok())
                                .unwrap_or("");
                            let local = grand
                                .child_by_field_name("alias")
                                .and_then(|n| n.utf8_text(self.content.as_bytes()).ok())
                                .unwrap_or(original);
                            if !original.is_empty() && !local.is_empty() {
                                self.tree.add_binding(
                                    scope,
                                    local.to_string(),
                                    LocalDef {
                                        line,
                                        kind: DefKind::Import,
                                        import_path: Some(UsePath {
                                            segments: vec![
                                                source.to_string(),
                                                original.to_string(),
                                            ],
                                        }),
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

    fn bind_pattern(&mut self, pat: tree_sitter::Node, scope: ScopeId, kind: DefKind) {
        if pat.kind() == "identifier" {
            self.add_binding(scope, pat, kind);
        }
        // Destructuring patterns are deferred — names inside an object
        // or array pattern silently stay unbound until 11.1.4c.
    }

    fn walk_optional_field(&mut self, node: tree_sitter::Node, field: &str, scope: ScopeId) {
        if let Some(child) = node.child_by_field_name(field) {
            self.walk(child, scope);
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

fn is_ts_comment_kind(kind: &str) -> bool {
    kind == "comment"
}

fn is_ts_plain_string_kind(kind: &str) -> bool {
    matches!(kind, "string" | "regex")
}

/// Return the textual content of a TypeScript `string` node — the
/// `string_fragment` child without the surrounding quote tokens.
fn extract_string_text(node: tree_sitter::Node, content: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string_fragment" {
            return child.utf8_text(content.as_bytes()).ok().map(String::from);
        }
    }
    None
}
