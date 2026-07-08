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

use anyhow::Result;
use tree_sitter::{Node, Tree};

use super::{BindTarget, BoundRef, DefKind, LocalDef, RefKind, ScopeId, ScopeTree, UsePath};
use crate::index::symbols::ParsedSymbol;
use crate::parse::extractor::is_meaningful_identifier;
use crate::parse::language::Language;
use crate::parse::NodeTextExt;

/// Per-language tree dispatch — matches on node kind, recurses via
/// `Walker::walk_children`, and emits bindings + refs.
///
/// Stateless — per-binder mutable state must be stored on the
/// [`Walker`] directly, not captured. Function pointers can't close
/// over environments, which is the deliberate trade for avoiding
/// trait-object dispatch or per-binder generics.
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

    /// Thin forwarder onto the underlying `ScopeTree`. Keeps binders
    /// from reaching into `Walker::tree` directly.
    pub(super) fn push_scope(&mut self, kind: super::ScopeKind, parent: ScopeId) -> ScopeId {
        self.tree.push_scope(kind, parent)
    }

    pub(super) fn root_scope(&self) -> ScopeId {
        self.tree.root()
    }

    pub(super) fn add_binding(&mut self, scope: ScopeId, name_node: Node, kind: DefKind) {
        // `node_text` is bounds-safe (see `NodeTextExt`): a tree-sitter node
        // spanning a BOM-shifted / out-of-range byte range yields "" rather
        // than panicking the indexer, so we silently drop the binding.
        let name = name_node.node_text(self.content.as_bytes());
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
        // See add_binding — `node_text` is bounds-safe for the rare
        // BOM/out-of-range edge.
        let text = node.node_text(self.content.as_bytes());
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
                    // ModuleSymbol promotion is gated on the file root:
                    // a local def in a nested scope that shadows a
                    // module-level name wins lexically and stays Local.
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
/// don't repeat the boilerplate. v1.12.0 P3 — borrows a pooled per-thread
/// parser instead of constructing one per call.
pub(super) fn parse_with(lang: Language, content: &str) -> Result<Tree> {
    crate::parse::parser_pool::parse_text(lang, content)
}
