//! Kotlin scope binder. Shares scaffolding with the other binders via
//! [`super::walker::Walker`].
//!
//! ## Resolution model
//!
//! Like Go / Java / C#, Kotlin has no include graph; cross-file resolution
//! piggybacks on the writer's Pass-2 `name_to_global` single-candidate
//! fallback. A bare `helperFn()` call or a `Helper.doWork()` member access
//! whose target lives in another file resolves to
//! [`super::BindTarget::Unresolved`] here and is linked by Pass-2 when the
//! name is unique corpus-wide.
//!
//! Kotlin's lowercase-package / lowercase-member convention does the
//! noise-filtering for free: `is_meaningful_identifier` drops
//! pure-lowercase identifiers without an underscore, so the `com`/`example`
//! segments of a qualified `com.example.Widget` (or an `import`) and
//! lowercase receivers (`order` in `order.amount`) never reach the ref
//! table — only Capitalized / camelCase tails survive.
//!
//! ## What's handled
//!
//! - `function_declaration` / `secondary_constructor` — name (when present)
//!   bound in the parent scope; params (incl. `vararg`) + return type + body
//!   in a child `Function` scope. `modifiers` / `type_parameters` skipped so
//!   annotation labels never leak.
//! - `class_declaration` / `object_declaration` — name bound in parent;
//!   `delegation_specifiers` (`: Base(), Iface`) walked in the parent scope
//!   so the supertypes resolve there; `primary_constructor` params + the
//!   class/enum body in a child `Class` scope.
//! - `companion_object` — body walked in a child `Class` scope (it has no
//!   name binding of its own).
//! - `enum_entry` — bound as a constant in the enclosing class scope (so the
//!   `RED` / `GREEN` constants aren't emitted as phantom Unresolved refs);
//!   any constructor arguments are walked.
//! - `property_declaration` — the `variable_declaration` name bound as a
//!   local/field; its type + initializer walked.
//! - `import` — `import a.b.C` binds the tail (`C`) to a `DefKind::Import`
//!   the writer resolves cross-file. Wildcard `import a.b.*` binds nothing.
//! - `user_type` — the type identifier(s) emitted as `RefKind::Type`.
//! - `block` / `lambda_literal` — child scopes; lambda params are bound.
//!
//! ## What's deferred / invisible
//!
//! - Pure-lowercase-without-underscore calls (`run()`, `parse()`) are
//!   dropped by `is_meaningful_identifier` before resolution — Capitalized
//!   and camelCase names survive. See LIMITATIONS.
//! - Wildcard imports (`import a.b.*`) — members stay `Unresolved` unless
//!   uniquely named corpus-wide (same as Java `import a.b.*` / C# `using`).
//! - Generic type parameters (`<T : Comparable<T>>`) are not bound;
//!   single-/two-letter names are filtered anyway.
//! - Extension-function receivers, destructuring declarations, and `when`
//!   subject bindings are best-effort (walked as refs, not bound) — harmless
//!   for idiomatic lowercase names.
//! - Annotation labels (`@Marker`) are suppressed (the `modifiers` /
//!   `parameter_modifiers` subtrees are skipped), mirroring the Java binder.

use anyhow::Result;
use tree_sitter::Node;

use super::walker::{parse_with, Walker};
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind, UsePath};
use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;
use crate::parse::NodeTextExt;

pub struct KotlinBinder;

impl ScopeBinder for KotlinBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let tree = parse_with(Language::Kotlin, content)?;
        Ok(Walker::new(content, file_symbols, dispatch).run(&tree))
    }
}

fn dispatch(w: &mut Walker, node: Node, scope: ScopeId) {
    let kind = node.kind();

    if is_kotlin_comment_kind(kind) || is_kotlin_string_kind(kind) {
        return;
    }

    match kind {
        // Declaration site / keyword-and-annotation containers — skip so
        // package segments and annotation labels don't leak as refs.
        "package_header" | "modifiers" | "parameter_modifiers" | "type_parameters" => {}
        "import" => walk_import(w, node, scope),
        "function_declaration" => walk_named_fn(w, node, scope),
        "secondary_constructor" => walk_anon_fn(w, node, scope),
        "class_declaration" | "object_declaration" => walk_class_like(w, node, scope),
        "companion_object" => walk_companion(w, node, scope),
        "enum_entry" => walk_enum_entry(w, node, scope),
        "property_declaration" => walk_property(w, node, scope),
        "lambda_literal" => walk_lambda(w, node, scope),
        "user_type" => walk_user_type(w, node, scope),
        // Kotlin strings are interpolatable: descend only into `${…}`
        // `interpolation` children so real refs survive while the literal
        // text around them is dropped. The bare `$name` short form is plain
        // `string_content` text and is intentionally not walked.
        "string_literal" | "multiline_string_literal" => walk_string(w, node, scope),
        "block" => {
            let s = w.push_scope(ScopeKind::Block, scope);
            w.walk_children(node, s);
        }
        "identifier" => w.emit_ref(node, scope, RefKind::Value),
        // call_expression, navigation_expression, value_arguments,
        // constructor_invocation, … — recurse. Both the receiver and the
        // member of a `Helper.doWork()` navigation are emitted as refs; the
        // lowercase ones are dropped by is_meaningful_identifier.
        _ => w.walk_children(node, scope),
    }
}

