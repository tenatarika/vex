//! Scope binder skeleton (11.1.2a).
//!
//! The scope binder is the second layer of the 11.1 type-aware
//! refactor-grade `usages` work. Phase 11.1.1 deleted the loudest noise
//! (refs found inside comments and string literals) via an AST walk.
//! Phase 11.1.2 starts wiring the LSP-style resolver that maps each
//! textual reference to a defining symbol id.
//!
//! This file ships only the data structures and an inert dispatch —
//! per-language binders (Rust first) plug in via [`ScopeBinder`].
//! Until 11.1.2b lands the stub binders return an empty `bound_refs`
//! vector and `vex usages` keeps serving from the existing refs FST.
//!
//! Many fields/variants below (`BoundRef` field reads, `RefKind::Call`,
//! `RefKind::Macro`, `BindTarget` arms, `UsePath::segments` outside of
//! tests) stay unread until 11.1.3 wires them to the persistent
//! `reference_edges` section and 11.1.6 wires them to
//! `vex implementations`. We tolerate dead-code warnings at the module
//! level rather than scattering `#[allow]` over every variant; revisit
//! and remove this when 11.1.3 lands.
#![allow(dead_code)]

use anyhow::Result;

use super::language::Language;
use crate::index::symbols::ParsedSymbol;

pub mod rust;
pub mod typescript;

/// Arena index into [`ScopeTree::scopes`]. `0` is always the file root.
pub type ScopeId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    /// Top-level scope for the file's module.
    File,
    /// `mod foo { ... }` (Rust), namespace/module in other langs.
    Module,
    /// Function body — `fn`, `def`, `function`, lambda.
    Function,
    /// Free `{ ... }` block.
    Block,
    /// `impl Foo { ... }` (Rust). Method scope is a child Function scope.
    Impl,
    /// `class Foo: ... ` / `class Foo { ... }` body.
    Class,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    /// `let x = ...` / `var x = ...` / Python local assignment.
    Variable,
    /// Function parameter.
    Param,
    /// Type definition — struct, enum, trait, interface, class.
    Type,
    /// Function / method definition.
    Function,
    /// Submodule definition.
    Module,
    /// Name brought in via `use` / `import` / `from ... import ...`.
    Import,
}

#[derive(Debug, Clone)]
pub struct LocalDef {
    pub line: usize,
    pub kind: DefKind,
    /// Populated only when `kind == DefKind::Import` — captures the
    /// dotted path the `use` statement pulls the name from so the
    /// resolver can return `BindTarget::Imported`. Cross-file
    /// resolution from path to global symbol idx is 11.1.3.
    pub import_path: Option<UsePath>,
}

#[derive(Debug, Clone)]
struct Scope {
    kind: ScopeKind,
    parent: Option<ScopeId>,
    bindings: std::collections::HashMap<String, LocalDef>,
}

/// Arena-backed scope tree built during parsing. Languages walk their
/// AST and call [`ScopeTree::push_scope`] / [`ScopeTree::add_binding`];
/// reference resolution then calls [`ScopeTree::resolve`] which walks
/// the parent chain until it finds the nearest binding for a name.
#[derive(Debug, Clone, Default)]
pub struct ScopeTree {
    scopes: Vec<Scope>,
}

impl ScopeTree {
    /// Construct a tree with a single `File` root scope at id `0`.
    pub fn new() -> Self {
        Self {
            scopes: vec![Scope {
                kind: ScopeKind::File,
                parent: None,
                bindings: std::collections::HashMap::new(),
            }],
        }
    }

    pub fn root(&self) -> ScopeId {
        0
    }

    pub fn len(&self) -> usize {
        self.scopes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    pub fn kind_of(&self, scope: ScopeId) -> Option<ScopeKind> {
        self.scopes.get(scope as usize).map(|s| s.kind)
    }

    pub fn parent_of(&self, scope: ScopeId) -> Option<ScopeId> {
        self.scopes.get(scope as usize).and_then(|s| s.parent)
    }

    /// Append a new child scope under `parent` and return its id.
    pub fn push_scope(&mut self, kind: ScopeKind, parent: ScopeId) -> ScopeId {
        let id = self.scopes.len() as ScopeId;
        self.scopes.push(Scope {
            kind,
            parent: Some(parent),
            bindings: std::collections::HashMap::new(),
        });
        id
    }

    /// Record that `name` is bound at this scope. Re-binding the same
    /// name in the same scope shadows the previous entry — this matches
    /// `let x = 1; let x = 2;` Rust semantics where the second binding
    /// wins for subsequent lookups in the same scope.
    pub fn add_binding(&mut self, scope: ScopeId, name: impl Into<String>, def: LocalDef) {
        if let Some(s) = self.scopes.get_mut(scope as usize) {
            s.bindings.insert(name.into(), def);
        }
    }

    /// Walk from `scope` toward the root, returning the first scope
    /// where `name` is bound and the binding metadata.
    pub fn resolve(&self, scope: ScopeId, name: &str) -> Option<(ScopeId, &LocalDef)> {
        let mut cur = Some(scope);
        while let Some(id) = cur {
            let s = self.scopes.get(id as usize)?;
            if let Some(def) = s.bindings.get(name) {
                return Some((id, def));
            }
            cur = s.parent;
        }
        None
    }
}

/// A dotted import path. Cross-file resolution (`UsePath` → global
/// symbol idx) lands in 11.1.3 alongside the persistent
/// `reference_edges` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsePath {
    pub segments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindTarget {
    /// Resolved to a local definition at the given scope.
    Local(ScopeId),
    /// Resolved to a symbol declared in the same file.
    ModuleSymbol(u32),
    /// Resolved to an imported name; cross-file resolution is 11.1.3.
    Imported(UsePath),
    /// Could not be resolved with the current binder.
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Type,
    Value,
    Call,
    Macro,
}

#[derive(Debug, Clone)]
pub struct BoundRef {
    pub name: String,
    pub line: usize,
    pub col: usize,
    pub target: BindTarget,
    pub kind: RefKind,
}

pub trait ScopeBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>>;
}

