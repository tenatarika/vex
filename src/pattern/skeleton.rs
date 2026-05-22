//! Phase 11.4 — pattern skeleton extraction.
//!
//! At index time we walk each source file's tree-sitter AST and emit a
//! [`Skeleton`] per *pattern-targetable* node (function, struct, class,
//! method, …). The skeleton carries the node's structural shape
//! (`kind`, optional `parent_kind`), its leaf identifier when one is
//! recoverable, and span info — enough for the Phase 11.4 prefilter
//! (Inc 5) to narrow `vex pattern` candidate files without re-parsing.
//!
//! Inc 2 scope: **pure function only**. No storage wiring, no FST, no
//! pipeline integration — just the extractor + unit tests. Inc 3 will
//! pack `Skeleton`s into a side-table behind a `PatternSkeletonHeader`
//! that older readers skip.
//!
//! Per-language coverage (T1 lands now; T2/T3 in follow-up trains —
//! see `.claude/Task/PHASE11.4-first-class-pattern.md`):
//!
//! | Tier | Languages                                        | Allowlist     |
//! |------|--------------------------------------------------|---------------|
//! | T1   | Rust, TypeScript, Python                         | populated     |
//! | T2a  | Go, C++, C#, SQL, Markdown, Java, CSS, HTML      | populated     |
//! | T2   | Kotlin, Swift, PHP, Ruby                         | empty for now |
//! | T3   | YAML, TOML, Bash, Lua                            | empty (final) |
//!
//! JavaScript shares the TypeScript grammar (`Language::TypeScript`)
//! via `"js" | "jsx" → TypeScript` in the extension map, so the T1
//! TypeScript allowlist already covers it — no separate JS row.
//!
//! An empty allowlist short-circuits to `Vec::new()`, so unrolled-T2
//! / T3 files produce no skeletons and `vex pattern --lang <x>` falls
//! back to live-scan exactly as today.
//!
//! Go-specific note: struct / interface bodies in Go live two AST
//! levels below `type_spec` (`type_spec > struct_type >
//! field_declaration_list`), so [`has_body_block`] returns `false` for
//! `type_spec` even when there's a structural body. Patterns using
//! `$$$BODY` on Go struct/interface declarations therefore fall back
//! to live-scan — correctness preserved, perf-only impact.

use tree_sitter::{Node, Parser};

use crate::parse::language::Language;

/// One structural fingerprint per pattern-targetable AST node.
///
/// Stored in-memory only at this stage. The compact on-disk form lands
/// in Inc 3 (`SkeletonRecord` with string-pool indices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skeleton {
    /// 0-based row of the node's first token.
    pub start_row: u32,
    /// 0-based row of the node's last token. Useful for matching
    /// multi-line `$$$BODY` metavars later (Inc 6).
    pub end_row: u32,
    /// Tree-sitter node kind (e.g. `function_item`, `class_declaration`).
    /// `&'static str` because tree-sitter interns kind names.
    pub kind: &'static str,
    /// Parent node's kind when the parent is *not* the file root
    /// (`source_file` / `program` / `module` / `translation_unit`).
    /// Lets the prefilter distinguish e.g. a bare `function_definition`
    /// from one inside a `template_declaration` (C++) or
    /// `decorated_definition` (Python).
    pub parent_kind: Option<&'static str>,
    /// Leaf identifier when the grammar exposes one for this node
    /// (function name, struct name, impl type). `None` for anonymous
    /// nodes like `lambda`, `arrow_function`, `decorated_definition`.
    pub ident: Option<String>,
    /// Whether the node carries a block-shaped body child (`block`,
    /// `statement_block`, `declaration_list`, …). Inc 6 uses this to
    /// decide whether `$$$BODY` is applicable for the skeleton.
    pub has_block: bool,
}

/// Walk `source` under `lang`'s grammar and emit one [`Skeleton`] per
/// allowlisted node. Returns an empty `Vec` when the language has no
/// allowlist (T2/T3 today) or when tree-sitter fails to parse.
pub fn extract_skeletons(source: &str, lang: Language) -> Vec<Skeleton> {
    let allowlist = pattern_targetable_kinds(lang);
    if allowlist.is_empty() {
        return Vec::new();
    }
    let mut parser = Parser::new();
    let ts_lang = lang.ts_language();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut skeletons = Vec::new();
    walk(tree.root_node(), source, lang, allowlist, &mut skeletons);
    skeletons
}