/// `fun Name(params): Ret { body }`. Name bound in the parent scope; the
/// params, return type, and body go in a child `Function` scope. The `name`
/// field child is skipped during the generic body walk so the definition
/// isn't emitted as a ref.
fn walk_named_fn(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name) = node.child_by_field_name("name") {
        w.add_binding(parent, name, DefKind::Function);
    }
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    walk_fn_children(w, node, fn_scope);
}

/// `constructor(params) { body }` — a function-shaped node with no name.
fn walk_anon_fn(w: &mut Walker, node: Node, parent: ScopeId) {
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    walk_fn_children(w, node, fn_scope);
}

/// Shared body walk for `function_declaration` / `secondary_constructor`:
/// bind params, walk the return type + body in `fn_scope`, and skip the
/// `name` field child + modifier/keyword containers.
fn walk_fn_children(w: &mut Walker, node: Node, fn_scope: ScopeId) {
    let name_id = node.child_by_field_name("name").map(|n| n.id());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) == name_id {
            continue;
        }
        match child.kind() {
            "modifiers" | "type_parameters" | "type_constraints" => {}
            "function_value_parameters" => bind_params(w, child, fn_scope),
            // user_type (return), function_body, block, = expression … —
            // walk so their refs resolve in the function scope.
            _ => w.walk(child, fn_scope),
        }
    }
}

/// Bind each `parameter` / `class_parameter` name as a `Param` in `scope`
/// and walk its type so type refs are emitted. The name is the direct
/// `identifier` child; the type lives in a nested `user_type`. `vararg`
/// surfaces as a sibling `parameter_modifiers` node (skipped), so variadic
/// params bind like any other.
fn bind_params(w: &mut Walker, list: Node, scope: ScopeId) {
    let mut cursor = list.walk();
    for child in list.children(&mut cursor) {
        if !matches!(child.kind(), "parameter" | "class_parameter") {
            continue;
        }
        let mut inner = child.walk();
        for gc in child.children(&mut inner) {
            match gc.kind() {
                "modifiers" | "parameter_modifiers" => {}
                // The direct identifier child is the parameter name.
                "identifier" => w.add_binding(scope, gc, DefKind::Param),
                // user_type / nullable_type / function_type … — emit type refs.
                _ => w.walk(gc, scope),
            }
        }
    }
}

/// `class Name(...) : Base(), Iface { body }` (also `object Name { body }`).
/// Name bound in the parent; the primary constructor's params + the
/// class/enum body walked in a child `Class` scope; `delegation_specifiers`
/// (the supertype list) walked in the *parent* scope so `Base` / `Iface`
/// resolve there. `modifiers` / `type_parameters` skipped.
fn walk_class_like(w: &mut Walker, node: Node, parent: ScopeId) {
    let name_id = node.child_by_field_name("name").map(|name| {
        w.add_binding(parent, name, DefKind::Type);
        name.id()
    });
    let class_scope = w.push_scope(ScopeKind::Class, parent);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) == name_id {
            continue;
        }
        match child.kind() {
            "modifiers" | "type_parameters" | "type_constraints" => {}
            // Supertypes resolve in the parent scope (not the class body).
            "delegation_specifiers" => w.walk(child, parent),
            "primary_constructor" => {
                let mut inner = child.walk();
                for gc in child.children(&mut inner) {
                    match gc.kind() {
                        "class_parameters" => bind_params(w, gc, class_scope),
                        "modifiers" => {}
                        _ => w.walk(gc, class_scope),
                    }
                }
            }
            // class_body / enum_class_body and anything else → class scope.
            _ => w.walk(child, class_scope),
        }
    }
}

/// `companion object [Name] { body }` — body walked in a fresh `Class`
/// scope. A named companion's own identifier is not emitted as a ref.
fn walk_companion(w: &mut Walker, node: Node, parent: ScopeId) {
    let class_scope = w.push_scope(ScopeKind::Class, parent);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_body" | "enum_class_body" => w.walk(child, class_scope),
            _ => {}
        }
    }
}

/// `RED` / `GREEN(arg)` enum constant. The constant name is bound (so it's
/// not a phantom ref); constructor arguments are walked in `scope`.
fn walk_enum_entry(w: &mut Walker, node: Node, scope: ScopeId) {
    let name_id = node
        .children(&mut node.walk())
        .find(|c| c.kind() == "identifier")
        .map(|c| {
            w.add_binding(scope, c, DefKind::Variable);
            c.id()
        });
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) == name_id {
            continue;
        }
        // Skip annotation labels (`@Deprecated Red`) so they don't leak.
        if child.kind() == "modifiers" {
            continue;
        }
        w.walk(child, scope);
    }
}

