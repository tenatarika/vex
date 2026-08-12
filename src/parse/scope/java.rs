//! Java scope binder. Shares scaffolding with the other binders via
//! [`super::walker::Walker`].
//!
//! ## Resolution model
//!
//! Like C#, Java has no include graph; cross-file resolution piggybacks
//! on the writer's Pass-2 `name_to_global` single-candidate fallback. A
//! bare `helper()` call or a `pkg.Type` reference whose target lives in
//! another file resolves to [`super::BindTarget::Unresolved`] here and is
//! linked by Pass-2 when the name is unique corpus-wide.
//!
//! Java's lowercase-package convention does most of the noise-filtering
//! for free: `is_meaningful_identifier` drops pure-lowercase identifiers
//! without an underscore, so the `java`/`util` segments of a qualified
//! `java.util.List` (or an `import`) never reach the ref table — only the
//! capitalized tail (`List`) survives. This is why qualified names and
//! `import` paths can be walked generically without leaking package noise.
//!
//! ## What's handled
//!
//! - `method_declaration` / `constructor_declaration` — name bound in the
//!   parent scope; params (incl. varargs `T...`) + return type + body in a
//!   child `Function` scope. Modifiers/annotations are skipped (field-based
//!   walk only touches `name`/`parameters`/`type`/`body`).
//! - `class_declaration` / `interface_declaration` / `enum_declaration` /
//!   `record_declaration` / `annotation_type_declaration` — name bound in
//!   parent; `superclass` / `interfaces` / `permits` walked in the parent
//!   scope (their refs resolve there); `body` walked in a child `Class`
//!   scope; record components bound as params. `type_parameters` skipped.
//! - `variable_declarator` — value walked first, then the name bound as a
//!   local (covers both local vars and fields).
//! - `import_declaration` — `import a.b.C;` / `import static a.b.C.m;`
//!   bind the tail (`C` / `m`) to a `DefKind::Import` the writer resolves
//!   cross-file. Wildcard `import a.b.*;` binds nothing.
//! - `block` — child block scope.
//! - `package_declaration` / `annotation` / `marker_annotation` — skipped
//!   (declaration site / annotation labels, not ref-bearing).
//!
//! ## What's deferred / invisible
//!
//! - Unexported lowercase-without-underscore calls (`run()`, `parse()`)
//!   are dropped by `is_meaningful_identifier` before resolution —
//!   capitalized and snake_case names survive. See LIMITATIONS.
//! - Wildcard imports (`import a.b.*;`) — like C# `using a.b;`, the
//!   unqualified members stay `Unresolved` unless uniquely named.
//! - Generic type parameters (`<T extends Comparable<T>>`) — not bound;
//!   single-/two-letter names are filtered anyway.
//! - Lambda params, enhanced-`for` loop vars, `catch` params, and
//!   try-with-resources resource vars are best-effort (walked as refs,
//!   not bound) — harmless for idiomatic lowercase names; a capitalized
//!   one becomes a phantom `Unresolved` ref.
//! - Anonymous class bodies (`new Runnable() { … }`) ARE contained in a
//!   fresh `Class` scope so members don't leak outward, but their methods
//!   resolve only locally (not promoted to `ModuleSymbol`). Per-constant
//!   `enum_constant` bodies are walked in the enum's class scope rather
//!   than a dedicated child scope.

use tree_sitter::{Node, Tree};

use super::walker::Walker;
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind, UsePath};
use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;
use crate::parse::NodeTextExt;

pub struct JavaBinder;

impl ScopeBinder for JavaBinder {
    fn lang(&self) -> Language {
        Language::Java
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

    if is_java_comment_kind(kind) || is_java_string_kind(kind) {
        return;
    }

    match kind {
        // Declaration site, not a ref — skip so the package path doesn't
        // leak as phantom refs.
        "package_declaration" => {}
        // Annotation labels (`@Override`, `@Autowired`) — suppress so the
        // annotation type doesn't bloat the ref table.
        "annotation" | "marker_annotation" => {}
        "import_declaration" => walk_import(w, node, scope),
        "method_declaration" | "constructor_declaration" => walk_named_fn(w, node, scope),
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => walk_class_like(w, node, scope),
        "variable_declarator" => walk_var_declarator(w, node, scope),
        "object_creation_expression" => walk_object_creation(w, node, scope),
        "block" => {
            let s = w.push_scope(ScopeKind::Block, scope);
            w.walk_children(node, s);
        }
        "identifier" => w.emit_ref(node, scope, RefKind::Value),
        "type_identifier" => w.emit_ref(node, scope, RefKind::Type),
        // local_variable_declaration, expression_statement, method_invocation,
        // field_access, object_creation_expression, scoped_identifier,
        // scoped_type_identifier, … — recurse. Qualified-name package
        // segments are lowercase and filtered, so generic recursion is safe.
        _ => w.walk_children(node, scope),
    }
}

/// `[modifiers] ret Name(params) { body }` (method) or `Name(params) {
/// body }` (constructor). Name bound in the parent scope; params + return
/// type + body in a child `Function` scope. Field-based so annotations and
/// other modifiers are never walked.
fn walk_named_fn(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name) = node.child_by_field_name("name") {
        w.add_binding(parent, name, DefKind::Function);
    }
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    if let Some(params) = node.child_by_field_name("parameters") {
        bind_params(w, params, fn_scope);
    }
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, fn_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, fn_scope);
    }
}