fn walk(
    node: Node<'_>,
    source: &str,
    lang: Language,
    allowlist: &[&'static str],
    out: &mut Vec<Skeleton>,
) {
    let kind = node.kind();
    if allowlist.contains(&kind) {
        let parent_kind = node
            .parent()
            .map(|p| p.kind())
            .filter(|k| !is_root_kind(k, lang));
        out.push(Skeleton {
            start_row: node.start_position().row as u32,
            end_row: node.end_position().row as u32,
            kind,
            parent_kind,
            ident: extract_ident(node, source, lang, kind),
            has_block: has_body_block(node),
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, source, lang, allowlist, out);
    }
}

/// Per-language root-node kind suppression. The shared base set
/// (`source_file` / `program` / `module` / `translation_unit` /
/// `compilation_unit`) covers Rust, Go, TS, Python, C++, C# — none of
/// those grammars use any of those names elsewhere. `stylesheet` is
/// CSS's root; the kind name is unused by every other grammar in the
/// matrix so it stays in the global base set.
///
/// `document` is gated to Markdown AND HTML because both grammars use
/// it as the file root, but YAML uses the same kind name for a
/// *non-root* subtree under `stream`. A global suppression would
/// silently break the parent-kind contract the moment YAML moves out
/// of the empty T3 allowlist.
fn is_root_kind(kind: &str, lang: Language) -> bool {
    if matches!(
        kind,
        "source_file"
            | "program"
            | "module"
            | "translation_unit"
            | "compilation_unit"
            | "stylesheet"
    ) {
        return true;
    }
    matches!(
        (kind, lang),
        ("document", Language::Markdown) | ("document", Language::Html)
    )
}

/// T1 allowlist. T2/T3 languages return an empty slice — see module docs.
fn pattern_targetable_kinds(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Rust => &[
            "function_item",
            "struct_item",
            "enum_item",
            "impl_item",
            "trait_item",
            "mod_item",
            "type_item",
            "const_item",
            "static_item",
            "macro_definition",
        ],
        Language::TypeScript => &[
            "function_declaration",
            "function_expression",
            "arrow_function",
            "class_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
        ],
        Language::Python => &[
            "function_definition",
            "class_definition",
            "decorated_definition",
            "lambda",
        ],
        Language::Cpp => &[
            // Top-level functions and methods. `function_definition`
            // buries the name under a declarator chain — see
            // [`extract_ident`] for the special-case walker.
            "function_definition",
            // Named record / enum kinds — all carry `name:
            // type_identifier`. `class_specifier` / `struct_specifier`
            // also fire for forward declarations (`class Foo;`); the
            // skeleton emits with `has_block=false` in that case.
            "class_specifier",
            "struct_specifier",
            "union_specifier",
            "enum_specifier",
            "namespace_definition",
            // Wrapper around fn/class templates. No name on the
            // wrapper itself — anonymous, but the inner specifier
            // emits its own skeleton. Also fires for C++20 concept
            // declarations (`template<typename T> concept X = ...`);
            // the concept body lives outside the allowlist so the
            // skeleton is just an anonymous wrapper.
            "template_declaration",
            // Type aliases: `using V = T;` and the older
            // `typedef T V;` (note: `type_definition` puts the alias
            // name in `declarator:`, not `name:`). Function-pointer
            // typedefs (`typedef int (*FuncPtr)();`) land here with
            // `ident=None` — the abstract declarator chain has no
            // identifier the helper can recover.
            "alias_declaration",
            "type_definition",
            // Anonymous closures: `auto f = [](int x) { return x; };`.
            "lambda_expression",
            // Intentionally absent (deferred / out of scope):
            //   * `field_declaration` — multi-declarator forms
            //     (`int a, b, c;`) would emit only the first name.
            //   * `declaration` — prototypes; the body-bearing
            //     `function_definition` is what users target.
            //   * `concept_definition` — handled at the wrapper level
            //     via `template_declaration`; revisit if patterns
            //     need to target the bare concept body.
            //   * `friend_declaration`, `static_assert_declaration` —
            //     not pattern-targetable.
        ],
        Language::CSharp => &[
            // Type declarations — all carry `name: identifier`.
            "class_declaration",
            "interface_declaration",
            "struct_declaration",
            "enum_declaration",
            "record_declaration",
            // Members — methods, constructors, destructors, accessor
            // properties, delegates. All carry `name: identifier`
            // (`~Foo()` parses with `name:` pointing at `Foo`, the
            // `~` is its own keyword child).
            "method_declaration",
            "constructor_declaration",
            "destructor_declaration",
            "property_declaration",
            "delegate_declaration",
            "local_function_statement",
            // Namespaces — block-bodied `namespace X { ... }` and the
            // C# 10 file-scoped `namespace X;` form.
            "namespace_declaration",
            "file_scoped_namespace_declaration",
            // Anonymous callables: `x => x + 1` lambdas and the older
            // `delegate { ... }` syntax.
            "lambda_expression",
            "anonymous_method_expression",
            // Intentionally absent (deferred / out of scope):
            //   * `field_declaration` — multi-declarator forms
            //     (`int a, b, c;`) would emit only the first name.
            //   * `event_declaration` — niche surface.
            //   * `operator_declaration`,
            //     `conversion_operator_declaration` — no `name:`
            //     field; the operator token lives under `operator:`,
            //     so inclusion needs a special-case arm in
            //     `extract_ident`. Revisit when users ask.
            //   * `indexer_declaration` (`public $T this[$I $P]
            //     { ... }`) — same "no `name:` field" problem; the
            //     `this` keyword is its identity. Falls back to
            //     live-scan today.
            //   * `using_directive` / `extern_alias_directive` —
            //     binding sites, not pattern targets.
            // Note on explicit interface impl (`void IFoo.Run()`):
            // the prefix is an `explicit_interface_specifier`
            // sibling, NOT part of `name:`. The skeleton ident
            // returns just `Run` — `vex pattern 'void $NAME()'` then
            // captures `Run` from the source text, which is the
            // user-intuitive result.
        ],
        Language::Sql => &[
            // Top-level DDL statements — each carries an object name
            // either via `object_reference > name:` (most), a direct
            // `column:` field (`create_index`), or a direct `name:`
            // field (`drop_index`). Extraction is dispatched in
            // [`extract_ident`] below.
            "create_table",
            "create_index",
            "create_view",
            "create_materialized_view",
            "create_function",
            "alter_table",
            "drop_table",
            "drop_view",
            "drop_function",
            "drop_index",
            // Intentionally absent:
            //   * `create_trigger`, `create_schema`, `create_type` —
            //     niche; add when patterns need them.
            //   * `CREATE PROCEDURE` — the tree-sitter-sequel grammar
            //     does not have a `create_procedure` node kind;
            //     procedure declarations parse as ERROR nodes. Falls
            //     through to live-scan.
            //   * Plain `select` / DML — not pattern-targetable as
            //     definitions; `vex pattern` falls through to live-
            //     scan for ad-hoc query shapes.
        ],
        Language::Markdown => &[
            // Headings + fenced code blocks are the structurally
            // pinnable elements of a Markdown document.
            "atx_heading",
            "setext_heading",
            "fenced_code_block",
            // Intentionally absent:
            //   * `paragraph` / `inline` — too noisy; every line of
            //     prose would land in the skeleton table.
            //   * Lists, blockquotes, tables — revisit when there's
            //     demand for `vex pattern` on those shapes.
        ],
        Language::Css => &[
            // Top-level CSS rules + at-rules. `rule_set` carries
            // selectors text as its ident (`.btn`, `body > p`),
            // `keyframes_statement` has a proper `name:` field, and
            // `media_statement` is anonymous (no useful name; its
            // `feature_query` is part of the pattern body, not a
            // name). All three carry a `block:` body.
            "rule_set",
            "keyframes_statement",
            "media_statement",
            // Intentionally absent:
            //   * `import_statement` (`@import "..."`) — binding
            //     site, not a pattern target.
            //   * `charset_statement`, `namespace_statement`,
            //     `supports_statement`, generic `at_rule` — niche;
            //     revisit when patterns need them.
            //   * `declaration` (`color: red;`) — too granular;
            //     would emit thousands of skeletons per file.
        ],
        Language::Html => &[
            // Every named element + raw-text elements. Ident is the
            // `tag_name` inside `start_tag`, extracted via the
            // language-specific arm in `extract_ident`.
            "element",
            "script_element",
            "style_element",
            // Intentionally absent:
            //   * `doctype`, `xml_declaration` — no useful name; the
            //     declaration text is the entire content.
            //   * Inline attribute / text nodes — too granular.
        ],
        Language::Java => &[
            // Top-level type declarations — all carry
            // `name: identifier`. `record_declaration` is Java 16+,
            // `annotation_type_declaration` covers `@interface`.
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
            "annotation_type_declaration",
            // Members — methods, constructors. Both have
            // `name: identifier` and `body: block`.
            // `compact_constructor_declaration` is the Java 16+
            // record-only compact form (`public User { validate(); }`),
            // same `name: identifier` + `body: block` shape — without
            // it, record-targeted constructor patterns would silently
            // fall through to live-scan.
            "method_declaration",
            "constructor_declaration",
            "compact_constructor_declaration",
            // Anonymous closures. Lambda body may be `block` (block-
            // bodied) or an inline expression (no block) — has_block
            // reflects this naturally.
            "lambda_expression",
            // Intentionally absent:
            //   * `field_declaration` — multi-declarator forms
            //     (`int a, b, c;`) would emit only the first name;
            //     same reason as C++/C#.
            //   * `package_declaration`, `import_declaration` —
            //     binding sites / metadata, not pattern targets.
            //   * `annotation` — too noisy (every `@Override` on a
            //     method would emit).
            //   * `static_initializer` and bare `block` instance
            //     initializers — niche. Note: tree-sitter-java has
            //     no `instance_initializer` kind; `{ ... }` blocks
            //     directly under `class_body` ARE the instance-init
            //     form.
        ],
        Language::Go => &[
            // Top-level decls — `function_declaration` and
            // `method_declaration` are first-class; `type_spec` /
            // `var_spec` / `const_spec` are the named units inside
            // grouped `type (...)` / `var (...)` / `const (...)`
            // wrappers (also fired for ungrouped single-spec forms).
            "function_declaration",
            "method_declaration",
            "type_spec",
            // `type_alias` is a sibling of `type_spec` for `type
            // Alias = Target` (Go 1.9+ alias form). Same `name:`
            // field shape, so `extract_ident` handles it.
            "type_alias",
            "var_spec",
            "const_spec",
            // Anonymous closures — `value = func(x) { ... }` and
            // `defer func() { ... }()` patterns.
            "func_literal",
        ],
        _ => &[],
    }
}

/// Return the leaf identifier text for declaration-shaped nodes. The
/// field name varies by language and kind — anonymous nodes (lambdas,
/// arrow functions, decorated wrappers) return `None`.
fn extract_ident(node: Node<'_>, source: &str, lang: Language, kind: &str) -> Option<String> {
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
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
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
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
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
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
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
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
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
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .map(String::from);
    }
    // For Rust `impl_item` the identifying field is `type`, not `name`.
    let field = if matches!((lang, kind), (Language::Rust, "impl_item")) {
        "type"
    } else {
        "name"
    };
    let name_node = node.child_by_field_name(field)?;
    name_node
        .utf8_text(source.as_bytes())
        .ok()
        .map(String::from)
}

/// First child of `node` whose kind equals `kind`, or `None`. Walks
/// the children once; allocates nothing beyond the tree-sitter
/// cursor. Useful when a grammar exposes a meaningful child as a
/// positional (not named) field — e.g. SQL `object_reference` under
/// `create_table`, or Markdown `info_string` under `fenced_code_block`.
fn child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let result = node.children(&mut cursor).find(|c| c.kind() == kind);
    result
}