/// `val/var Name[: Type] [= init]` (top-level, class-level, or local). The
/// name is bound as a local; its type + initializer are walked.
fn walk_property(w: &mut Walker, node: Node, scope: ScopeId) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "modifiers" => {}
            "variable_declaration" | "multi_variable_declaration" => {
                let mut inner = child.walk();
                for gc in child.children(&mut inner) {
                    match gc.kind() {
                        "identifier" => w.add_binding(scope, gc, DefKind::Variable),
                        _ => w.walk(gc, scope),
                    }
                }
            }
            // initializer expression / delegate.
            _ => w.walk(child, scope),
        }
    }
}

/// `{ x, y -> body }`. Lambda params are bound in a child `Function` scope so
/// they don't leak; the body is walked there.
fn walk_lambda(w: &mut Walker, node: Node, parent: ScopeId) {
    let lambda_scope = w.push_scope(ScopeKind::Function, parent);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "lambda_parameters" => {
                let mut inner = child.walk();
                for gc in child.children(&mut inner) {
                    if gc.kind() == "variable_declaration" {
                        let mut vc = gc.walk();
                        for v in gc.children(&mut vc) {
                            match v.kind() {
                                "identifier" => w.add_binding(lambda_scope, v, DefKind::Param),
                                _ => w.walk(v, lambda_scope),
                            }
                        }
                    }
                }
            }
            _ => w.walk(child, lambda_scope),
        }
    }
}

/// `Type` / `Type<Arg>` / `pkg.Type`. Emit the type identifier(s) as
/// `RefKind::Type`; recurse into type arguments and qualified segments
/// (lowercase package segments are filtered by `is_meaningful_identifier`).
fn walk_user_type(w: &mut Walker, node: Node, scope: ScopeId) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" => w.emit_ref(child, scope, RefKind::Type),
            _ => w.walk(child, scope),
        }
    }
}

/// A `string_literal` / `multiline_string_literal` — descend only into
/// `${…}` `interpolation` children so real code refs survive; the
/// surrounding `string_content` text (incl. the bare `$name` short form) is
/// dropped, matching the non-strict refs FST filter.
fn walk_string(w: &mut Walker, node: Node, scope: ScopeId) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "interpolation" {
            w.walk(child, scope);
        }
    }
}

/// `import a.b.C` / `import a.b.C as D`. Binds the tail (`C`) so a later bare
/// `C` or `C.member` resolves cross-file via Pass-2. Wildcard `import a.b.*`
/// binds nothing (the `*` is an anonymous token, so detect it from the raw
/// text). An `as` alias binds the alias name.
fn walk_import(w: &mut Walker, node: Node, scope: ScopeId) {
    let raw = node.node_text(w.content.as_bytes());
    if raw.trim_end().ends_with('*') {
        return;
    }
    let line = node.start_position().row + 1;

    // An `import a.b.C as D` exposes the alias `D`; otherwise the tail of the
    // qualified path is the bound name. tree-sitter-kotlin-ng has NO
    // `import_alias` wrapper: `as` is an anonymous token and the alias is a
    // bare `identifier` child appearing after the `qualified_identifier`.
    let mut alias: Option<Node> = None;
    let mut path_node: Option<Node> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "qualified_identifier" => path_node = Some(child),
            // Only an alias identifier follows the path node; a single-segment
            // import (no path) never reaches here as a bare identifier.
            "identifier" | "type_identifier" if path_node.is_some() => alias = Some(child),
            _ => {}
        }
    }

    if let Some(alias) = alias {
        let name = alias.node_text(w.content.as_bytes()).trim();
        if !name.is_empty() {
            // The alias path still points at the original qualified target.
            let segments = path_node
                .map(|p| collect_kotlin_path(p, w.content))
                .unwrap_or_default();
            w.add_import_binding(scope, name.to_string(), line, UsePath { segments });
        }
        return;
    }

    let Some(path_node) = path_node else { return };
    let segments = collect_kotlin_path(path_node, w.content);
    let Some(bind_name) = segments.last().cloned() else {
        return;
    };
    if bind_name.is_empty() {
        return;
    }
    w.add_import_binding(scope, bind_name, line, UsePath { segments });
}

/// Flatten a `qualified_identifier` into dotted segments: `a.b.C` →
/// `[a, b, C]`.
fn collect_kotlin_path(node: Node, content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            let text = child.node_text(content.as_bytes()).trim();
            if !text.is_empty() {
                out.push(text.to_string());
            }
        }
    }
    out
}

fn is_kotlin_comment_kind(kind: &str) -> bool {
    matches!(kind, "line_comment" | "block_comment")
}

/// Only the char literal is dropped wholesale. `string_literal` /
/// `multiline_string_literal` are interpolatable and handled by
/// [`walk_string`] so `${…}` refs survive.
fn is_kotlin_string_kind(kind: &str) -> bool {
    kind == "character_literal"
}
