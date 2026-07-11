//! Per-language tree-sitter SCM queries for `vex implementations`.
//!
//! The dispatch fn [`inheritance_query`] returns the SCM source for a
//! language (or `None` when implementations search isn't wired for that
//! grammar). [`relation_label`] maps a matched pattern's index back to
//! the relation kind reported in the result (`extends` / `implements`
//! / `uses` / `inherits` / `include`). The two `_PATTERN_START`
//! constants encode the index boundary between extends/implements-style
//! patterns and trait-/mixin-style ones for PHP and Ruby — bumping a
//! query without updating the constant mis-labels relations, so the
//! regression suite in `tests.rs` pins both sides.
//!
//! Isolated from the matcher so adding a language is a queries-only
//! change once the matcher's grammar coverage already handles it.

use crate::parse::language::Language;

/// Get the inheritance tree-sitter query for a language, if supported.
pub(super) fn inheritance_query(lang: Language) -> Option<&'static str> {
    match lang {
        Language::Rust => Some(
            r#"
            (impl_item
              trait: (type_identifier) @base
              type: (type_identifier) @child) @def

            ; 11.1.6: generic-parameterised trait — `impl Iterator<T> for Foo`.
            ; tree-sitter-rust wraps the trait field in `generic_type`.
            ;
            ; NOTE: scoped generic traits (`impl path::Trait<T> for Foo`)
            ; are not matched — the `type:` field becomes
            ; `scoped_identifier`, not `type_identifier`. Same gap on
            ; the `@child` side when the impl type is itself generic
            ; (`impl Trait<T> for Container<T>`).
            (impl_item
              trait: (generic_type type: (type_identifier) @base)
              type: (type_identifier) @child) @def
            "#,
        ),
        Language::Python => Some(
            r#"
            (class_definition
              name: (identifier) @child
              superclasses: (argument_list
                (identifier) @base)) @def

            ; 11.1.6: typing.Generic[T]-style parameterised base —
            ; `class IntBox(Container[int])`. The base is wrapped in a
            ; `subscript` node whose `value:` field is the identifier.
            ;
            ; NOTE: PEP 604 union bases (`Container[int] | None`) are
            ; not matched — they parse as a `binary_operator` at the
            ; argument_list level rather than a direct subscript.
            (class_definition
              name: (identifier) @child
              superclasses: (argument_list
                (subscript value: (identifier) @base))) @def
            "#,
        ),
        Language::Java => Some(
            r#"
            (class_declaration
              name: (identifier) @child
              (superclass (type_identifier) @base)) @def

            (class_declaration
              name: (identifier) @child
              (super_interfaces (type_list (type_identifier) @base))) @def

            (interface_declaration
              name: (identifier) @child
              (extends_interfaces (type_list (type_identifier) @base))) @def

            ; 11.3: generic-parameterised bases — `extends Repository<T>`,
            ; `implements Handler<T>`. Tree-sitter wraps the identifier in
            ; a `generic_type` parent when type arguments are present.
            (class_declaration
              name: (identifier) @child
              (superclass (generic_type (type_identifier) @base))) @def

            (class_declaration
              name: (identifier) @child
              (super_interfaces (type_list (generic_type (type_identifier) @base)))) @def

            (interface_declaration
              name: (identifier) @child
              (extends_interfaces (type_list (generic_type (type_identifier) @base)))) @def
            "#,
        ),
        Language::TypeScript => Some(
            r#"
            (class_declaration
              name: (type_identifier) @child
              (class_heritage
                (extends_clause
                  value: (identifier) @base))) @def

            (class_declaration
              name: (type_identifier) @child
              (class_heritage
                (implements_clause
                  (type_identifier) @base))) @def

            ; 11.1.6: generic-parameterised implements — `implements
            ; Handler<T>`. Tree-sitter-typescript wraps it in
            ; `generic_type` whose `name:` field is the identifier.
            ; (`extends Foo<T>` already works because tree-sitter keeps
            ; `value: identifier` with `type_arguments` as a sibling.)
            (class_declaration
              name: (type_identifier) @child
              (class_heritage
                (implements_clause
                  (generic_type name: (type_identifier) @base)))) @def
            "#,
        ),
        Language::CSharp => Some(
            r#"
            (class_declaration
              name: (identifier) @child
              (base_list (identifier) @base)) @def

            (class_declaration
              name: (identifier) @child
              (base_list
                (qualified_name
                  (identifier) @base))) @def

            ; 11.3: generic-parameterised bases — `: Repository<T>`,
            ; `: IRepository<T>`. The grammar wraps the identifier in
            ; `generic_name` when type arguments are present.
            ;
            ; TODO(11.4 follow-up): qualified+generic combo — `: Namespace.Repo<T>`
            ; — needs a `(qualified_name ... (generic_name (identifier)))`
            ; pattern. Common in EF Core / DI-heavy .NET codebases.
            (class_declaration
              name: (identifier) @child
              (base_list (generic_name (identifier) @base))) @def
            "#,
        ),
        Language::Swift => Some(
            r#"
            (class_declaration
              name: (type_identifier) @child
              (inheritance_specifier
                (user_type (type_identifier) @base))) @def

            (protocol_declaration
              name: (type_identifier) @child
              (inheritance_specifier
                (user_type (type_identifier) @base))) @def
            "#,
        ),
        Language::Kotlin => Some(
            // tree-sitter-kotlin-ng node names verified via AST dump:
            // class name is `identifier` (not `type_identifier`); the
            // delegation list is `delegation_specifiers` (not
            // `_list`); generic args live as a sibling
            // `type_arguments` of `identifier` inside `user_type`,
            // so the bare `(user_type (identifier))` pattern matches
            // both plain and generic bases.
            r#"
            (class_declaration
              (identifier) @child
              (delegation_specifiers
                (delegation_specifier
                  (user_type (identifier) @base)))) @def

            ; Superclass call: `class Foo : Bar()` or `class Foo : Repository<T>()`.
            (class_declaration
              (identifier) @child
              (delegation_specifiers
                (delegation_specifier
                  (constructor_invocation
                    (user_type (identifier) @base))))) @def
            "#,
        ),
        // Go has implicit interfaces, Ruby has mixins — skip for now
        Language::Cpp => Some(
            r#"
            (class_specifier
              name: (type_identifier) @child
              (base_class_clause
                (type_identifier) @base)) @def

            (struct_specifier
              name: (type_identifier) @child
              (base_class_clause
                (type_identifier) @base)) @def

            ; 11.1.6: template-parameterised base — `: public Foo<T>`.
            ; Tree-sitter-cpp wraps the identifier in `template_type`.
            (class_specifier
              name: (type_identifier) @child
              (base_class_clause
                (template_type name: (type_identifier) @base))) @def

            (struct_specifier
              name: (type_identifier) @child
              (base_class_clause
                (template_type name: (type_identifier) @base))) @def
            "#,
        ),
        // PHP: `class Foo extends Bar implements I1, I2`, plus
        // `interface ChildI extends ParentI`, plus trait composition via
        // `use TraitName;` inside a class or trait body. Base types can
        // be a bare name or a qualified name (`App\Service\Foo`); we
        // capture the trailing `name` node in either form.
        //
        // Pattern order matters: patterns 0..=5 are extends/implements
        // (label "extends"), patterns 6..=9 are trait `use` (label
        // "uses"). `php_relation_label` below dispatches on
        // `m.pattern_index` accordingly — reorder with care.
        Language::Php => Some(
            r#"
            (class_declaration
              name: (name) @child
              (base_clause (name) @base)) @def

            (class_declaration
              name: (name) @child
              (base_clause (qualified_name (name) @base))) @def

            (class_declaration
              name: (name) @child
              (class_interface_clause (name) @base)) @def

            (class_declaration
              name: (name) @child
              (class_interface_clause (qualified_name (name) @base))) @def

            (interface_declaration
              name: (name) @child
              (base_clause (name) @base)) @def

            (interface_declaration
              name: (name) @child
              (base_clause (qualified_name (name) @base))) @def

            (enum_declaration
              name: (name) @child
              (class_interface_clause (name) @base)) @def

            (enum_declaration
              name: (name) @child
              (class_interface_clause (qualified_name (name) @base))) @def

            (class_declaration
              name: (name) @child
              body: (declaration_list
                (use_declaration (name) @base))) @def

            (class_declaration
              name: (name) @child
              body: (declaration_list
                (use_declaration (qualified_name (name) @base)))) @def

            (trait_declaration
              name: (name) @child
              body: (declaration_list
                (use_declaration (name) @base))) @def

            (trait_declaration
              name: (name) @child
              body: (declaration_list
                (use_declaration (qualified_name (name) @base)))) @def
            "#,
        ),
        // Ruby: `class Foo < Bar` for single inheritance, and
        // `include Mixin` / `extend Mixin` / `prepend Mixin` inside class
        // or module bodies for mixin composition. The mixin call appears
        // as a normal `call` node (no implicit receiver) in
        // tree-sitter-ruby.
        //
        // The `#match?` predicate MUST sit inside the enclosing pattern's
        // S-expression — otherwise tree-sitter silently treats it as a
        // standalone (no-op) pattern and the query degrades to "any
        // method call inside the class body with a Constant argument",
        // which would match noise like `assert_equal Foo, bar.class`.
        Language::Ruby => Some(
            r#"
            (class
              name: (constant) @child
              superclass: (superclass (constant) @base)) @def

            (class
              name: (constant) @child
              body: (body_statement
                (call
                  method: (identifier) @_m
                  arguments: (argument_list (constant) @base)))
              (#match? @_m "^(include|extend|prepend)$")) @def

            (module
              name: (constant) @child
              body: (body_statement
                (call
                  method: (identifier) @_m
                  arguments: (argument_list (constant) @base)))
              (#match? @_m "^(include|extend|prepend)$")) @def
            "#,
        ),
        // Languages without class-hierarchy semantics in their grammar.
        // Go uses structural typing (no declared base list), SQL /
        // Markdown / config formats have no classes, Bash and Lua are
        // not OO.
        Language::Go
        | Language::Sql
        | Language::Markdown
        | Language::Bash
        | Language::Lua
        | Language::Css
        | Language::Html
        | Language::Yaml
        | Language::Toml => None,
    }
}

/// First pattern index in PHP's [`inheritance_query`] that denotes a
/// trait `use` (rather than class extends / class implements / interface
/// extends / enum implements). Patterns 0..[`PHP_TRAIT_PATTERN_START`) →
/// `"extends"`, patterns [`PHP_TRAIT_PATTERN_START`).. → `"uses"`.
///
/// Layout (keep in sync with the query string):
/// - 0..=1: `class extends` (bare + qualified)
/// - 2..=3: `class implements` (bare + qualified)
/// - 4..=5: `interface extends` (bare + qualified)
/// - 6..=7: `enum implements` (bare + qualified, PHP 8.1+)
/// - 8..=9: `class { use Trait }` (bare + qualified)
/// - 10..=11: `trait { use OtherTrait }` (bare + qualified)
///
/// Adding a new extends / implements pattern requires bumping this
/// constant, otherwise the new pattern will be mislabelled as `"uses"`.
/// Symmetric regression tests `php_extends_stays_extends_not_uses` and
/// `php_trait_uses_relation_is_uses_not_extends` guard the boundary
/// from both sides.
pub(super) const PHP_TRAIT_PATTERN_START: usize = 8;

/// First pattern index in Ruby's [`inheritance_query`] that denotes a
/// mixin (`include` / `extend` / `prepend`) rather than `class < Bar`.
/// Pattern 0 is the superclass form → `"inherits"`; patterns 1.. are
/// mixin calls → `"include"`.
pub(super) const RUBY_MIXIN_PATTERN_START: usize = 1;

/// Relation label for a language + matched query pattern.
///
/// Most languages return a single label per language (Java/C# lump
/// `extends` and `implements` under `"extends"`, mirroring user-visible
/// source). PHP and Ruby dispatch on [`tree_sitter::QueryMatch::pattern_index`]
/// to surface the semantic difference between inheritance and mixin /
/// trait composition.
///
/// **Invariant:** every language that returns `Some(_)` from
/// [`inheritance_query`] must have a real arm here. The catch-all
/// `"extends"` fallback is reachable in practice only for Cpp / Python /
/// TypeScript; the remaining languages (Go, Sql, Markdown, Bash, Lua,
/// Css, Html, Yaml, Toml) currently return `None` from `inheritance_query`
/// so `relation_label` is never called for them — they're listed in the
/// arm only because `Language` is non-exhaustive at the match site and a
/// stray future caller would otherwise hit a wildcard with no label.
///
/// **Coupling:** `extract::relation_to_edge_kind` maps these strings to
/// `EdgeKind` discriminants. Adding a new label here (e.g. splitting a real
/// `"implements"` out of `"extends"`) requires updating that mapping in
/// lockstep — the `every_known_relation_label_maps_to_intended_kind` test in
/// `extract.rs` is the tripwire that fails if the two drift apart.
pub(super) fn relation_label(lang: Language, pattern_index: usize) -> &'static str {
    match lang {
        Language::Rust => "impl",
        Language::Ruby if pattern_index >= RUBY_MIXIN_PATTERN_START => "include",
        Language::Ruby => "inherits",
        Language::Php if pattern_index >= PHP_TRAIT_PATTERN_START => "uses",
        Language::Php => "extends",
        Language::Java | Language::CSharp | Language::Kotlin | Language::Swift => "extends",
        Language::Cpp
        | Language::Python
        | Language::TypeScript
        | Language::Go
        | Language::Sql
        | Language::Markdown
        | Language::Bash
        | Language::Lua
        | Language::Css
        | Language::Html
        | Language::Yaml
        | Language::Toml => "extends",
    }
}
