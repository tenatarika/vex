//! Shared scope-walker scaffolding used by every per-language binder.
//!
//! Each binder used to carry its own copy of `Walker`, `add_binding`,
//! `emit_ref`, and `resolve` — five identical 20-line methods that
//! would drift the moment one language's `resolve()` was tweaked.
//! This module consolidates them so language files only own the
//! tree-walk dispatch.
//!
//! Dispatch is wired in via a function pointer (`DispatchFn`) rather
//! than a trait so per-language helpers can stay as free functions
//! without becoming generic or boxing their state.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tree_sitter::{Node, Tree};

use super::{BindTarget, BoundRef, DefKind, LocalDef, RefKind, ScopeId, ScopeTree, UsePath};
use crate::index::symbols::ParsedSymbol;
use crate::parse::extractor::is_meaningful_identifier;
use crate::parse::language::Language;

/// Per-language tree dispatch — matches on node kind, recurses via
/// `Walker::walk_children`, and emits bindings + refs.
pub(super) type DispatchFn = fn(&mut Walker<'_>, Node, ScopeId);

pub(super) struct Walker<'a> {
    pub(super) content: &'a str,
    pub(super) file_symbols_by_name: HashMap<&'a str, u32>,
    pub(super) tree: ScopeTree,
    pub(super) refs: Vec<BoundRef>,
    dispatch: DispatchFn,
}

impl<'a> Walker<'a> {
    pub(super) fn new(
        content: &'a str,
        file_symbols: &'a [ParsedSymbol],
        dispatch: DispatchFn,
    ) -> Self {
        // Multiple symbols with the same name keep the *first* index —
        // mirrors the prior `iter().position` behaviour. Cross-file
        // ambiguity is resolved later in the writer's Pass-2.
        let mut by_name: HashMap<&'a str, u32> = HashMap::with_capacity(file_symbols.len());
        for (i, s) in file_symbols.iter().enumerate() {
            by_name.entry(s.name.as_str()).or_insert(i as u32);
        }
        Self {
            content,
            file_symbols_by_name: by_name,
            tree: ScopeTree::new(),
            refs: Vec::new(),
            dispatch,
        }
    }

    /// Drive the walk from the parser's root node and return the
    /// emitted refs. Consumes the walker.
    pub(super) fn run(mut self, tree: &Tree) -> Vec<BoundRef> {
        let root = self.tree.root();
        self.walk(tree.root_node(), root);
        self.refs
    }

    pub(super) fn walk(&mut self, node: Node, scope: ScopeId) {
        (self.dispatch)(self, node, scope);
    }

    pub(super) fn walk_children(&mut self, node: Node, scope: ScopeId) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, scope);
        }
    }

    pub(super) fn add_binding(&mut self, scope: ScopeId, name_node: Node, kind: DefKind) {
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

    pub(super) fn add_import_binding(
        &mut self,
        scope: ScopeId,
        name: impl Into<String>,
        line: usize,
        path: UsePath,
    ) {
        let name = name.into();
        if name.is_empty() {
            return;
        }
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

    pub(super) fn emit_ref(&mut self, node: Node, scope: ScopeId, kind: RefKind) {
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
            kind,
        });
    }

    pub(super) fn resolve(&self, scope: ScopeId, name: &str) -> BindTarget {
        match self.tree.resolve(scope, name) {
            Some((sid, def)) => {
                // Import binding always wins; the language semantics
                // make `use foo as Bar` and a local `Bar` definition
                // mutually exclusive in the same scope (compile error
                // in Rust, name collision in TS/Python/C#).
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

/// Parse `content` with the given language's tree-sitter grammar.
/// Each binder used to inline this; collected here so language files
/// don't repeat the boilerplate.
pub(super) fn parse_with(lang: Language, content: &str) -> Result<Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.ts_language())
        .context("set language for scope binder")?;
    parser
        .parse(content, None)
        .context("tree-sitter parse failed in scope binder")
}
