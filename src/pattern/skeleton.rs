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
//! | T2a  | Go, C++, C#                                      | populated     |
//! | T2   | Java, Kotlin, Swift, PHP, Ruby                   | empty for now |
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
            //   * `event_declaration`, `operator_declaration`,
            //     `conversion_operator_declaration` — niche; revisit
            //     if patterns need them.
            //   * `using_directive` / `extern_alias_directive` —
            //     binding sites, not pattern targets.
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
    );
    if anonymous {
        return None;
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
    fn csharp_grammar_fingerprint_is_stable_and_nonzero() {
        let a = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::CSharp);
        let b = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(Language::CSharp);
        assert_eq!(a, b, "fingerprint must be deterministic");
        assert_ne!(a, 0, "zero is reserved as the not-stored sentinel");
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