/// Bind each parameter's name as a `Param` in `scope` and walk its type so
/// type refs are emitted. Handles fixed `formal_parameter` and variadic
/// `spread_parameter` (`T... name`, whose name lives in a nested
/// `variable_declarator`); `receiver_parameter` (`Foo this`) binds nothing.
/// Reused for record components.
fn bind_params(w: &mut Walker, list: Node, scope: ScopeId) {
    let mut cursor = list.walk();
    for child in list.children(&mut cursor) {
        match child.kind() {
            "formal_parameter" => {
                if let Some(ty) = child.child_by_field_name("type") {
                    w.walk(ty, scope);
                }
                bind_ident_name(w, child.child_by_field_name("name"), scope, DefKind::Param);
            }
            "spread_parameter" => {
                let mut inner = child.walk();
                for gc in child.children(&mut inner) {
                    match gc.kind() {
                        "variable_declarator" => {
                            bind_ident_name(
                                w,
                                gc.child_by_field_name("name"),
                                scope,
                                DefKind::Param,
                            );
                        }
                        "modifiers" | "annotation" | "marker_annotation" => {}
                        // the `_unannotated_type` — emit its type refs.
                        _ => w.walk(gc, scope),
                    }
                }
            }
            _ => {}
        }
    }
}

/// `Name <body>` for any class-like declaration. Name bound in the parent;
/// heritage walked in the parent scope (so `extends Base` resolves there);
/// body walked in a child `Class` scope. `type_parameters` are skipped
/// (generics deferred). Record components are bound as params.
fn walk_class_like(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name) = node.child_by_field_name("name") {
        w.add_binding(parent, name, DefKind::Type);
    }
    for field in ["superclass", "interfaces", "permits"] {
        if let Some(h) = node.child_by_field_name(field) {
            w.walk(h, parent);
        }
    }
    let class_scope = w.push_scope(ScopeKind::Class, parent);
    // record components: `record Point(int X, int Y)` — bind in class scope.
    if let Some(params) = node.child_by_field_name("parameters") {
        bind_params(w, params, class_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, class_scope);
    }
}

/// `new Type(args)` or `new Type(args) { body }`. Type + arguments resolve
/// in the current scope; an anonymous `class_body` is walked under a fresh
/// `Class` scope so its member declarations don't leak (and shadow) into
/// the enclosing scope — without this, a capitalized method/field inside
/// `new Runnable() { … }` would bind in the surrounding function scope.
fn walk_object_creation(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, scope);
    }
    if let Some(args) = node.child_by_field_name("arguments") {
        w.walk(args, scope);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "class_body" {
            let class_scope = w.push_scope(ScopeKind::Class, scope);
            w.walk(child, class_scope);
        }
    }
}

/// `Type name = value` declarator (local var or field). The value is walked
/// first (its refs resolve against the pre-declaration scope), then the
/// name is bound. The declaration's `type` is walked by the parent node's
/// generic recursion. Underscore patterns (`var _ = …`) bind nothing.
fn walk_var_declarator(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(value) = node.child_by_field_name("value") {
        w.walk(value, scope);
    }
    bind_ident_name(
        w,
        node.child_by_field_name("name"),
        scope,
        DefKind::Variable,
    );
}

/// `import [static] a.b.C[.*];`. Binds the local name so a later
/// `C.member` / bare `C` resolves cross-file via Pass-2. Wildcard imports
/// bind nothing (the unqualified members would need a side-channel, same
/// as C# `using a.b;`).
fn walk_import(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    let mut path_node: Option<Node> = None;
    let mut wildcard = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "asterisk" => wildcard = true,
            "scoped_identifier" | "identifier" => path_node = Some(child),
            _ => {}
        }
    }
    if wildcard {
        return;
    }
    let Some(path_node) = path_node else { return };
    let mut segments = Vec::new();
    collect_java_path(path_node, w.content, &mut segments);
    let bind_name = match segments.last() {
        Some(s) => s.clone(),
        None => return,
    };
    if bind_name.is_empty() {
        return;
    }
    w.add_import_binding(scope, bind_name, line, UsePath { segments });
}

/// Recursively flatten a `scoped_identifier` (or bare `identifier`) into
/// dotted segments: `a.b.C` → `[a, b, C]`. Mirrors C#'s `collect_cs_path`
/// but Java paths have no `::`, so a `scoped`/`name` field walk suffices.
fn collect_java_path(node: Node, content: &str, out: &mut Vec<String>) {
    match node.kind() {
        "scoped_identifier" => {
            if let Some(s) = node.child_by_field_name("scope") {
                collect_java_path(s, content, out);
            }
            if let Some(n) = node.child_by_field_name("name") {
                collect_java_path(n, content, out);
            }
        }
        "identifier" => {
            let text = node.node_text(content.as_bytes()).trim();
            if !text.is_empty() {
                out.push(text.to_string());
            }
        }
        _ => {}
    }
}

/// Bind `name_node` under `kind` if it is a plain `identifier`; skip
/// `underscore_pattern` (Java 21 `_`) and absent nodes.
fn bind_ident_name(w: &mut Walker, name_node: Option<Node>, scope: ScopeId, kind: DefKind) {
    if let Some(name) = name_node {
        if name.kind() == "identifier" {
            w.add_binding(scope, name, kind);
        }
    }
}

fn is_java_comment_kind(kind: &str) -> bool {
    matches!(kind, "line_comment" | "block_comment")
}

fn is_java_string_kind(kind: &str) -> bool {
    matches!(kind, "string_literal" | "character_literal" | "text_block")
}