/// Dispatch entry point used by `parse_file`. For languages without a
/// binder we return `Ok(vec![])`, keeping the existing refs FST as the
/// authoritative source for `vex usages` (no behavioural change).
pub fn bind_refs(
    content: &str,
    lang: Language,
    file_symbols: &[ParsedSymbol],
) -> Result<Vec<BoundRef>> {
    match lang {
        Language::Rust => rust::RustBinder.bind(content, file_symbols),
        Language::TypeScript => typescript::TypeScriptBinder.bind(content, file_symbols),
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(line: usize, kind: DefKind) -> LocalDef {
        LocalDef {
            line,
            kind,
            import_path: None,
        }
    }

    #[test]
    fn arena_starts_with_a_single_file_root() {
        let t = ScopeTree::new();
        assert_eq!(t.len(), 1);
        assert_eq!(t.root(), 0);
        assert_eq!(t.kind_of(0), Some(ScopeKind::File));
        assert_eq!(t.parent_of(0), None);
    }

    #[test]
    fn push_scope_returns_increasing_ids_and_records_parent() {
        let mut t = ScopeTree::new();
        let m = t.push_scope(ScopeKind::Module, t.root());
        let f = t.push_scope(ScopeKind::Function, m);
        let b = t.push_scope(ScopeKind::Block, f);
        assert_eq!(m, 1);
        assert_eq!(f, 2);
        assert_eq!(b, 3);
        assert_eq!(t.parent_of(b), Some(f));
        assert_eq!(t.parent_of(f), Some(m));
        assert_eq!(t.parent_of(m), Some(t.root()));
    }

    #[test]
    fn resolve_finds_binding_in_same_scope() {
        let mut t = ScopeTree::new();
        let f = t.push_scope(ScopeKind::Function, t.root());
        t.add_binding(f, "answer", def(10, DefKind::Variable));
        let (sid, d) = t.resolve(f, "answer").expect("answer in scope");
        assert_eq!(sid, f);
        assert_eq!(d.line, 10);
        assert_eq!(d.kind, DefKind::Variable);
    }

    #[test]
    fn resolve_walks_to_parent_scope() {
        let mut t = ScopeTree::new();
        t.add_binding(t.root(), "TopType", def(1, DefKind::Type));
        let f = t.push_scope(ScopeKind::Function, t.root());
        let b = t.push_scope(ScopeKind::Block, f);
        let (sid, _) = t.resolve(b, "TopType").expect("walks to file root");
        assert_eq!(sid, t.root());
    }

    #[test]
    fn resolve_returns_nearest_shadowing_definition() {
        let mut t = ScopeTree::new();
        t.add_binding(t.root(), "x", def(1, DefKind::Variable));
        let f = t.push_scope(ScopeKind::Function, t.root());
        t.add_binding(f, "x", def(20, DefKind::Param));
        let (sid, d) = t.resolve(f, "x").expect("inner shadow");
        assert_eq!(sid, f);
        assert_eq!(d.kind, DefKind::Param);
        assert_eq!(d.line, 20);
    }

    #[test]
    fn resolve_returns_none_when_unbound() {
        let mut t = ScopeTree::new();
        let f = t.push_scope(ScopeKind::Function, t.root());
        assert!(t.resolve(f, "ghost").is_none());
    }

    #[test]
    fn sibling_scopes_do_not_leak_bindings() {
        let mut t = ScopeTree::new();
        let f1 = t.push_scope(ScopeKind::Function, t.root());
        let f2 = t.push_scope(ScopeKind::Function, t.root());
        t.add_binding(f1, "only_in_f1", def(5, DefKind::Variable));
        assert!(t.resolve(f1, "only_in_f1").is_some());
        assert!(t.resolve(f2, "only_in_f1").is_none());
    }

    #[test]
    fn rebinding_same_name_in_same_scope_overwrites() {
        let mut t = ScopeTree::new();
        let f = t.push_scope(ScopeKind::Function, t.root());
        t.add_binding(f, "x", def(1, DefKind::Variable));
        t.add_binding(f, "x", def(2, DefKind::Variable));
        let (_, d) = t.resolve(f, "x").expect("rebinding");
        assert_eq!(d.line, 2, "later let must win for subsequent lookups");
    }
}
