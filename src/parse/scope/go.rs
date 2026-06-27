//! Go scope binder. Shares scaffolding with the other binders via
//! [`super::walker::Walker`].
//!
//! ## Resolution model
//!
//! Go packages span multiple files in a directory; symbols are visible
//! across same-package files by bare name. This binder stays strictly
//! per-file (no sibling-file access — the cross-file join happens only
//! in the writer's Pass-2 `name_to_global` loop). A bare `Helper()`
//! call referencing a function defined in another file of the package
//! therefore resolves to [`BindTarget::Unresolved`] here and is linked
//! by Pass-2's single-candidate fallback (exactly the C# path).
//!
//! ## What's handled
//!
//! - `function_declaration` / `method_declaration` — name bound in the
//!   parent scope; receiver + params + body in a child `Function` scope.
//! - `type_spec` (struct / interface / alias) — type name bound in
//!   parent; the underlying type walked for its own type refs.
//! - `field_declaration` — only the field *type* is a ref; the field
//!   name is a definition, not emitted.
//! - `short_var_declaration` (`x := …`) — RHS walked first, then LHS
//!   names bound as locals.
//! - `selector_expression` (`pkg.Symbol`, `recv.Method`) — operand
//!   walked (resolves to Local for receivers/locals, Import for package
//!   aliases), the trailing `field` emitted as a by-name `Value` ref so
//!   cross-package / method calls resolve via Pass-2.
//! - `import_spec` — `import "math/rand"` binds `rand` (last `/`
//!   segment); `import mr "math/rand"` binds `mr`; both `DefKind::Import`.
//!   Dot (`. "strings"`) and blank (`_ "embed"`) imports bind nothing.
//!
//! ## What's deferred / invisible
//!
//! - Unexported lowercase-without-underscore calls (`spin()`, `parse()`)
//!   are dropped by `is_meaningful_identifier` before resolution —
//!   exported (`Spin`) and snake_case names survive. See LIMITATIONS.
//! - `var` / `const` declaration names, `range` clause vars, type-switch
//!   bindings are best-effort (walked as refs, not bound) for now.
//! - Generic type parameters (`func F[K comparable]()`, `type Set[T any]`)
//!   are not bound; the `type_parameters` field is not walked. Single- and
//!   two-letter names (`T`, `K`) are filtered by `is_meaningful_identifier`
//!   anyway, so phantom refs only arise for a 3+ char mixed-case constraint
//!   name used in the body — rare.
//! - Named return values (`func F() (Out int)`) are walked for their type
//!   refs but not bound as locals; like the lowercase-call gap this is
//!   harmless for idiomatic lowercase names. Input params and receivers
//!   ARE bound.

use anyhow::Result;
use tree_sitter::Node;

use super::walker::{parse_with, Walker};
use super::{BoundRef, DefKind, RefKind, ScopeBinder, ScopeId, ScopeKind, UsePath};
use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;

pub struct GoBinder;

impl ScopeBinder for GoBinder {
    fn bind(&self, content: &str, file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        let tree = parse_with(Language::Go, content)?;
        Ok(Walker::new(content, file_symbols, dispatch).run(&tree))
    }
}

fn dispatch(w: &mut Walker, node: Node, scope: ScopeId) {
    let kind = node.kind();

    if is_go_comment_kind(kind) || is_go_plain_string_kind(kind) {
        return;
    }

    match kind {
        // The package clause is a declaration site, not a ref — skip it
        // so `package_identifier` doesn't leak as a phantom ref.
        "package_clause" => {}
        "import_spec" => walk_import_spec(w, node, scope),
        "function_declaration" => walk_fn(w, node, scope),
        "method_declaration" => walk_method(w, node, scope),
        "type_spec" => walk_type_spec(w, node, scope),
        "field_declaration" => walk_field_decl(w, node, scope),
        "selector_expression" => walk_selector(w, node, scope),
        "short_var_declaration" => walk_short_var(w, node, scope),
        "block" => {
            let s = w.push_scope(ScopeKind::Block, scope);
            w.walk_children(node, s);
        }
        "identifier" => w.emit_ref(node, scope, RefKind::Value),
        "type_identifier" => w.emit_ref(node, scope, RefKind::Type),
        // import_declaration, statement_list, expression_list,
        // call_expression, composite_literal, etc. — recurse.
        _ => w.walk_children(node, scope),
    }
}

/// `func Name(params) ret { body }`. Name bound in the parent scope;
/// params + body live in a child `Function` scope.
fn walk_fn(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name) = node.child_by_field_name("name") {
        w.add_binding(parent, name, DefKind::Function);
    }
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    bind_param_list(w, node, "parameters", fn_scope);
    if let Some(result) = node.child_by_field_name("result") {
        w.walk(result, fn_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, fn_scope);
    }
}

/// `func (recv T) Name(params) ret { body }`. The method name (a
/// `field_identifier`) is bound in the parent so cross-file `x.Name()`
/// calls resolve to it by name; the receiver var is bound in the fn
/// scope so `recv.Field` resolves the receiver to a Local.
fn walk_method(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name) = node.child_by_field_name("name") {
        w.add_binding(parent, name, DefKind::Function);
    }
    let fn_scope = w.push_scope(ScopeKind::Function, parent);
    bind_param_list(w, node, "receiver", fn_scope);
    bind_param_list(w, node, "parameters", fn_scope);
    if let Some(result) = node.child_by_field_name("result") {
        w.walk(result, fn_scope);
    }
    if let Some(body) = node.child_by_field_name("body") {
        w.walk(body, fn_scope);
    }
}

