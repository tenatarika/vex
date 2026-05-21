//! Rust scope binder (11.1.2b).
//!
//! Walks the tree-sitter-rust AST building a [`ScopeTree`] and emits a
//! [`BoundRef`] per identifier reference, tagged with the scope where
//! it resolves (`Local`), the file-level symbol it points at
//! (`ModuleSymbol`), or `Unresolved` when neither path finds a match.
//!
//! In-file resolution only. Cross-file resolution via the `use` graph
//! is 11.1.2c; persistence in `reference_edges` is 11.1.3.
//!
//! ## What's handled
//!
//! - `function_item` — name bound in parent scope; params bound in fn
//!   scope; return type + body walked as ref-bearing subtrees.
//! - `let_declaration` — type + value walked first (refs see the
//!   pre-binding scope), then the pattern is bound.
//! - `struct_item` / `enum_item` / `trait_item` / `type_item` /
//!   `const_item` / `static_item` — name bound in parent scope.
//! - `mod_item` — name bound in parent scope, body walked in a child
//!   module scope.
//! - `impl_item` — body walked in a child impl scope.
//! - `block` — opens a child block scope.
//!
//! ## What's deferred
//!
//! - Destructuring patterns (tuple, struct, tuple-struct, slice, or-pat).
//!   `bind_pattern` only handles plain `identifier` and its
//!   `mut_pattern` / `ref_pattern` / `reference_pattern` wrappers.
//! - `closure_expression`, `for_expression`, `while_let_expression`,
//!   `if_let_expression`, `match_arm` pattern bindings.
//! - Generic parameters and lifetimes — refs to a generic `T` stay
//!   `Unresolved`.
//! - `use` paths and aliasing — 11.1.2c.
//! - Macros — `macro_invocation` arguments still flow through the
//!   identifier matcher, so refs inside macros can be captured but the
//!   binder can't follow what the macro expands to.

use anyhow::{Context, Result};

use super::{
    BindTarget, BoundRef, DefKind, LocalDef, RefKind, ScopeBinder, ScopeId, ScopeKind, ScopeTree,
    UsePath,
};
use crate::index::symbols::ParsedSymbol;
use crate::parse::extractor::is_meaningful_identifier;
use crate::parse::language::Language;

pub struct RustBinder;

impl ScopeBinder for RustBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&Language::Rust.ts_language())
            .context("set language for rust binder")?;
        let tree = parser
            .parse(content, None)
            .context("tree-sitter parse failed in rust binder")?;

        let mut walker = Walker {
            content,
            file_symbols,
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
    file_symbols: &'a [ParsedSymbol],
    tree: ScopeTree,
    refs: Vec<BoundRef>,
}

impl<'a> Walker<'a> {
    fn walk(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        let kind = node.kind();

        // 11.1.1 noise filter — comments and string literals never
        // produce binder-visible refs.
        if is_rust_comment_kind(kind) || is_rust_string_kind(kind) {
            return;
        }

        match kind {
            "function_item" => self.walk_fn(node, scope),
            "let_declaration" => self.walk_let(node, scope),
            "struct_item" | "enum_item" | "trait_item" | "type_item" => {
                self.bind_named_decl(node, scope, DefKind::Type);
            }
            "const_item" | "static_item" => self.walk_const_like(node, scope),
            "mod_item" => self.walk_mod(node, scope),
            "impl_item" => self.walk_impl(node, scope),
            "use_declaration" => self.walk_use(node, scope),
            "block" => {
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

    fn walk_fn(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(parent, name_node, DefKind::Function);
        }
        let fn_scope = self.tree.push_scope(ScopeKind::Function, parent);

        if let Some(params) = node.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for child in params.children(&mut cursor) {
                if child.kind() == "parameter" {
                    if let Some(pat) = child.child_by_field_name("pattern") {
                        self.bind_pattern(pat, fn_scope, DefKind::Param);
                    }
                    if let Some(ty) = child.child_by_field_name("type") {
                        self.walk(ty, fn_scope);
                    }
                }
                // `self_parameter` is implicit; no binding needed here.
            }
        }

        if let Some(ret) = node.child_by_field_name("return_type") {
            self.walk(ret, fn_scope);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, fn_scope);
        }
    }

    fn walk_let(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        if let Some(ty) = node.child_by_field_name("type") {
            self.walk(ty, scope);
        }
        if let Some(value) = node.child_by_field_name("value") {
            self.walk(value, scope);
        }
        // `let Ok(x) = foo() else { return default; }` — the `else`
        // block is a regular ref-bearing subtree (the `block` arm in
        // `walk` opens its own child scope), so dispatch it through
        // the main walker.
        if let Some(alt) = node.child_by_field_name("alternative") {
            self.walk(alt, scope);
        }
        if let Some(pat) = node.child_by_field_name("pattern") {
            self.bind_pattern(pat, scope, DefKind::Variable);
        }
    }

