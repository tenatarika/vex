//! Per-language identifier extraction for pattern-targetable AST nodes.
//!
//! `extract_ident` is the single entry-point invoked by the skeleton
//! walker. The anonymous-kind match short-circuits to `None`; the
//! per-language dispatch handles CSS/HTML/SQL/Markdown positional names
//! and the Cpp/Kotlin/PHP override fields; everything else falls through
//! to the generic `name:` / `type:` named-field lookup.
//!
//! Isolated from the walker because ident extraction diverges enough
//! per-language (positional vs named-field children, multi-level lookups,
//! sigil-stripping) that co-locating it obscured the walker's small
//! kernel. Adding language quirks here keeps `super::walk` minimal.

use tree_sitter::Node;

use crate::parse::language::Language;
use crate::parse::NodeTextExt;

/// Return the leaf identifier text for declaration-shaped nodes. The
/// field name varies by language and kind — anonymous nodes (lambdas,
/// arrow functions, decorated wrappers) return `None`.
pub(super) fn extract_ident(
    node: Node<'_>,
    source: &str,
    lang: Language,
    kind: &str,
) -> Option<String> {
    // Anonymous kinds — no recoverable identifier.
    let anonymous = matches!(
        (lang, kind),
        (
            Language::TypeScript,
            "arrow_function" | "function_expression",
        ) | (Language::Python, "lambda" | "decorated_definition")
            | (Language::Go, "func_literal")
            | (Language::Cpp, "template_declaration" | "lambda_expression",)
            | (
                Language::CSharp,
                "lambda_expression" | "anonymous_method_expression",
            )
            | (Language::Java, "lambda_expression")
            | (
                Language::Kotlin,
                // `secondary_constructor` has no recoverable name —
                // the `constructor` keyword is its identity. The
                // other three are textbook anonymous callables /
                // initializers.
                "lambda_literal"
                    | "anonymous_function"
                    | "anonymous_initializer"
                    | "secondary_constructor",
            )
            | (
                Language::Swift,
                // Swift identifies these by keyword (`init`,
                // `deinit`, `subscript`) — no `name:` field. The
                // `operator_declaration` wrapper's identity is the
                // `custom_operator` child (not exposed as a name).
                // `lambda_literal` is the textbook anonymous closure.
                "init_declaration"
                    | "deinit_declaration"
                    | "subscript_declaration"
                    | "operator_declaration"
                    | "lambda_literal",
            )
            | (
                Language::Php,
                // PHP anonymous callables (`function() use (...) { ... }`
                // and `fn($x) => $x + 1`) plus the anonymous
                // class form `new class { ... }`. None expose a
                // `name:` field — all three short-circuit here.
                "anonymous_function" | "arrow_function" | "anonymous_class",
            )
            | (
                Language::Ruby,
                // Ruby anonymous callables: lambda literals
                // (`->{}`), brace blocks (`{ |x| ... }`), keyword
                // blocks (`do |x| ... end`), and the
                // `class << self` singleton-class form (which has
                // a `value:` field for the receiver but no `name:`).
                "lambda" | "block" | "do_block" | "singleton_class",
            )
    );
    if anonymous {
        return None;
    }
    // CSS: `media_statement` is intentionally anonymous (its
    // `feature_query` is part of the pattern, not a name) — keep
    // its early-return distinct from the named-ident path so a
    // future contributor doesn't accidentally fold a new CSS kind
    // into the same arm. Both `rule_set` ident (full selector
    // chain) and `keyframes_statement` ident (`keyframes_name`)
    // come from positional children — there's no `name:` field on
    // either, same pattern as SQL `object_reference`.
    if matches!((lang, kind), (Language::Css, "media_statement")) {
        return None;
    }
    if matches!(
        (lang, kind),
        (Language::Css, "rule_set" | "keyframes_statement")
    ) {
        let name_node = match kind {
            "rule_set" => child_by_kind(node, "selectors"),
            "keyframes_statement" => child_by_kind(node, "keyframes_name"),
            _ => unreachable!("guarded by outer matches!"),
        };
        return name_node
            .and_then(|n| n.node_text_opt(source.as_bytes()))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    // HTML: `element` ident = `tag_name` text from the positional
    // `start_tag` child. `script_element` / `style_element` are
    // shaped the same way.
    if matches!(
        (lang, kind),
        (
            Language::Html,
            "element" | "script_element" | "style_element",
        )
    ) {
        return child_by_kind(node, "start_tag")
            .and_then(|st| child_by_kind(st, "tag_name"))
            .and_then(|n| n.node_text_opt(source.as_bytes()))
            .map(String::from);
    }
    // SQL DDL nodes split across three name shapes:
    //   * `create_index` — direct `column:` named field.
    //   * `drop_index`   — direct `name:` named field.
    //   * everything else — positional `object_reference > name:`.
    //     `child_by_kind` finds the FIRST `object_reference` child
    //     in source order; for `create_function` (which has a second
    //     `object_reference` under `custom_type:` for the RETURNS
    //     type) and `alter_table ... RENAME TO` (which has a second
    //     under `rename_object`), the function-being-declared /
    //     source-table reliably precedes those, so first-match is
    //     the correct entity-being-declared. Qualified names like
    //     `schema.users` resolve via the leaf `name:` field —
    //     skeleton ident is the unqualified leaf (e.g. `users`).
    if matches!(
        (lang, kind),
        (
            Language::Sql,
            "create_table"
                | "create_view"
                | "create_materialized_view"
                | "create_function"
                | "alter_table"
                | "drop_table"
                | "drop_view"
                | "drop_function"
                | "create_index"
                | "drop_index",
        )
    ) {
        let name_node = match kind {
            "create_index" => node.child_by_field_name("column"),
            "drop_index" => node.child_by_field_name("name"),
            _ => {
                child_by_kind(node, "object_reference").and_then(|r| r.child_by_field_name("name"))
            }
        };
        return name_node
            .and_then(|n| n.node_text_opt(source.as_bytes()))
            .map(String::from);
    }
    // Markdown: extract heading text for `atx_heading` / `setext_
    // heading` (both expose `heading_content:` as a named field),
    // and the language tag from `info_string` for fenced code
    // blocks (positional child — walk children by kind).
    if matches!(
        (lang, kind),
        (
            Language::Markdown,
            "atx_heading" | "setext_heading" | "fenced_code_block",
        )
    ) {
        let text_node = if kind == "fenced_code_block" {
            child_by_kind(node, "info_string")
        } else {
            node.child_by_field_name("heading_content")
        };
        return text_node
            .and_then(|n| n.node_text_opt(source.as_bytes()))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    // C++ buries function/method names under a declarator chain.
    // Reuse the scope binder's `extract_inner_identifier` so the
    // skeleton prefilter and `vex usages` agree on what the name is —
    // a duplicated walk could silently drift the moment one side
    // picks up a future operator-overload / abstract-declarator
    // tweak.
    if matches!(
        (lang, kind),
        (Language::Cpp, "function_definition" | "type_definition")
    ) {
        return node
            .child_by_field_name("declarator")
            .and_then(crate::parse::scope::cpp_extract_inner_identifier)
            .and_then(|n| n.node_text_opt(source.as_bytes()))
            .map(String::from);
    }
    // Kotlin: `property_declaration` has no `name:` field — the
    // identifier sits under `variable_declaration > identifier`.
    // Destructuring forms (`val (a, b) = pair`) parse as
    // `multi_variable_declaration` instead and fall through to
    // `ident=None` here.
    if matches!((lang, kind), (Language::Kotlin, "property_declaration")) {
        return child_by_kind(node, "variable_declaration")
            .and_then(|vd| child_by_kind(vd, "identifier"))
            .and_then(|n| n.node_text_opt(source.as_bytes()))
            .map(String::from);
    }
    // Kotlin: `enum_entry` exposes its identifier as a positional
    // `identifier` child (no `name:` field).
    if matches!((lang, kind), (Language::Kotlin, "enum_entry")) {
        return child_by_kind(node, "identifier")
            .and_then(|n| n.node_text_opt(source.as_bytes()))
            .map(String::from);
    }
    // PHP: `const_element` exposes its identifier as a positional
    // `name` child (NOT a `name:` field — distinct from sibling
    // `property_element` which DOES have a `name:` field of type
    // `variable_name`).
    if matches!((lang, kind), (Language::Php, "const_element")) {
        return child_by_kind(node, "name")
            .and_then(|n| n.node_text_opt(source.as_bytes()))
            .map(String::from);
    }
    // PHP: `property_element.name` is a `variable_name` wrapper
    // whose utf8_text would include the leading `$` sigil (e.g.
    // `$a` rather than `a`). Walk one level deeper to the bare
    // `name` child so the ident aligns with every other T1/T2a
    // language (where the skeleton ident never includes language
    // punctuation). Without this arm, users writing
    // `vex pattern 'public $$NAME'` would need to know to include
    // the sigil — asymmetric with the `const_element` arm above
    // which surfaces the bare name natively.
    if matches!((lang, kind), (Language::Php, "property_element")) {
        return node
            .child_by_field_name("name")
            .and_then(|vn| child_by_kind(vn, "name"))
            .and_then(|n| n.node_text_opt(source.as_bytes()))
            .map(String::from);
    }
    // For Rust `impl_item` AND Kotlin `type_alias` the identifying
    // field is `type`, not `name`. (Note: Go also has a `type_alias`
    // kind but it uses `name:`, so the Kotlin arm must be language-
    // gated — same-kind-name-different-field-shape across grammars.
    // Swift `typealias_declaration` uses `name:` (NOT `type:`) — it
    // flows through the generic fallback below, NOT this override.)
    let field = if matches!(
        (lang, kind),
        (Language::Rust, "impl_item") | (Language::Kotlin, "type_alias")
    ) {
        "type"
    } else {
        "name"
    };
    let name_node = node.child_by_field_name(field)?;
    name_node.node_text_opt(source.as_bytes()).map(String::from)
}

/// First child of `node` whose kind equals `kind`, or `None`. Walks
/// the children once; allocates nothing beyond the tree-sitter
/// cursor. Useful when a grammar exposes a meaningful child as a
/// positional (not named) field — e.g. SQL `object_reference` under
/// `create_table`, or Markdown `info_string` under `fenced_code_block`.
fn child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    // Bind the result before the function returns so `cursor`'s borrow
    // outlives the iterator — direct `return node.children(&mut cursor)…`
    // fails E0597 because tree-sitter's iterator holds `&mut cursor`.
    let result = node.children(&mut cursor).find(|c| c.kind() == kind);
    result
}