/// Bind each parameter's `name` field as a `Param` in `scope`, and walk
/// its `type` so type refs are emitted. Shared by the receiver list and
/// the parameter list. Both fixed (`parameter_declaration`) and variadic
/// (`variadic_parameter_declaration`, the `elems ...T` form) carry the
/// same `name`/`type` fields — without the variadic arm the param name
/// falls through to the `identifier` dispatch and leaks as a phantom ref.
fn bind_param_list(w: &mut Walker, fn_node: Node, field: &str, scope: ScopeId) {
    let Some(list) = fn_node.child_by_field_name(field) else {
        return;
    };
    let mut cursor = list.walk();
    for child in list.children(&mut cursor) {
        if matches!(
            child.kind(),
            "parameter_declaration" | "variadic_parameter_declaration"
        ) {
            if let Some(name) = child.child_by_field_name("name") {
                w.add_binding(scope, name, DefKind::Param);
            }
            if let Some(ty) = child.child_by_field_name("type") {
                w.walk(ty, scope);
            }
        }
    }
}

/// `type Name <struct|interface|…>`. Bind the type name, then walk the
/// underlying type so its field/element types emit refs.
fn walk_type_spec(w: &mut Walker, node: Node, parent: ScopeId) {
    if let Some(name) = node.child_by_field_name("name") {
        w.add_binding(parent, name, DefKind::Type);
    }
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, parent);
    }
}

/// A struct `field_declaration` — only the field `type` is a ref; the
/// field name is a definition we don't emit (it isn't a top-level
/// symbol and would pollute the ref table).
fn walk_field_decl(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(ty) = node.child_by_field_name("type") {
        w.walk(ty, scope);
    }
}

/// `operand.field` — walk the operand (resolves Local / Import / Module
/// / Unresolved) and emit the trailing `field` as a by-name `Value` ref.
/// For `pkg.Symbol` the operand is an import binding and `Symbol`
/// resolves cross-package via Pass-2; for `recv.Method` the operand is a
/// Local and `Method` resolves to the method symbol by name.
fn walk_selector(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(operand) = node.child_by_field_name("operand") {
        w.walk(operand, scope);
    }
    if let Some(field) = node.child_by_field_name("field") {
        w.emit_ref(field, scope, RefKind::Value);
    }
}

/// `lhs := rhs` — the RHS is evaluated first (its refs resolve against
/// the pre-declaration scope), then the LHS identifiers become locals.
fn walk_short_var(w: &mut Walker, node: Node, scope: ScopeId) {
    if let Some(right) = node.child_by_field_name("right") {
        w.walk(right, scope);
    }
    if let Some(left) = node.child_by_field_name("left") {
        let mut cursor = left.walk();
        for child in left.children(&mut cursor) {
            if child.kind() == "identifier" {
                w.add_binding(scope, child, DefKind::Variable);
            }
        }
    }
}

/// `import [alias] "path"`. Binds the local package name so a later
/// `pkg.Symbol` selector resolves the operand to an import. Plain
/// imports bind the last `/`-segment of the path; aliased imports bind
/// the alias; dot and blank imports bind nothing.
fn walk_import_spec(w: &mut Walker, node: Node, scope: ScopeId) {
    let line = node.start_position().row + 1;
    let Some(path_node) = node.child_by_field_name("path") else {
        return;
    };
    let path_text = path_node
        .utf8_text(w.content.as_bytes())
        .unwrap_or("")
        .trim_matches('"')
        .trim();
    if path_text.is_empty() {
        return;
    }
    // Go import paths are always `/`-separated (no `::` or other
    // metacharacter), so a plain text split is safe here — unlike C#,
    // which needs AST recursion to walk `::`-qualified names.
    let segments: Vec<String> = path_text.split('/').map(|s| s.to_string()).collect();

    // The `name:` field is present for `import alias "path"`. It is a
    // `package_identifier` for an alias, or a `dot` / `blank_identifier`
    // node for `.`/`_` imports (which bind nothing).
    let bind_name = match node.child_by_field_name("name") {
        Some(n) if n.kind() == "package_identifier" => n
            .utf8_text(w.content.as_bytes())
            .unwrap_or("")
            .trim()
            .to_string(),
        Some(_) => return, // dot or blank import — no binding
        // `path_text` is non-empty (guarded above), so `split` always
        // yields a last segment; `next_back` avoids the unreachable panic.
        None => path_text.rsplit('/').next().unwrap_or("").to_string(),
    };
    if bind_name.is_empty() {
        return;
    }
    w.add_import_binding(scope, bind_name, line, UsePath { segments });
}

fn is_go_comment_kind(kind: &str) -> bool {
    kind == "comment"
}

fn is_go_plain_string_kind(kind: &str) -> bool {
    matches!(
        kind,
        "interpreted_string_literal" | "raw_string_literal" | "rune_literal"
    )
}
