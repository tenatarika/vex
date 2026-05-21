//! C# scope binder (11.1.5).
//!
//! Walks the tree-sitter-c-sharp AST building a [`ScopeTree`] and emits
//! a [`BoundRef`] per identifier reference. In-file resolution only —
//! `using` import resolution is a follow-up.
//!
//! ## What's handled
//!
//! - `method_declaration`, `constructor_declaration`,
//!   `local_function_statement` — name bound in parent scope; params
//!   bound in fn scope; body walked.
//! - `class_declaration`, `interface_declaration`, `struct_declaration`,
//!   `enum_declaration`, `record_declaration` — name bound in parent.
//! - `variable_declarator` (under local declarations) — bind pattern;
//!   value walked first so the RHS sees the pre-binding scope.
//! - `block` — opens a child block scope.
//!
//! ## What's deferred
//!
//! - `using` import resolution (cross-file like 11.1.2c for Rust).
//! - Destructuring patterns.
//! - Tuple patterns in deconstruction.
//! - Generic type parameters.

use anyhow::{Context, Result};

use super::{
    BindTarget, BoundRef, DefKind, LocalDef, RefKind, ScopeBinder, ScopeId, ScopeKind, ScopeTree,
};
use crate::index::symbols::ParsedSymbol;
use crate::parse::extractor::is_meaningful_identifier;
use crate::parse::language::Language;

pub struct CSharpBinder;

impl ScopeBinder for CSharpBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&Language::CSharp.ts_language())
            .context("set language for c# binder")?;
        let tree = parser
            .parse(content, None)
            .context("tree-sitter parse failed in c# binder")?;

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

        if is_cs_comment_kind(kind) || is_cs_plain_string_kind(kind) {
            return;
        }
        if kind == "interpolated_string_expression" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "interpolation" {
                    self.walk(child, scope);
                }
            }
            return;
        }

        match kind {
            "method_declaration"
            | "constructor_declaration"
            | "destructor_declaration"
            | "local_function_statement" => self.walk_named_fn(node, scope),
            "variable_declarator" => self.walk_var_declarator(node, scope),
            "class_declaration"
            | "interface_declaration"
            | "struct_declaration"
            | "enum_declaration"
            | "record_declaration" => self.walk_class_like(node, scope),
            "delegate_declaration" => self.bind_named_decl(node, scope, DefKind::Type),
            "block" => {
                let s = self.tree.push_scope(ScopeKind::Block, scope);
                self.walk_children(node, s);
            }
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

    fn walk_named_fn(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(parent, name_node, DefKind::Function);
        }
        let fn_scope = self.tree.push_scope(ScopeKind::Function, parent);
        if let Some(params) = node.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for child in params.children(&mut cursor) {
                if child.kind() == "parameter" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        self.add_binding(fn_scope, name_node, DefKind::Param);
                    }
                    if let Some(ty) = child.child_by_field_name("type") {
                        self.walk(ty, fn_scope);
                    }
                }
            }
        }
        if let Some(ty) = node.child_by_field_name("type") {
            self.walk(ty, fn_scope);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, fn_scope);
        }
    }

    fn walk_var_declarator(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        // `var x = expr;` — the expression on the RHS should see the
        // outer scope (consistent with `let` semantics in Rust/TS).
        // tree-sitter-c-sharp 0.23 marks only `name:` as a field; the
        // value sits as a positional child after the `=` token, so we
        // iterate children and walk anything that's neither the name
        // nor the literal `=`.
        let name_node = node.child_by_field_name("name");
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if Some(child.id()) == name_node.map(|n| n.id()) {
                continue;
            }
            if child.kind() == "=" {
                continue;
            }
            self.walk(child, scope);
        }
        if let Some(name) = name_node {
            self.add_binding(scope, name, DefKind::Variable);
        }
    }

    fn walk_class_like(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        let name_node = node.child_by_field_name("name");
        if let Some(n) = name_node {
            self.add_binding(parent, n, DefKind::Type);
        }
        let class_scope = self.tree.push_scope(ScopeKind::Class, parent);
        // tree-sitter-c-sharp 0.23 uses `body: declaration_list`. Walk
        // every child under the class scope EXCEPT the `name` field
        // (already bound — re-emitting it would produce a phantom
        // self-ref), the heritage list (whose refs live in the
        // parent scope), and attribute lists (`[Serializable]`,
        // `[JsonProperty("x")]` etc. — those identifiers are
        // annotation labels rather than type or value refs and bloat
        // the ref table without resolving to anything useful).
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if Some(child.id()) == name_node.map(|n| n.id()) {
                continue;
            }
            match child.kind() {
                "base_list" | "type_parameter_constraints_clause" => {
                    self.walk(child, parent);
                }
                "attribute_list" => {}
                _ => self.walk(child, class_scope),
            }
        }
    }

    fn bind_named_decl(&mut self, node: tree_sitter::Node, scope: ScopeId, kind: DefKind) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(scope, name_node, kind);
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
        // C# uses plain `identifier` for both type and value positions
        // — without a type-vs-value namespace split at the node level
        // we default to `Value`. The packed `RefKind` lives in the
        // persistent edge so a future pass can refine this.
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

fn is_cs_comment_kind(kind: &str) -> bool {
    kind == "comment"
}

fn is_cs_plain_string_kind(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal" | "verbatim_string_literal" | "raw_string_literal" | "character_literal"
    )
}