/// Block-shaped body markers, language-agnostic. Tree-sitter uses these
/// kinds for the statement-list child of declarations across grammars.
fn has_body_block(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            // Universal markers across T1 grammars; add per-language
            // body kind names below this line as T2 languages get
            // promoted (e.g. Java would bring `class_body` flavors,
            // Kotlin `function_body`, etc.).
            "block"
                | "statement_block"
                | "declaration_list"
                | "field_declaration_list"
                | "enum_body"
                | "enum_variant_list"
                | "enumerator_list"           // C++ enum body
                | "class_body"
                | "interface_body"
                | "compound_statement"           // C++ fn / lambda body
                | "enum_member_declaration_list" // C# enum body
                | "accessor_list" // C# property body
                | "column_definitions" // SQL CREATE TABLE body
                | "function_body" // SQL CREATE FUNCTION body
                | "code_fence_content" // Markdown fenced block content
                | "annotation_type_body" // Java @interface body
                | "constructor_body" // Java constructor body
                | "keyframe_block_list" // CSS @keyframes body
        )
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(lang: Language, src: &str) -> Vec<Skeleton> {
        extract_skeletons(src, lang)
    }

    #[test]
    fn rust_function_emits_single_skeleton() {
        let sk = extract(Language::Rust, "fn foo() {}\n");
        assert_eq!(sk.len(), 1);
        assert_eq!(sk[0].kind, "function_item");
        assert_eq!(sk[0].parent_kind, None);
        assert_eq!(sk[0].ident.as_deref(), Some("foo"));
        assert!(sk[0].has_block);
        assert_eq!(sk[0].start_row, 0);
    }

    #[test]
    fn rust_struct_unit_has_no_block() {
        let sk = extract(Language::Rust, "struct Foo;\n");
        assert_eq!(sk.len(), 1);
        assert_eq!(sk[0].kind, "struct_item");
        assert_eq!(sk[0].ident.as_deref(), Some("Foo"));
        assert!(!sk[0].has_block);
    }

    #[test]
    fn rust_impl_ident_comes_from_type_field() {
        let sk = extract(Language::Rust, "impl Foo {\n    fn bar(&self) {}\n}\n");
        // Two skeletons: the impl_item and the inner function_item.
        let impl_sk = sk.iter().find(|s| s.kind == "impl_item").unwrap();
        assert_eq!(impl_sk.ident.as_deref(), Some("Foo"));
        assert!(impl_sk.has_block);
        let fn_sk = sk.iter().find(|s| s.kind == "function_item").unwrap();
        assert_eq!(fn_sk.parent_kind, Some("declaration_list"));
        assert_eq!(fn_sk.ident.as_deref(), Some("bar"));
    }

    #[test]
    fn rust_nested_struct_in_mod_carries_parent_kind() {
        let sk = extract(Language::Rust, "mod outer {\n    struct Inner;\n}\n");
        let mod_sk = sk.iter().find(|s| s.kind == "mod_item").unwrap();
        assert_eq!(mod_sk.parent_kind, None);
        let struct_sk = sk.iter().find(|s| s.kind == "struct_item").unwrap();
        assert_eq!(struct_sk.parent_kind, Some("declaration_list"));
        assert_eq!(struct_sk.ident.as_deref(), Some("Inner"));
    }

    #[test]
    fn typescript_class_with_method_emits_both() {
        let sk = extract(
            Language::TypeScript,
            "class Foo {\n  bar() { return 1; }\n}\n",
        );
        let class_sk = sk.iter().find(|s| s.kind == "class_declaration").unwrap();
        assert_eq!(class_sk.ident.as_deref(), Some("Foo"));
        assert!(class_sk.has_block);
        let method_sk = sk.iter().find(|s| s.kind == "method_definition").unwrap();
        assert_eq!(method_sk.ident.as_deref(), Some("bar"));
    }

    #[test]
    fn typescript_arrow_function_has_no_ident() {
        let sk = extract(Language::TypeScript, "const f = (x) => x;\n");
        let arrow = sk.iter().find(|s| s.kind == "arrow_function").unwrap();
        assert_eq!(arrow.ident, None);
    }

    #[test]
    fn python_decorated_function_emits_wrapper_and_inner() {
        let sk = extract(Language::Python, "@app.get(\"/\")\ndef foo():\n    pass\n");
        let wrapper = sk
            .iter()
            .find(|s| s.kind == "decorated_definition")
            .unwrap();
        assert_eq!(wrapper.ident, None);
        let inner = sk.iter().find(|s| s.kind == "function_definition").unwrap();
        assert_eq!(inner.ident.as_deref(), Some("foo"));
        assert_eq!(inner.parent_kind, Some("decorated_definition"));
    }

    #[test]
    fn python_lambda_has_no_ident() {
        let sk = extract(Language::Python, "f = lambda x: x\n");
        let lam = sk.iter().find(|s| s.kind == "lambda").unwrap();
        assert_eq!(lam.ident, None);
    }

    #[test]
    fn t2_language_returns_empty_until_rolled_out() {
        // Kotlin is still T2 — not yet in the allowlist. Java used
        // to live here but moved to T2a; this test will need to
        // repoint at the next still-empty T2 language each time one
        // rolls out (Swift / PHP / Ruby remain). Once *all* T2
        // languages are populated, repoint at a T3 language we
        // explicitly never plan to fill (e.g. `Language::Yaml` or
        // `Language::Toml`) so the canary stops shifting.
        let sk = extract(Language::Kotlin, "class Foo\n");
        assert!(sk.is_empty());
    }

    #[test]
    fn go_function_emits_single_skeleton() {
        let sk = extract(Language::Go, "package main\n\nfunc Foo() {}\n");
        let f = sk
            .iter()
            .find(|s| s.kind == "function_declaration")
            .unwrap();
        assert_eq!(f.ident.as_deref(), Some("Foo"));
        assert_eq!(f.parent_kind, None);
        assert!(f.has_block);
    }

    #[test]
    fn go_method_decl_carries_receiver_field_name() {
        let sk = extract(
            Language::Go,
            "package main\n\ntype Bar struct{}\n\nfunc (b *Bar) Hello() {}\n",
        );
        let m = sk.iter().find(|s| s.kind == "method_declaration").unwrap();
        // `name:` is a `field_identifier`, not a plain `identifier` —
        // pin that `extract_ident` still recovers it.
        assert_eq!(m.ident.as_deref(), Some("Hello"));
        assert!(m.has_block);
    }

    #[test]
    fn go_type_spec_inside_grouped_decl_emits_per_spec() {
        let sk = extract(
            Language::Go,
            "package main\n\ntype (\n    Foo struct{}\n    Bar interface{}\n)\n",
        );
        let specs: Vec<_> = sk.iter().filter(|s| s.kind == "type_spec").collect();
        assert_eq!(specs.len(), 2);
        let names: Vec<&str> = specs.iter().filter_map(|s| s.ident.as_deref()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"Bar"));
        // `type_spec` body lives one level deeper — has_block stays
        // false (see module doc note).
        assert!(specs.iter().all(|s| !s.has_block));
        // Parent is `type_declaration` (the `type (...)` wrapper).
        assert!(specs
            .iter()
            .all(|s| s.parent_kind == Some("type_declaration")));
    }

    #[test]
    fn go_func_literal_is_anonymous() {
        let sk = extract(
            Language::Go,
            "package main\n\nvar handler = func(x int) int { return x }\n",
        );
        let lit = sk.iter().find(|s| s.kind == "func_literal").unwrap();
        assert_eq!(lit.ident, None);
        assert!(lit.has_block);
    }

    #[test]
    fn go_var_and_const_specs_carry_idents() {
        let sk = extract(
            Language::Go,
            "package main\n\nvar top = 42\nconst MyConst = \"hi\"\n",
        );
        let v = sk.iter().find(|s| s.kind == "var_spec").unwrap();
        assert_eq!(v.ident.as_deref(), Some("top"));
        let c = sk.iter().find(|s| s.kind == "const_spec").unwrap();
        assert_eq!(c.ident.as_deref(), Some("MyConst"));
    }

    #[test]
    fn go_type_spec_ungrouped_emits_same_parent_kind() {
        // Reviewer GAP-2: pin that the ungrouped single-spec form
        // produces the same `parent_kind` as the grouped form —
        // tree-sitter's grammar wraps both in `type_declaration`.
        let sk = extract(Language::Go, "package main\n\ntype Foo struct{}\n");
        let spec = sk.iter().find(|s| s.kind == "type_spec").unwrap();
        assert_eq!(spec.ident.as_deref(), Some("Foo"));
        assert_eq!(spec.parent_kind, Some("type_declaration"));
    }

    #[test]
    fn go_type_alias_form_emits_skeleton_with_ident() {
        // Reviewer GAP-3: `type Alias = Target` parses as `type_alias`,
        // not `type_spec`. Without the dedicated allowlist entry it
        // would silently produce zero skeletons.
        let sk = extract(Language::Go, "package main\n\ntype Alias = int\n");
        let a = sk.iter().find(|s| s.kind == "type_alias").unwrap();
        assert_eq!(a.ident.as_deref(), Some("Alias"));
        assert_eq!(a.parent_kind, Some("type_declaration"));
    }

    #[test]
    fn go_nested_func_literal_has_expression_list_parent() {
        // Reviewer GAP-5: inner closures sit inside an
        // `expression_list` wrapper (right-hand-side of a short var
        // declaration). Pin so a future prefilter that narrows by
        // `parent_kind` doesn't regress this surface.
        let sk = extract(
            Language::Go,
            "package main\n\nvar outer = func() { inner := func() {} }\n",
        );
        let literals: Vec<_> = sk.iter().filter(|s| s.kind == "func_literal").collect();
        assert_eq!(literals.len(), 2, "outer + inner literal");
        let inner = literals.iter().find(|s| s.start_row > 0 || s.end_row > 0);
        // Both literals are anonymous regardless of nesting depth.
        assert!(literals.iter().all(|s| s.ident.is_none()));
        // The deeper one sits under an `expression_list` (RHS of
        // `inner := func() {}`), not directly under the outer block.
        assert!(
            literals
                .iter()
                .any(|s| s.parent_kind == Some("expression_list")),
            "expected at least one literal with expression_list parent, got: {:?}",
            literals.iter().map(|s| s.parent_kind).collect::<Vec<_>>()
        );
        let _ = inner; // silence unused
    }

    #[test]
    fn go_grouped_var_block_parents_are_var_spec_list() {
        // Reviewer GAP-6: grouped `var (...)` puts each `var_spec`
        // under `var_spec_list`, distinct from the ungrouped case
        // where the parent is `var_declaration`.
        let sk = extract(
            Language::Go,
            "package main\n\nvar (\n    aOne int\n    bTwo string\n)\n",
        );
        let specs: Vec<_> = sk.iter().filter(|s| s.kind == "var_spec").collect();
        assert_eq!(specs.len(), 2);
        assert!(specs.iter().all(|s| s.parent_kind == Some("var_spec_list")));
        let names: Vec<&str> = specs.iter().filter_map(|s| s.ident.as_deref()).collect();
        assert!(names.contains(&"aOne"));
        assert!(names.contains(&"bTwo"));
    }

    #[test]
    fn cpp_free_function_emits_skeleton_with_ident() {
        let sk = extract(Language::Cpp, "void freefn() {}\n");
        let f = sk.iter().find(|s| s.kind == "function_definition").unwrap();
        assert_eq!(f.ident.as_deref(), Some("freefn"));
        assert!(f.has_block);
        assert_eq!(f.parent_kind, None);
    }

    #[test]
    fn cpp_qualified_method_definition_extracts_inner_name() {
        // `int Server::Start() {}` — the declarator chain ends in
        // `qualified_identifier { scope: Server, name: Start }`. The
        // skeleton ident must surface `Start`, not the qualifier.
        let sk = extract(Language::Cpp, "int Server::Start() { return 0; }\n");
        let f = sk.iter().find(|s| s.kind == "function_definition").unwrap();
        assert_eq!(f.ident.as_deref(), Some("Start"));
    }

    #[test]
    fn cpp_class_emits_with_field_declaration_list_body() {
        let sk = extract(
            Language::Cpp,
            "class Server {\npublic:\n    int Start();\n};\n",
        );
        let c = sk.iter().find(|s| s.kind == "class_specifier").unwrap();
        assert_eq!(c.ident.as_deref(), Some("Server"));
        assert!(c.has_block, "field_declaration_list counts as a body");
    }

    #[test]
    fn cpp_namespace_definition_carries_name_and_body() {
        let sk = extract(Language::Cpp, "namespace App {\nint counter = 0;\n}\n");
        let n = sk
            .iter()
            .find(|s| s.kind == "namespace_definition")
            .unwrap();
        assert_eq!(n.ident.as_deref(), Some("App"));
        assert!(n.has_block, "namespace body is declaration_list");
    }

    #[test]
    fn cpp_template_declaration_is_anonymous_wrapper() {
        // The wrapper itself has no name; the inner `function_definition`
        // is what carries `identity`. Mirrors Python `decorated_definition`.
        let sk = extract(
            Language::Cpp,
            "template<typename T>\nT identity(T x) { return x; }\n",
        );
        let wrapper = sk
            .iter()
            .find(|s| s.kind == "template_declaration")
            .unwrap();
        assert_eq!(wrapper.ident, None);
        let inner = sk.iter().find(|s| s.kind == "function_definition").unwrap();
        assert_eq!(inner.ident.as_deref(), Some("identity"));
        assert_eq!(inner.parent_kind, Some("template_declaration"));
    }

    #[test]
    fn cpp_alias_and_typedef_both_emit_named_skeletons() {
        let sk = extract(
            Language::Cpp,
            "using IntPtr = int*;\ntypedef int Counter;\n",
        );
        let alias = sk.iter().find(|s| s.kind == "alias_declaration").unwrap();
        assert_eq!(alias.ident.as_deref(), Some("IntPtr"));
        // `type_definition` puts the alias name in `declarator:`, not
        // `name:` — that's the branch `extract_cpp_declarator_ident`
        // covers.
        let td = sk.iter().find(|s| s.kind == "type_definition").unwrap();
        assert_eq!(td.ident.as_deref(), Some("Counter"));
    }

    #[test]
    fn cpp_enum_class_has_enumerator_list_body() {
        let sk = extract(Language::Cpp, "enum class Mode { Fast, Slow };\n");
        let e = sk.iter().find(|s| s.kind == "enum_specifier").unwrap();
        assert_eq!(e.ident.as_deref(), Some("Mode"));
        assert!(
            e.has_block,
            "enumerator_list must count as a body for $$$BODY matching"
        );
    }

    #[test]
    fn cpp_lambda_expression_is_anonymous_with_block() {
        let sk = extract(Language::Cpp, "auto f = [](int x) { return x; };\n");
        let lam = sk.iter().find(|s| s.kind == "lambda_expression").unwrap();
        assert_eq!(lam.ident, None);
        assert!(lam.has_block, "lambda body is compound_statement");
    }

    #[test]
    fn cpp_union_specifier_emits_named_skeleton() {
        let sk = extract(Language::Cpp, "union Variant { int i; double d; };\n");
        let u = sk.iter().find(|s| s.kind == "union_specifier").unwrap();
        assert_eq!(u.ident.as_deref(), Some("Variant"));
        assert!(u.has_block);
    }

    #[test]
    fn cpp_forward_class_decl_emits_skeleton_with_no_block() {
        // Reviewer GAP-1 (critical): forward declarations share the
        // `class_specifier` node kind with full definitions —
        // `has_block=false` is the *only* signal that separates them.
        // Pin both halves of the contract so a future tweak to
        // has_body_block doesn't silently flip the prefilter.
        let sk = extract(Language::Cpp, "class Server;\nstruct Foo;\n");
        let c = sk.iter().find(|s| s.kind == "class_specifier").unwrap();
        assert_eq!(c.ident.as_deref(), Some("Server"));
        assert!(
            !c.has_block,
            "forward class decl must report has_block=false"
        );
        let s = sk.iter().find(|s| s.kind == "struct_specifier").unwrap();
        assert_eq!(s.ident.as_deref(), Some("Foo"));
        assert!(!s.has_block);
    }

    #[test]
    fn cpp_typedef_fn_ptr_ident_is_none() {
        // Reviewer GAP-2 (critical): `typedef int (*FuncPtr)();` has
        // an abstract function declarator with no recoverable
        // identifier on the walked path. Pin `ident=None` so a
        // future "smarter" walker doesn't accidentally start
        // returning the wrong substring.
        let sk = extract(Language::Cpp, "typedef int (*FuncPtr)();\n");
        let td = sk.iter().find(|s| s.kind == "type_definition").unwrap();
        assert_eq!(td.ident, None, "fn-ptr typedef must report ident=None");
    }

    #[test]
    fn cpp_operator_overload_ident_is_none() {
        // Reviewer GAP-4: `operator_name` is not an identifier kind,
        // so `extract_inner_identifier` terminates with None. Correct
        // behaviour, but unpinned until now.
        let sk = extract(
            Language::Cpp,
            "struct Foo {};\nint operator+(const Foo& a, const Foo& b) { return 0; }\n",
        );
        let op = sk.iter().find(|s| s.kind == "function_definition").unwrap();
        assert_eq!(op.ident, None, "operator overload must report ident=None");
    }

    #[test]
    fn cpp_fn_returning_fn_ptr_ident_is_none() {
        // Reviewer GAP-3: nested abstract declarators in
        // `int (*foo())();`-style return types break the walk —
        // pin the `None` outcome.
        let sk = extract(Language::Cpp, "int (*foo())() { return 0; }\n");
        if let Some(f) = sk.iter().find(|s| s.kind == "function_definition") {
            assert_eq!(f.ident, None, "fn-returning-fn-ptr must report ident=None",);
        }
        // Either no skeleton (parse rejection) or ident=None is
        // acceptable; what we want to fail loudly is a wrong-string
        // surface.
    }

    #[test]
    fn cpp_class_template_inner_class_has_template_declaration_parent() {
        // Reviewer GAP-6: the function-template case is tested but
        // the class-template inner class wasn't pinned.
        let sk = extract(Language::Cpp, "template<typename T>\nclass Box {};\n");
        let inner = sk.iter().find(|s| s.kind == "class_specifier").unwrap();
        assert_eq!(inner.ident.as_deref(), Some("Box"));
        assert_eq!(inner.parent_kind, Some("template_declaration"));
    }

    #[test]
    fn csharp_class_emits_with_body_and_method_child() {
        let sk = extract(
            Language::CSharp,
            "class Server {\n    public int Start() { return 0; }\n}\n",
        );
        let c = sk.iter().find(|s| s.kind == "class_declaration").unwrap();
        assert_eq!(c.ident.as_deref(), Some("Server"));
        assert!(c.has_block, "declaration_list counts as body");
        let m = sk.iter().find(|s| s.kind == "method_declaration").unwrap();
        assert_eq!(m.ident.as_deref(), Some("Start"));
        assert_eq!(m.parent_kind, Some("declaration_list"));
    }

    #[test]
    fn csharp_interface_struct_enum_record_emit_named_skeletons() {
        let sk = extract(
            Language::CSharp,
            "interface IRunner { void Run(); }\n\
             struct Point { public int X; }\n\
             enum Mode { Fast, Slow }\n\
             record User(string Name);\n",
        );
        for (kind, name) in [
            ("interface_declaration", "IRunner"),
            ("struct_declaration", "Point"),
            ("enum_declaration", "Mode"),
            ("record_declaration", "User"),
        ] {
            let s = sk
                .iter()
                .find(|s| s.kind == kind)
                .unwrap_or_else(|| panic!("missing {kind}"));
            assert_eq!(s.ident.as_deref(), Some(name));
        }
    }

    #[test]
    fn csharp_enum_body_counts_as_has_block() {
        // `enum_member_declaration_list` is the C# enum body — pin
        // that the language-specific arm of `has_body_block` was
        // wired up.
        let sk = extract(Language::CSharp, "enum Mode { Fast, Slow }\n");
        let e = sk.iter().find(|s| s.kind == "enum_declaration").unwrap();
        assert!(
            e.has_block,
            "C# enum_member_declaration_list must register as body",
        );
    }

    #[test]
    fn csharp_property_emits_with_accessor_list_body() {
        let sk = extract(
            Language::CSharp,
            "class C {\n    public string Name { get; set; }\n}\n",
        );
        let p = sk
            .iter()
            .find(|s| s.kind == "property_declaration")
            .unwrap();
        assert_eq!(p.ident.as_deref(), Some("Name"));
        assert!(p.has_block, "accessor_list must register as body");
    }

    #[test]
    fn csharp_constructor_and_destructor_carry_type_name() {
        // `~Server()` parses with `name:` pointing at `Server` (the
        // tilde is its own keyword child) — same field as the
        // constructor.
        let sk = extract(
            Language::CSharp,
            "class Server {\n    public Server() {}\n    ~Server() {}\n}\n",
        );
        let ctor = sk
            .iter()
            .find(|s| s.kind == "constructor_declaration")
            .unwrap();
        assert_eq!(ctor.ident.as_deref(), Some("Server"));
        let dtor = sk
            .iter()
            .find(|s| s.kind == "destructor_declaration")
            .unwrap();
        assert_eq!(dtor.ident.as_deref(), Some("Server"));
    }

    #[test]
    fn csharp_delegate_decl_has_no_block() {
        // Delegates are signature-only; their declaration is
        // semicolon-terminated with no body. Pin `has_block=false`
        // so a future signature change doesn't accidentally start
        // matching `$$$BODY` patterns against them.
        let sk = extract(Language::CSharp, "public delegate void Handler(int x);\n");
        let d = sk
            .iter()
            .find(|s| s.kind == "delegate_declaration")
            .unwrap();
        assert_eq!(d.ident.as_deref(), Some("Handler"));
        assert!(!d.has_block);
    }

    #[test]
    fn csharp_local_function_is_pattern_targetable() {
        // C#'s `local_function_statement` lets users target nested
        // helper fns. Pin name + parent kind so the prefilter can
        // narrow on either.
        let sk = extract(
            Language::CSharp,
            "class C { void M() { int Local() { return 0; } } }\n",
        );
        let l = sk
            .iter()
            .find(|s| s.kind == "local_function_statement")
            .unwrap();
        assert_eq!(l.ident.as_deref(), Some("Local"));
        assert_eq!(l.parent_kind, Some("block"));
    }

    #[test]
    fn csharp_namespace_both_block_and_file_scoped() {
        let block_form = extract(Language::CSharp, "namespace App { class C {} }\n");
        let b = block_form
            .iter()
            .find(|s| s.kind == "namespace_declaration")
            .unwrap();
        assert_eq!(b.ident.as_deref(), Some("App"));
        assert!(b.has_block);

        // File-scoped form (C# 10): `namespace App.Other;` — `name:`
        // is `qualified_name`, no body. utf8_text gives the full
        // dotted path which is fine for the prefilter.
        let file_form = extract(Language::CSharp, "namespace App.Other;\nclass C {}\n");
        let f = file_form
            .iter()
            .find(|s| s.kind == "file_scoped_namespace_declaration")
            .unwrap();
        assert_eq!(f.ident.as_deref(), Some("App.Other"));
        assert!(!f.has_block);
    }

    #[test]
    fn csharp_lambda_and_anonymous_method_are_anonymous() {
        let sk = extract(
            Language::CSharp,
            "class C { void M() { System.Func<int,int> a = x => x;\n\
             System.Action b = delegate { }; } }\n",
        );
        let lam = sk.iter().find(|s| s.kind == "lambda_expression").unwrap();
        assert_eq!(lam.ident, None);
        let dm = sk
            .iter()
            .find(|s| s.kind == "anonymous_method_expression")
            .unwrap();
        assert_eq!(dm.ident, None);
        assert!(
            dm.has_block,
            "anonymous-method body is `block`, must register as a body",
        );
    }

    #[test]
    fn csharp_top_level_decls_have_no_parent_kind() {
        // Reviewer CRITICAL: tree-sitter C# uses `compilation_unit`
        // as the file-root node. Missing this in `is_root_kind`
        // would leak `parent_kind = Some("compilation_unit")` for
        // every top-level decl, silently breaking any future
        // prefilter that treats `None` as the file-root marker.
        let sk = extract(
            Language::CSharp,
            "namespace App;\nclass Foo {}\nstruct Bar {}\nenum E { A }\n",
        );
        for kind in [
            "file_scoped_namespace_declaration",
            "class_declaration",
            "struct_declaration",
            "enum_declaration",
        ] {
            let s = sk
                .iter()
                .find(|s| s.kind == kind)
                .unwrap_or_else(|| panic!("missing {kind}"));
            assert_eq!(
                s.parent_kind, None,
                "{kind} at file root must report parent_kind=None, got {:?}",
                s.parent_kind
            );
        }
    }

    #[test]
    fn csharp_record_struct_is_record_declaration_not_struct() {
        // C# 10 `record struct Point(int X, int Y);` — the grammar
        // emits a single `record_declaration` for both the class-
        // and struct-flavoured record syntax. Pin the kind so a
        // future grammar split doesn't silently double-emit (one
        // skeleton each for record_declaration AND struct_declaration).
        let sk = extract(Language::CSharp, "record struct Point(int X, int Y);\n");
        assert!(
            sk.iter()
                .any(|s| s.kind == "record_declaration" && s.ident.as_deref() == Some("Point")),
            "record struct must emit a record_declaration skeleton",
        );
        assert!(
            !sk.iter().any(|s| s.kind == "struct_declaration"),
            "record struct must NOT also emit a struct_declaration",
        );
    }

    #[test]
    fn csharp_expression_bodied_property_has_no_block() {
        // `public int X => 42;` — no `accessor_list` body, so
        // `has_block=false`. Paired with the existing accessor-list
        // test so the two halves of the property surface are pinned.
        let sk = extract(Language::CSharp, "class C { public int X => 42; }\n");
        let p = sk
            .iter()
            .find(|s| s.kind == "property_declaration")
            .unwrap();
        assert_eq!(p.ident.as_deref(), Some("X"));
        assert!(
            !p.has_block,
            "expression-bodied property has no block — pin so $$$BODY \
             matchers correctly fall through to live-scan",
        );
    }

    #[test]
    fn csharp_explicit_interface_impl_ident_drops_iface_prefix() {
        // `void IFoo.Run() {}` — the explicit_interface_specifier
        // (`IFoo.`) is a sibling of `name:`, not part of it. The
        // skeleton ident must return just `Run` so that
        // `vex pattern 'void $NAME()'` consistently captures the
        // member-side name.
        let sk = extract(
            Language::CSharp,
            "interface IFoo { void Run(); }\n\
             class C : IFoo { void IFoo.Run() {} }\n",
        );
        let m = sk
            .iter()
            .filter(|s| s.kind == "method_declaration")
            .find(|s| s.ident.as_deref() == Some("Run"))
            .expect("explicit interface impl must surface as ident=Run");
        // Sanity: the method is the class-side impl, not the
        // interface declaration (which is a method_declaration too).
        assert_eq!(m.parent_kind, Some("declaration_list"));
    }

    #[test]
    fn csharp_grammar_fingerprint_is_stable_and_nonzero() {
        let a = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::CSharp);
        let b = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::CSharp);
        assert_eq!(a, b, "fingerprint must be deterministic");
        assert_ne!(a, 0, "zero is reserved as the not-stored sentinel");
    }

    #[test]
    fn sql_create_table_extracts_object_name_with_body() {
        let sk = extract(
            Language::Sql,
            "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);\n",
        );
        let t = sk.iter().find(|s| s.kind == "create_table").unwrap();
        assert_eq!(t.ident.as_deref(), Some("users"));
        assert!(t.has_block, "`column_definitions` must register as body");
        assert_eq!(t.parent_kind, Some("statement"));
    }

    #[test]
    fn sql_create_index_uses_column_field_for_ident() {
        // `create_index` is the odd one out — name lives directly in
        // the `column:` field, not nested under `object_reference`.
        let sk = extract(Language::Sql, "CREATE INDEX idx_users ON users(name);\n");
        let i = sk.iter().find(|s| s.kind == "create_index").unwrap();
        assert_eq!(i.ident.as_deref(), Some("idx_users"));
    }

    #[test]
    fn sql_create_view_function_alter_drop_emit_named_skeletons() {
        let sk = extract(
            Language::Sql,
            "CREATE VIEW active_users AS SELECT * FROM users;\n\
             CREATE OR REPLACE FUNCTION greet() RETURNS TEXT AS $$ SELECT 'hi'; $$ LANGUAGE plpgsql;\n\
             ALTER TABLE users ADD COLUMN email TEXT;\n\
             DROP TABLE old_data;\n",
        );
        for (kind, name) in [
            ("create_view", "active_users"),
            ("create_function", "greet"),
            ("alter_table", "users"),
            ("drop_table", "old_data"),
        ] {
            let s = sk
                .iter()
                .find(|s| s.kind == kind)
                .unwrap_or_else(|| panic!("missing {kind}"));
            assert_eq!(s.ident.as_deref(), Some(name), "{kind} ident");
        }
    }

    #[test]
    fn java_class_with_method_emits_both() {
        let sk = extract(
            Language::Java,
            "public class Server {\n    public String getName() { return \"\"; }\n}\n",
        );
        let c = sk.iter().find(|s| s.kind == "class_declaration").unwrap();
        assert_eq!(c.ident.as_deref(), Some("Server"));
        assert!(c.has_block, "class_body must register as body");
        assert_eq!(c.parent_kind, None);
        let m = sk.iter().find(|s| s.kind == "method_declaration").unwrap();
        assert_eq!(m.ident.as_deref(), Some("getName"));
        assert_eq!(m.parent_kind, Some("class_body"));
        assert!(m.has_block);
    }

    #[test]
    fn java_interface_enum_record_emit_named_skeletons() {
        let sk = extract(
            Language::Java,
            "interface IRunner { void run(); }\n\
             enum Mode { FAST, SLOW }\n\
             record User(String name, int age) {}\n",
        );
        for (kind, name) in [
            ("interface_declaration", "IRunner"),
            ("enum_declaration", "Mode"),
            ("record_declaration", "User"),
        ] {
            let s = sk
                .iter()
                .find(|s| s.kind == kind)
                .unwrap_or_else(|| panic!("missing {kind}"));
            assert_eq!(s.ident.as_deref(), Some(name), "{kind}");
            assert!(s.has_block, "{kind} must register as body");
        }
    }

    #[test]
    fn java_annotation_type_emits_with_body() {
        // `@interface MyAnnot { ... }` — uses `annotation_type_body`
        // which is the Java-specific arm of `has_body_block`.
        let sk = extract(
            Language::Java,
            "@interface MyAnnot {\n    String value();\n}\n",
        );
        let a = sk
            .iter()
            .find(|s| s.kind == "annotation_type_declaration")
            .unwrap();
        assert_eq!(a.ident.as_deref(), Some("MyAnnot"));
        assert!(a.has_block, "annotation_type_body must register as body",);
    }

    #[test]
    fn java_constructor_emits_with_block_body() {
        let sk = extract(
            Language::Java,
            "class Server {\n    public Server(String name) {}\n}\n",
        );
        let c = sk
            .iter()
            .find(|s| s.kind == "constructor_declaration")
            .unwrap();
        assert_eq!(c.ident.as_deref(), Some("Server"));
        // `constructor_body` is the Java grammar's body kind here.
        // The unit pins both the ident and that has_block fires so
        // the `constructor_body` arm of has_body_block is exercised.
        assert!(c.has_block);
    }

    #[test]
    fn java_lambda_expression_block_vs_expression_body() {
        // Lambdas come in two shapes: block-bodied (`() -> { ... }`)
        // gets `has_block=true`; expression-bodied (`() -> x.foo()`)
        // gets `has_block=false`. Both are anonymous.
        let block_form = extract(
            Language::Java,
            "class C { Runnable r = () -> { System.out.println(\"hi\"); }; }\n",
        );
        let lb = block_form
            .iter()
            .find(|s| s.kind == "lambda_expression")
            .unwrap();
        assert_eq!(lb.ident, None);
        assert!(lb.has_block);

        let expr_form = extract(
            Language::Java,
            "class C { java.util.function.Function<String,Integer> f = s -> s.length(); }\n",
        );
        let le = expr_form
            .iter()
            .find(|s| s.kind == "lambda_expression")
            .unwrap();
        assert_eq!(le.ident, None);
        assert!(
            !le.has_block,
            "expression-bodied lambda must report has_block=false",
        );
    }

    #[test]
    fn java_generic_type_ident_is_bare_name() {
        // `class Box<T> {}` — `name: identifier` gives `Box`; the
        // `<T>` lives in a sibling `type_parameters` field. Pin so
        // a future grammar update doesn't accidentally start
        // returning `Box<T>` or `<T>`.
        let sk = extract(Language::Java, "class Box<T> { T value; }\n");
        let b = sk.iter().find(|s| s.kind == "class_declaration").unwrap();
        assert_eq!(b.ident.as_deref(), Some("Box"));
    }

    #[test]
    fn java_nested_class_carries_class_body_parent_kind() {
        // Inner classes sit under the outer class's `class_body`.
        let sk = extract(
            Language::Java,
            "class Outer {\n    static class Inner {}\n}\n",
        );
        let inner = sk
            .iter()
            .filter(|s| s.kind == "class_declaration")
            .find(|s| s.ident.as_deref() == Some("Inner"))
            .expect("missing Inner");
        assert_eq!(inner.parent_kind, Some("class_body"));
    }

    #[test]
    fn css_rule_set_emits_with_selectors_as_ident() {
        let sk = extract(
            Language::Css,
            ".btn { color: red; }\n#header { width: 100%; }\nbody > p { font-size: 14px; }\n",
        );
        let rules: Vec<_> = sk.iter().filter(|s| s.kind == "rule_set").collect();
        assert_eq!(rules.len(), 3);
        let idents: Vec<&str> = rules.iter().filter_map(|s| s.ident.as_deref()).collect();
        assert!(idents.contains(&".btn"));
        assert!(idents.contains(&"#header"));
        assert!(idents.contains(&"body > p"));
        // Each rule has a `block:` body which is already in the
        // universal markers — pin so a future refactor doesn't
        // accidentally remove the `block` arm.
        assert!(rules.iter().all(|s| s.has_block));
        // Top-level rules sit directly under `stylesheet` (root)
        // and therefore have `parent_kind=None`.
        assert!(rules.iter().all(|s| s.parent_kind.is_none()));
    }

    #[test]
    fn css_keyframes_extracts_name_with_keyframe_block_list_body() {
        let sk = extract(
            Language::Css,
            "@keyframes fade {\n    from { opacity: 0; }\n    to { opacity: 1; }\n}\n",
        );
        let k = sk.iter().find(|s| s.kind == "keyframes_statement").unwrap();
        assert_eq!(k.ident.as_deref(), Some("fade"));
        assert!(k.has_block, "keyframe_block_list must register as body",);
    }

    #[test]
    fn css_media_statement_is_anonymous() {
        let sk = extract(
            Language::Css,
            "@media (max-width: 600px) { .btn { color: blue; } }\n",
        );
        let m = sk.iter().find(|s| s.kind == "media_statement").unwrap();
        assert_eq!(m.ident, None);
        assert!(m.has_block, "media body is a `block`");
    }

    #[test]
    fn css_nested_rule_in_media_block_has_block_as_parent_kind() {
        // Reviewer H2: an inner `.btn { ... }` inside `@media (...) {
        // ... }` sits under the media_statement's `block` child, NOT
        // directly under `media_statement`. The immediate parent —
        // and therefore `parent_kind` — is `Some("block")`. Pin so a
        // prefilter that branches on `parent_kind == "media_statement"`
        // doesn't silently miss every nested rule.
        let sk = extract(
            Language::Css,
            "@media (max-width: 600px) { .btn { color: blue; } }\n",
        );
        let inner = sk
            .iter()
            .find(|s| s.kind == "rule_set")
            .expect("inner rule_set missing");
        assert_eq!(inner.parent_kind, Some("block"));
        // `.btn` selector flows through `selectors` like any other.
        assert_eq!(inner.ident.as_deref(), Some(".btn"));
    }

    #[test]
    fn css_deferred_at_rules_emit_zero_skeletons() {
        // Reviewer aqa-H: `@import`, `@supports`, `@font-face`, and
        // bare `@charset` are intentionally absent from the
        // allowlist. Pin zero-emission so a future train that adds
        // any of them must explicitly opt in (and update this test).
        let sk = extract(
            Language::Css,
            "@import \"reset.css\";\n\
             @charset \"UTF-8\";\n\
             @supports (display: grid) { .btn { color: red; } }\n\
             @font-face { font-family: \"X\"; src: url(\"x.woff2\"); }\n",
        );
        for deferred in [
            "import_statement",
            "charset_statement",
            "supports_statement",
            "at_rule",
        ] {
            assert!(
                !sk.iter().any(|s| s.kind == deferred),
                "{deferred} is deferred — must not emit; saw: {sk:?}",
            );
        }
        // The `.btn` rule inside @supports DOES emit (rule_set is
        // allowlisted) — pin its parent_kind=Some("block") for the
        // same reason as the @media case.
        let nested = sk
            .iter()
            .find(|s| s.kind == "rule_set" && s.ident.as_deref() == Some(".btn"))
            .expect("inner .btn rule_set missing");
        assert_eq!(nested.parent_kind, Some("block"));
    }

    #[test]
    fn css_compound_selectors_with_combinators_extract_full_text() {
        // The `selectors` text covers compound forms — child (`>`),
        // adjacent sibling (`+`), general sibling (`~`),
        // attribute selectors, pseudo-classes — all flow through
        // `utf8_text(selectors)` as the literal source text.
        let sk = extract(
            Language::Css,
            "h1 + p { margin: 0; }\nh1 ~ p { color: gray; }\na[href^=\"https\"] { font-weight: bold; }\na:hover { text-decoration: underline; }\n",
        );
        let idents: Vec<&str> = sk
            .iter()
            .filter(|s| s.kind == "rule_set")
            .filter_map(|s| s.ident.as_deref())
            .collect();
        assert!(idents.contains(&"h1 + p"), "got {idents:?}");
        assert!(idents.contains(&"h1 ~ p"), "got {idents:?}");
        assert!(
            idents.iter().any(|s| s.contains("a[href")),
            "got {idents:?}"
        );
        assert!(idents.contains(&"a:hover"), "got {idents:?}");
    }

    #[test]
    fn css_grammar_fingerprint_is_stable_and_nonzero() {
        let a = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Css);
        let b = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Css);
        assert_eq!(a, b);
        assert_ne!(a, 0);
    }

    #[test]
    fn html_element_ident_is_tag_name() {
        let sk = extract(
            Language::Html,
            "<div id=\"root\">\n  <h1>Hello</h1>\n  <p>Text.</p>\n</div>\n",
        );
        let outer = sk
            .iter()
            .find(|s| s.kind == "element" && s.ident.as_deref() == Some("div"))
            .expect("outer div element missing");
        // Top-level `element` sits directly under `document` — must
        // be suppressed to None by `is_root_kind` (HTML gate).
        assert_eq!(outer.parent_kind, None);
        let h1 = sk
            .iter()
            .find(|s| s.kind == "element" && s.ident.as_deref() == Some("h1"))
            .expect("h1 element missing");
        // Nested elements get the parent element kind, not None.
        assert_eq!(h1.parent_kind, Some("element"));
    }

    #[test]
    fn html_script_and_style_elements_extract_tag_name() {
        let sk = extract(
            Language::Html,
            "<script>console.log('hi');</script>\n<style>body { color: red; }</style>\n",
        );
        let s = sk.iter().find(|s| s.kind == "script_element").unwrap();
        assert_eq!(s.ident.as_deref(), Some("script"));
        let st = sk.iter().find(|s| s.kind == "style_element").unwrap();
        assert_eq!(st.ident.as_deref(), Some("style"));
    }

    #[test]
    fn html_self_closing_element_ident_is_none() {
        // Reviewer H1: tree-sitter-html parses `<br />` as an
        // `element` whose only child is `self_closing_tag` (not
        // `start_tag`). The `extract_ident` walker hits
        // `child_by_kind("start_tag") -> None` and the skeleton
        // emits with `ident=None`. Pin so a grammar tweak that
        // renames `self_closing_tag` doesn't silently flip the
        // contract.
        let sk = extract(Language::Html, "<br />\n");
        let e = sk
            .iter()
            .find(|s| s.kind == "element")
            .expect("self-closing element must still emit a skeleton");
        assert_eq!(e.ident, None);
        assert!(
            !e.has_block,
            "self-closing element has no body — has_block must be false",
        );
    }

    #[test]
    fn html_void_element_with_no_end_tag_still_extracts_tag_name() {
        // `<img src="...">` is a void element — no end tag, but the
        // grammar emits a normal `start_tag` so `tag_name` is still
        // recoverable. Distinct from the self-closing XHTML form.
        let sk = extract(Language::Html, "<img src=\"x.png\">\n");
        let e = sk
            .iter()
            .find(|s| s.kind == "element")
            .expect("void element must emit a skeleton");
        assert_eq!(e.ident.as_deref(), Some("img"));
    }

    #[test]
    fn html_doctype_emits_zero_skeletons() {
        // `<!DOCTYPE html>` parses as `doctype`, intentionally
        // absent from the allowlist. Pin zero emission.
        let sk = extract(Language::Html, "<!DOCTYPE html>\n");
        assert!(
            !sk.iter().any(|s| s.kind == "doctype"),
            "doctype is deferred — must not emit; saw {sk:?}",
        );
    }

    #[test]
    fn html_grammar_fingerprint_is_stable_and_nonzero() {
        let a = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Html);
        let b = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Html);
        assert_eq!(a, b);
        assert_ne!(a, 0);
    }

    #[test]
    fn java_generic_method_ident_is_bare_name() {
        // Reviewer M2: generic class is tested, but generic method
        // has its own `<T>` shape (`type_parameters` sibling of
        // `name:`). Pin so a future grammar restructure can't slip.
        let sk = extract(
            Language::Java,
            "class C { <T> T identity(T x) { return x; } }\n",
        );
        let m = sk
            .iter()
            .find(|s| s.kind == "method_declaration")
            .expect("missing method_declaration");
        assert_eq!(m.ident.as_deref(), Some("identity"));
    }

    #[test]
    fn java_abstract_method_emits_with_no_block() {
        // Reviewer GAP-J1: an abstract method on an interface has
        // no `block` child — `has_block=false`. Mirrors the C++
        // `cpp_forward_class_decl_emits_skeleton_with_no_block`
        // contract: structurally identical to a definition except
        // for the body, so the only signal separating them is
        // `has_block`.
        let sk = extract(
            Language::Java,
            "interface IRunner {\n    void run();\n    void halt();\n}\n",
        );
        let methods: Vec<_> = sk
            .iter()
            .filter(|s| s.kind == "method_declaration")
            .collect();
        assert_eq!(methods.len(), 2);
        for m in &methods {
            assert!(
                !m.has_block,
                "abstract interface method must report has_block=false, \
                 got {:?} for {:?}",
                m.has_block, m.ident,
            );
        }
    }

    #[test]
    fn java_record_with_compact_constructor_emits_both_skeletons() {
        // Reviewer H1: `compact_constructor_declaration` is the
        // Java 16+ record-only form; pin both the outer record and
        // the compact constructor surface so the allowlist entry
        // stays load-bearing.
        let sk = extract(
            Language::Java,
            "public record User(String name, int age) {\n\
             \x20\x20\x20\x20public User { if (age < 0) throw new IllegalArgumentException(); }\n\
             }\n",
        );
        let r = sk
            .iter()
            .find(|s| s.kind == "record_declaration")
            .expect("missing record_declaration");
        assert_eq!(r.ident.as_deref(), Some("User"));
        let c = sk
            .iter()
            .find(|s| s.kind == "compact_constructor_declaration")
            .expect("missing compact_constructor_declaration");
        assert_eq!(c.ident.as_deref(), Some("User"));
        assert!(
            c.has_block,
            "compact constructor body is `block` — must register",
        );
    }

    #[test]
    fn java_grammar_fingerprint_is_stable_and_nonzero() {
        let a = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Java);
        let b = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Java);
        assert_eq!(a, b);
        assert_ne!(a, 0);
    }

    #[test]
    fn sql_create_function_with_custom_return_type_extracts_function_name() {
        // Reviewer H2: a custom RETURNS type produces a SECOND
        // `object_reference` child under `custom_type:`. We rely on
        // source-order first-match; pin that the function name
        // (positional) precedes the return type (`custom_type:`) so
        // `child_by_kind` resolves to `get_user`, not `user_type`.
        let sk = extract(
            Language::Sql,
            "CREATE FUNCTION get_user() RETURNS public.user_type AS $$ SELECT NULL; $$ LANGUAGE plpgsql;\n",
        );
        let f = sk.iter().find(|s| s.kind == "create_function").unwrap();
        assert_eq!(f.ident.as_deref(), Some("get_user"));
        assert!(f.has_block, "function_body must register as a body");
    }

    #[test]
    fn sql_qualified_table_ident_is_unqualified_leaf() {
        // `CREATE TABLE schema.users (...)` — the `object_reference`
        // wraps an `identifier field=schema` plus `identifier field=name`.
        // Skeleton ident is the leaf, not the dotted path.
        let sk = extract(Language::Sql, "CREATE TABLE public.users (id INT);\n");
        let t = sk.iter().find(|s| s.kind == "create_table").unwrap();
        assert_eq!(t.ident.as_deref(), Some("users"));
    }

    #[test]
    fn sql_create_table_if_not_exists_still_extracts_ident() {
        let sk = extract(
            Language::Sql,
            "CREATE TABLE IF NOT EXISTS users (id INT);\n",
        );
        let t = sk.iter().find(|s| s.kind == "create_table").unwrap();
        assert_eq!(t.ident.as_deref(), Some("users"));
    }

    #[test]
    fn sql_create_unique_temp_modifier_variants_extract_ident() {
        let unique = extract(Language::Sql, "CREATE UNIQUE INDEX idx ON t(c);\n");
        let i = unique.iter().find(|s| s.kind == "create_index").unwrap();
        assert_eq!(i.ident.as_deref(), Some("idx"));

        let temp = extract(Language::Sql, "CREATE TEMP TABLE tmp (id INT);\n");
        let t = temp.iter().find(|s| s.kind == "create_table").unwrap();
        assert_eq!(t.ident.as_deref(), Some("tmp"));
    }

    #[test]
    fn sql_alter_table_rename_ident_is_source_table_not_destination() {
        // `ALTER TABLE users RENAME TO accounts;` has TWO
        // object_reference children (source, then rename_object >
        // destination). Pin source-table wins.
        let sk = extract(Language::Sql, "ALTER TABLE users RENAME TO accounts;\n");
        let a = sk.iter().find(|s| s.kind == "alter_table").unwrap();
        assert_eq!(a.ident.as_deref(), Some("users"));
    }

    #[test]
    fn sql_materialized_view_emits_named_skeleton() {
        // tree-sitter-sequel exposes `create_materialized_view` as a
        // distinct kind; the allowlist must list it explicitly to
        // avoid silent zero-emission.
        let sk = extract(
            Language::Sql,
            "CREATE MATERIALIZED VIEW active AS SELECT 1;\n",
        );
        let v = sk
            .iter()
            .find(|s| s.kind == "create_materialized_view")
            .unwrap();
        assert_eq!(v.ident.as_deref(), Some("active"));
    }

    #[test]
    fn sql_drop_view_function_index_emit_named_skeletons() {
        // `drop_view` / `drop_function` follow the `object_reference >
        // name:` shape; `drop_index` is the odd-one-out using a
        // direct `name:` field on the statement node.
        let sk = extract(
            Language::Sql,
            "DROP VIEW old_v;\nDROP FUNCTION old_f();\nDROP INDEX old_idx;\n",
        );
        for (kind, name) in [
            ("drop_view", "old_v"),
            ("drop_function", "old_f"),
            ("drop_index", "old_idx"),
        ] {
            let s = sk
                .iter()
                .find(|s| s.kind == kind)
                .unwrap_or_else(|| panic!("missing {kind}"));
            assert_eq!(s.ident.as_deref(), Some(name), "{kind} ident");
        }
    }

    #[test]
    fn sql_create_procedure_falls_through_to_live_scan() {
        // tree-sitter-sequel has no `create_procedure` node; the
        // input parses with ERROR nodes and emits zero skeletons.
        // Pin so a future grammar update that adds the node kind
        // doesn't silently start emitting nameless skeletons.
        let sk = extract(
            Language::Sql,
            "CREATE PROCEDURE p() AS $$ SELECT 1; $$ LANGUAGE plpgsql;\n",
        );
        assert!(
            !sk.iter().any(|s| s.kind == "create_procedure"),
            "no create_procedure kind expected — grammar gap; saw: {sk:?}",
        );
    }

    #[test]
    fn sql_grammar_fingerprint_is_stable_and_nonzero() {
        let a = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Sql);
        let b = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Sql);
        assert_eq!(a, b);
        assert_ne!(a, 0);
    }

    #[test]
    fn markdown_atx_heading_extracts_heading_text() {
        let sk = extract(
            Language::Markdown,
            "# Top Title\n\n## Section One\n\n### Sub Three\n",
        );
        let headings: Vec<_> = sk.iter().filter(|s| s.kind == "atx_heading").collect();
        assert_eq!(headings.len(), 3);
        let titles: Vec<&str> = headings.iter().filter_map(|s| s.ident.as_deref()).collect();
        assert!(titles.contains(&"Top Title"));
        assert!(titles.contains(&"Section One"));
        assert!(titles.contains(&"Sub Three"));
    }

    #[test]
    fn markdown_setext_heading_extracts_underlined_title() {
        let sk = extract(
            Language::Markdown,
            "Setext H1\n=========\n\nSetext H2\n---------\n",
        );
        let setexts: Vec<_> = sk.iter().filter(|s| s.kind == "setext_heading").collect();
        assert_eq!(setexts.len(), 2);
        // The grammar wraps the title in a `paragraph` node — utf8_
        // text on `heading_content` returns the line plus a trailing
        // newline, so we trim in `extract_ident`.
        let titles: Vec<&str> = setexts.iter().filter_map(|s| s.ident.as_deref()).collect();
        assert!(titles.contains(&"Setext H1"));
        assert!(titles.contains(&"Setext H2"));
    }

    #[test]
    fn markdown_fenced_code_block_extracts_info_string_lang() {
        let sk = extract(
            Language::Markdown,
            "```rust\nfn foo() {}\n```\n\n```python\nprint('hi')\n```\n",
        );
        let blocks: Vec<_> = sk
            .iter()
            .filter(|s| s.kind == "fenced_code_block")
            .collect();
        assert_eq!(blocks.len(), 2);
        let langs: Vec<&str> = blocks.iter().filter_map(|s| s.ident.as_deref()).collect();
        assert!(langs.contains(&"rust"));
        assert!(langs.contains(&"python"));
        // Code fence body is `code_fence_content` — has_block must
        // fire so `$$$BODY` patterns can prefilter against it.
        assert!(blocks.iter().all(|s| s.has_block));
    }

    #[test]
    fn markdown_atx_heading_parent_kind_is_section() {
        // tree-sitter-markdown wraps every heading in an enclosing
        // `section` — even at the top level. `parent_kind` should
        // therefore consistently be `Some("section")`, not `None`.
        // The skeleton.rs `is_root_kind` suppression for `document`
        // is still load-bearing for any future top-level node that
        // sits directly under `document` (e.g. front-matter blocks).
        let sk = extract(Language::Markdown, "# Top\n");
        let h = sk.iter().find(|s| s.kind == "atx_heading").unwrap();
        assert_eq!(h.parent_kind, Some("section"));
    }

    #[test]
    fn markdown_atx_heading_in_blockquote_still_parents_section() {
        // `> # Quoted` nests as `document > section > block_quote >
        // section > atx_heading`. The immediate parent is still
        // `section`, so `parent_kind` cannot distinguish top-level
        // headings from blockquote-nested ones. Pin this so prefilter
        // logic doesn't accidentally start branching on it.
        let sk = extract(Language::Markdown, "> # Quoted\n");
        let h = sk.iter().find(|s| s.kind == "atx_heading").unwrap();
        assert_eq!(h.parent_kind, Some("section"));
    }

    #[test]
    fn markdown_fenced_code_block_empty_info_string_has_none_ident() {
        // ```` ``` ```` with no language tag — the `info_string`
        // child is absent / empty, the `.filter(|s| !s.is_empty())`
        // guard fires and ident comes back as `None`.
        let sk = extract(Language::Markdown, "```\njust text\n```\n");
        let b = sk.iter().find(|s| s.kind == "fenced_code_block").unwrap();
        assert_eq!(b.ident, None);
    }

    #[test]
    fn markdown_atx_heading_with_inline_markup_returns_raw_markup() {
        // `# **Bold** Title` — utf8_text on `heading_content` returns
        // the raw inline span including `**` markers. The skeleton
        // ident is for prefilter narrowing, not rendering; document
        // this contract so a future "render-strip" patch fails loudly.
        let sk = extract(Language::Markdown, "# **Bold** Title\n");
        let h = sk.iter().find(|s| s.kind == "atx_heading").unwrap();
        assert_eq!(h.ident.as_deref(), Some("**Bold** Title"));
    }

    #[test]
    fn is_root_kind_document_gated_for_markdown_and_html_but_not_yaml() {
        // Reviewer H1 (SQL/MD train): `document` must be a root kind
        // for Markdown AND HTML (both use it as the file root), but
        // NOT for YAML (where `document` is a non-root subtree
        // under `stream`). Otherwise top-level YAML nodes would
        // silently leak `parent_kind=None` once YAML rolls out.
        assert!(is_root_kind("document", Language::Markdown));
        assert!(is_root_kind("document", Language::Html));
        assert!(!is_root_kind("document", Language::Yaml));
        // Base set stays language-agnostic, including the new
        // `stylesheet` (CSS root). Pin under CSS itself plus a
        // sample of other languages so a future per-language gate
        // can't silently break the global behaviour.
        assert!(is_root_kind("source_file", Language::Rust));
        assert!(is_root_kind("compilation_unit", Language::CSharp));
        assert!(is_root_kind("stylesheet", Language::Css));
        assert!(is_root_kind("stylesheet", Language::Rust));
        // T3 languages that still have empty allowlists — verify
        // their root kinds aren't accidentally suppressed AND that
        // `stylesheet` / `document` don't trigger for them. (`bash`,
        // `lua`, `toml` all use `program` for their root, which IS
        // in the global base — that's the right behaviour.)
        assert!(is_root_kind("program", Language::Bash));
        assert!(is_root_kind("program", Language::Lua));
        assert!(!is_root_kind("document", Language::Toml));
        assert!(!is_root_kind("document", Language::Bash));
    }

    #[test]
    fn markdown_grammar_fingerprint_is_stable_and_nonzero() {
        let a = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Markdown);
        let b = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Markdown);
        assert_eq!(a, b);
        assert_ne!(a, 0);
    }

    #[test]
    fn cpp_grammar_fingerprint_is_stable_and_nonzero() {
        let a = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Cpp);
        let b = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Cpp);
        assert_eq!(a, b, "fingerprint must be deterministic");
        assert_ne!(a, 0, "zero is reserved as the not-stored sentinel");
    }

    #[test]
    fn go_grammar_fingerprint_is_stable_and_nonzero() {
        // Smoke test: the grammar fingerprint must be deterministic
        // across calls and never collide with the zero sentinel (which
        // the reader uses to signal "not stored").
        let a = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Go);
        let b = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::Go);
        assert_eq!(a, b, "fingerprint must be deterministic");
        assert_ne!(a, 0, "zero is reserved as the not-stored sentinel");
    }

    #[test]
    fn t3_language_short_circuits_to_empty() {
        // YAML is T3 — config / data formats with no useful
        // pattern-targetable shape. Markdown then CSS used to be
        // the canary here, but both moved to T2a; YAML is a stable
        // anchor that we explicitly never plan to populate.
        let sk = extract(Language::Yaml, "key: value\n");
        assert!(sk.is_empty());
    }

    #[test]
    fn malformed_source_returns_partial_skeletons_not_panic() {
        // Tree-sitter is error-tolerant — partial trees should still
        // yield whatever skeletons it could recover, never panic.
        let sk = extract(Language::Rust, "fn broken( {} fn ok() {}\n");
        // We don't pin the exact count — grammar recovery varies by
        // version. The assertion is "doesn't panic" + "we got at
        // least the well-formed declaration".
        assert!(sk.iter().any(|s| s.ident.as_deref() == Some("ok")));
    }
}