    fn walk_const_like(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(scope, name_node, DefKind::Variable);
        }
        if let Some(ty) = node.child_by_field_name("type") {
            self.walk(ty, scope);
        }
        if let Some(value) = node.child_by_field_name("value") {
            self.walk(value, scope);
        }
    }

    fn walk_mod(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(parent, name_node, DefKind::Module);
        }
        let mod_scope = self.tree.push_scope(ScopeKind::Module, parent);
        if let Some(body) = node.child_by_field_name("body") {
            self.walk_children(body, mod_scope);
        }
    }

    fn walk_impl(&mut self, node: tree_sitter::Node, parent: ScopeId) {
        let impl_scope = self.tree.push_scope(ScopeKind::Impl, parent);
        // `impl Foo for Bar` — `type` and `trait` are ref-bearing
        // positions and live in the parent scope; the method bodies
        // (`declaration_list` under `body`) live in the child impl
        // scope. Dispatch by field name rather than skipping by node-
        // kind string so a future grammar rename of `declaration_list`
        // can't double-emit the body or silently drop the type/trait.
        if let Some(ty) = node.child_by_field_name("type") {
            self.walk(ty, parent);
        }
        if let Some(tr) = node.child_by_field_name("trait") {
            self.walk(tr, parent);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk_children(body, impl_scope);
        }
    }

    fn bind_named_decl(&mut self, node: tree_sitter::Node, scope: ScopeId, kind: DefKind) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.add_binding(scope, name_node, kind);
        }
        // We don't recurse into the body for 11.1.2b — generic params,
        // field types, and trait method bodies have edge cases the
        // simple walker would mishandle. 11.1.2c picks them up.
    }

    fn bind_pattern(&mut self, pat: tree_sitter::Node, scope: ScopeId, kind: DefKind) {
        match pat.kind() {
            "identifier" => self.add_binding(scope, pat, kind),
            "mut_pattern" | "ref_pattern" | "reference_pattern" => {
                let mut cursor = pat.walk();
                for child in pat.children(&mut cursor) {
                    self.bind_pattern(child, scope, kind);
                }
            }
            // Destructuring patterns: deferred. The names inside silently
            // stay unbound for 11.1.2b — they'll show up as `Unresolved`
            // refs later in the file if used.
            _ => {}
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

    /// Process a `use_declaration` node. The use path is a binding
    /// site, not a reference site — children are *not* emitted as refs
    /// (otherwise the line scanner inside 11.1.1 would have already
    /// done that work). Instead we walk the path tree via
    /// [`collect_use_imports`] and stamp `(local_name, UsePath)` pairs
    /// into the current scope as `DefKind::Import` bindings.
    fn walk_use(&mut self, node: tree_sitter::Node, scope: ScopeId) {
        let line = node.start_position().row + 1;
        let mut imports: Vec<(String, UsePath)> = Vec::new();
        if let Some(arg) = node.child_by_field_name("argument") {
            collect_use_imports(arg, &[], self.content, &mut imports);
        }
        for (name, path) in imports {
            self.tree.add_binding(
                scope,
                name,
                LocalDef {
                    line,
                    kind: DefKind::Import,
                    import_path: Some(path),
                },
            );
        }
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
                // Import wins over a same-name `ModuleSymbol`: in valid
                // Rust a `use foo as Bar;` cannot coexist with a local
                // `struct Bar;` in the same scope (compile error), so
                // the ordering is harmless and matches the language.
                if def.kind == DefKind::Import {
                    if let Some(path) = &def.import_path {
                        return BindTarget::Imported(path.clone());
                    }
                }
                if sid == self.tree.root() {
                    // Prefer ModuleSymbol when the file-level resolution
                    // also matches a ParsedSymbol entry — 11.1.3 will use
                    // the idx to look up the global symbol id.
                    if let Some(idx) = self.file_symbols.iter().position(|s| s.name == name) {
                        return BindTarget::ModuleSymbol(idx as u32);
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

fn is_rust_comment_kind(kind: &str) -> bool {
    matches!(kind, "line_comment" | "block_comment")
}

fn is_rust_string_kind(kind: &str) -> bool {
    // tree-sitter-rust 0.24 parses `b"..."` as `string_literal` too
    // (the lexer prefix is part of the same rule), so the three kinds
    // below cover every string-shaped literal in current Rust.
    matches!(
        kind,
        "string_literal" | "raw_string_literal" | "char_literal"
    )
}

/// Recursively flatten the path argument of a `use_declaration` into a
/// list of `(local_name, full_use_path)` pairs. `prefix` accumulates
/// segments while we descend through `scoped_use_list` / nested forms.
///
/// Handled shapes:
///   `use a;`                          → `[(a, [a])]`
///   `use a::b;`                       → `[(b, [a, b])]`
///   `use a::b::C;`                    → `[(C, [a, b, C])]`
///   `use a::{b, c};`                  → `[(b, [a, b]), (c, [a, c])]`
///   `use a::{b::C, d as E};`          → `[(C, [a, b, C]), (E, [a, d])]`
///   `use a as alias;`                 → `[(alias, [a])]`
///   `use a::*;`                       → no bindings (glob deferred to 11.1.3 cross-file pass).
fn collect_use_imports(
    node: tree_sitter::Node,
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
            // Get the raw `a::b::c` text and split — robust against
            // minor grammar shape differences for nested scoped paths.
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
            let path_node = node.child_by_field_name("path");
            let alias_node = node.child_by_field_name("alias");
            let path_text = path_node
                .and_then(|n| n.utf8_text(content.as_bytes()).ok())
                .unwrap_or("");
            let mut segments = prefix.to_vec();
            segments.extend(
                path_text
                    .split("::")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
            if let Some(alias) = alias_node {
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
            let path_node = node.child_by_field_name("path");
            let list_node = node.child_by_field_name("list");
            let path_text = path_node
                .and_then(|n| n.utf8_text(content.as_bytes()).ok())
                .unwrap_or("");
            let mut new_prefix = prefix.to_vec();
            new_prefix.extend(
                path_text
                    .split("::")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
            if let Some(list) = list_node {
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
        // `use a::*;` — glob. We deliberately emit no binding here; the
        // 11.1.3 cross-file pass will follow the prefix and pull the
        // exported names from the target module's symbol FST.
        "use_wildcard" => {}
        _ => {}
    }
}
