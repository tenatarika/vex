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
//! | T2a  | Go                                               | populated     |
//! | T2   | Java, Kotlin, C#, C++, Swift, PHP, Ruby          | empty for now |
//! | T3   | SQL, Markdown, CSS, HTML, YAML, TOML, Bash, Lua  | empty (final) |
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
        let parent_kind = node.parent().map(|p| p.kind()).filter(|k| !is_root_kind(k));
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

fn is_root_kind(kind: &str) -> bool {
    matches!(
        kind,
        "source_file" | "program" | "module" | "translation_unit"
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
    );
    if anonymous {
        return None;
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

/// Block-shaped body markers, language-agnostic. Tree-sitter uses these
/// kinds for the statement-list child of declarations across grammars.
fn has_body_block(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            // Universal markers across T1 grammars. Add as T2/T3
            // languages reveal new body-shaped kinds.
            "block"
                | "statement_block"
                | "declaration_list"
                | "field_declaration_list"
                | "enum_body"
                | "enum_variant_list"
                | "class_body"
                | "interface_body"
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
        // Java is still T2 — not yet in the allowlist. Go used to be
        // the canary here; it moved to T2a (populated) so swap to
        // Java to keep the empty-allowlist short-circuit covered.
        // When Java itself rolls out next, repoint at a T3 lang that
        // we explicitly don't plan to populate (e.g. `Language::Css`
        // or `Language::Yaml`) so the canary doesn't keep shifting.
        let sk = extract(Language::Java, "class Foo {}\n");
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
        // Markdown is T3 — never gets skeletons.
        let sk = extract(Language::Markdown, "# Heading\n");
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
