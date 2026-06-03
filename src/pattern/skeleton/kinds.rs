//! T1/T2a allowlist of pattern-targetable tree-sitter node kinds per
//! language. The dispatch fn [`pattern_targetable_kinds`] returns the
//! `&'static` slice for the requested language; T2/T3 languages return
//! an empty slice. See the parent [`super`] module docs for the tier table.
//!
//! Isolated from the walker so adding a new language requires only
//! extending the `match` here — no walker changes. Ident-extraction
//! quirks live in the sibling `ident` module for the same reason.

use crate::parse::language::Language;

/// T1 allowlist. T2/T3 languages return an empty slice — see module docs.
pub(super) fn pattern_targetable_kinds(lang: Language) -> &'static [&'static str] {
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
        Language::Kotlin => &[
            // Type declarations. `class_declaration` is the umbrella
            // for `class`, `interface`, `data class`, `enum class`,
            // and `sealed class` — the distinguishing keyword
            // (`class` / `interface` / `enum class`) is a literal
            // anonymous child of the node, not part of the kind. A
            // pattern like `interface $NAME` still narrows to this
            // kind and matches via source-text at the live-scan
            // stage, so the prefilter remains correct.
            "class_declaration",
            // `object Foo { ... }` singletons and the named/anonymous
            // `companion object { ... }` form. `companion_object`'s
            // `name:` field is optional — anonymous companions emit
            // with `ident=None`, which is the user-intuitive result.
            "object_declaration",
            "companion_object",
            // Functions — top-level, member, and abstract (no body)
            // all parse as `function_declaration` with `name:
            // identifier`. Abstract methods on interfaces lack a
            // `function_body` child, so `has_block=false` separates
            // them from concrete definitions (mirrors the Java
            // `abstract_method` and C++ forward-decl contract).
            "function_declaration",
            // `val x = 1` / `var y: Int`. Identifier lives under
            // `variable_declaration > identifier` (no `name:` field
            // on the property itself) — see `extract_ident` for the
            // child-walk. Destructuring forms
            // (`val (a, b) = pair`) use `multi_variable_declaration`
            // and fall through to `ident=None` for now; revisit if
            // patterns need them.
            "property_declaration",
            // `typealias Foo = Bar` — the alias name lives in the
            // `type:` field (NOT `name:`). Same shape as the Rust
            // `impl_item` exception in `extract_ident`.
            "type_alias",
            // `constructor(x: Int) : this(x, 0) { ... }` — no
            // recoverable name on the secondary constructor (the
            // `constructor` keyword is its identity). Anonymous,
            // body is a plain `block`.
            "secondary_constructor",
            // `enum class Mode { FAST, SLOW }` — each entry parses
            // as `enum_entry` with a positional `identifier` child
            // (no `name:` field). Optional `class_body` for entries
            // with overrides triggers `has_block=true`.
            "enum_entry",
            // `init { ... }` instance initializer — anonymous,
            // block body.
            "anonymous_initializer",
            // Anonymous callables: `{ x -> x + 1 }` lambdas and the
            // older `fun(x) { ... }` anonymous-function form.
            // `lambda_literal` wraps statements directly (no `block`
            // child) → `has_block=false`. `anonymous_function`
            // carries a proper `function_body` → `has_block=true`.
            // (Swift reuses the same `lambda_literal` kind name with
            // the same body-as-statements shape — see Swift arm below.)
            "lambda_literal",
            "anonymous_function",
            // Intentionally absent (deferred / out of scope):
            //   * `primary_constructor` — anonymous wrapper whose
            //     identity is the enclosing class name; nothing
            //     extra to surface via a separate skeleton.
            //   * `getter` / `setter` — anonymous accessors that
            //     live under `property_declaration`; niche surface
            //     for `vex pattern`.
            //   * `variable_declaration` / `multi_variable_declaration`
            //     — too granular; covered by `property_declaration`.
            //   * `import` / `package_header` — binding sites, not
            //     pattern targets.
        ],
        Language::Swift => &[
            // Type declarations. `class_declaration` is the umbrella
            // for `class`, `struct`, `enum`, `actor`, and `extension`
            // — distinguished by the named `declaration_kind` field
            // (anonymous keyword child). A pattern like
            // `struct $NAME` narrows by kind + source-text. For
            // `extension`, the `name:` field is the type BEING
            // extended (e.g. `Foo` in `extension Foo { ... }`) —
            // intuitive surface for `vex pattern`.
            "class_declaration",
            // `protocol Foo { ... }` — separate kind from class
            // umbrella, with its own `protocol_body`.
            "protocol_declaration",
            // Top-level + member functions. Body lives in a required
            // `function_body` field — reuses the SQL/Kotlin arm of
            // `has_body_block`. Concrete fns always have a body;
            // abstract protocol fns parse as
            // `protocol_function_declaration` instead.
            "function_declaration",
            // `var x = 1` / `let y: Int`. The `name:` field is a
            // `pattern` AST node, NOT a simple identifier — for
            // `var x: Int` the pattern text is `x`, for the
            // destructuring form `let (a, b) = pair` it's `(a, b)`.
            // Default text extraction returns whichever form the
            // user wrote; a future arm can post-process if needed.
            "property_declaration",
            // `typealias Foo = Bar` — has a proper `name:` field;
            // default extraction works (UNLIKE Kotlin, where the
            // alias name lives under `type:`).
            "typealias_declaration",
            // Enum cases: `case foo`, `case bar(Int)`. `name:` is a
            // simple identifier; default extraction works.
            "enum_entry",
            // `associatedtype Element` — protocol-level type slot,
            // has `name:` field.
            "associatedtype_declaration",
            // Protocol method/property signatures — body-less
            // requirements that live under `protocol_body`. Both
            // expose `name:`; `has_block=false` is the natural
            // outcome of the body-less shape.
            "protocol_function_declaration",
            "protocol_property_declaration",
            // Anonymous member declarations. None expose a
            // recoverable name (Swift identifies them by keyword:
            // `init`, `deinit`, `subscript`). All carry a
            // `function_body` (init/deinit) or wrap their body in
            // `computed_property` (subscript — has_block=false at
            // the skeleton's direct-child level).
            "init_declaration",
            "deinit_declaration",
            "subscript_declaration",
            // Custom operator definition — `infix operator +++: ...`.
            // No `name:` field; the operator token lives under
            // `custom_operator`. Anonymous at the skeleton level.
            "operator_declaration",
            // Anonymous closures: `{ x in x + 1 }`. The body lives
            // in a positional `statements` child, NOT a named
            // block — has_block=false. Mirrors the Kotlin
            // `lambda_literal` contract.
            "lambda_literal",
            // Intentionally absent (deferred / out of scope):
            //   * `precedence_group_declaration` — niche operator
            //     glue; not a pattern target users ask for.
            //   * `macro_declaration` / `macro_definition` /
            //     `external_macro_definition` — Swift 5.9+ macros;
            //     low-volume surface, revisit if patterns need them.
            //   * `computed_property` / `computed_getter` /
            //     `computed_setter` / `willset_didset_block` —
            //     accessor internals, live under `property_declaration`.
            //   * `import_declaration` — binding site, not a target.
        ],
        Language::Php => &[
            // Type declarations — all carry `name: name` (the
            // tree-sitter-php grammar calls its identifier kind
            // `name`, not `identifier`). Bodies live in
            // `declaration_list` (class/interface/trait) or
            // `enum_declaration_list` (enum) — the latter is the
            // PHP-specific arm in `has_body_block`.
            "class_declaration",
            "interface_declaration",
            "trait_declaration",
            "enum_declaration",
            // Top-level functions vs methods — different kind names
            // in tree-sitter-php (unlike e.g. C# where both share
            // `method_declaration`). Both carry `name:` + optional
            // `body: compound_statement`. Abstract method on an
            // interface / `abstract` class has no body — same
            // has_block=false signal as Java/Kotlin abstract fns.
            "function_definition",
            "method_declaration",
            // PHP property / constant declarations are TWO levels
            // deep: the wrapper (`property_declaration` /
            // `const_declaration`) carries modifiers but NO `name:`
            // field — the actual names live one level down on
            // `property_element` / `const_element`. We allowlist
            // the GRANULAR elements only (mirrors the existing
            // SymbolKind extraction in `queries/php.scm`):
            //   * Multi-declarator forms (`public $a, $b, $c;`)
            //     emit one skeleton per element with the correct
            //     name (avoids the C++/C#/Java "only-first-name"
            //     gap).
            //   * `parent_kind` carries the wrapper info
            //     (`Some("property_declaration")` /
            //     `Some("const_declaration")`) so prefilters can
            //     still narrow on the declaration site.
            "property_element",
            "const_element",
            // Enum cases (PHP 8.1+): `case Active;` / backed
            // `case Active = 1;`. `name:` field is a `name`.
            "enum_case",
            // `namespace Foo { ... }` (block form) and
            // `namespace Foo;` (semicolon form). The block form
            // carries a `compound_statement` body — already in the
            // universal markers — so `has_block=true`. The
            // semicolon form has no body field → `has_block=false`,
            // which separates the two forms naturally.
            "namespace_definition",
            // Anonymous callables / classes. None expose a `name:`
            // field — all three go through the anonymous-list arm
            // of `extract_ident`. `arrow_function`'s `body:` is an
            // `expression`, NOT a block — `has_block=false` mirrors
            // the Kotlin/Swift lambda contract.
            "anonymous_function",
            "arrow_function",
            "anonymous_class",
            // Intentionally absent (deferred / out of scope):
            //   * `property_declaration` / `const_declaration` —
            //     wrapper nodes whose `name:` field doesn't exist;
            //     would emit `ident=None` skeletons that the
            //     granular `*_element` allowlist already covers
            //     with proper names.
            //   * `namespace_use_clause` / `namespace_use_declaration`
            //     — binding sites, not pattern targets.
            //   * `function_static_declaration` / `global_declaration`
            //     — niche; revisit if patterns need them.
            //   * `static_variable_declaration` — too granular.
            //   * `declare_statement` (`declare(strict_types=1);`)
            //     — directive, not a definition.
        ],
        Language::Ruby => &[
            // Type-ish declarations. Ruby has no separate
            // interface/struct kinds — modules serve dual duty as
            // mixins AND namespaces. Both `class` and `module` carry
            // `name: constant` (e.g. `Foo`) and an optional
            // `body: body_statement`. `body_statement` is the
            // Ruby-specific arm in `has_body_block`.
            "class",
            "module",
            // Instance methods (`def foo; end`) and class/module
            // methods (`def self.foo; end`). Both have a `name:`
            // field; for singleton_method the `object:` field
            // carries the receiver (typically `self`) — the
            // skeleton ident is the method name only, which is the
            // user-intuitive surface (`vex pattern 'def $NAME'`).
            //
            // Operator methods (`def ==(other)`), setter methods
            // (`def name=(v)`) all flow through `name:` whose text
            // includes the trailing token (`==`, `name=`) — pinned
            // by the reviewer-gap test.
            "method",
            "singleton_method",
            // `class << self; ...; end` — anonymous singleton-class
            // block. `value:` field is the receiver; no `name:`.
            "singleton_class",
            // `alias new_name old_name` — tree-sitter-ruby exposes
            // BOTH `name:` (the new alias being created) AND
            // `alias:` (the original target). The generic
            // `extract_ident` path calls
            // `child_by_field_name("name")` deterministically, so
            // the skeleton ident is always the NEW alias name —
            // the `alias:` field is not surfaced. If a future
            // grammar version renames or reorders `name:`, the
            // happy-path test fails loudly.
            "alias",
            // Anonymous callables. Three forms:
            //   * `lambda` — `->{ }` / `->(x) { x + 1 }` literal.
            //   * `block` — `{ |x| ... }` brace-delimited block
            //     passed to a method call.
            //   * `do_block` — `do |x| ... end` keyword-delimited
            //     block, semantically identical to `block`.
            // All three carry a `body:` field (`body_statement` for
            // `do_block`/`block`, similar for `lambda`) so
            // `has_block=true` mirrors Kotlin `anonymous_function`.
            // Volume note: Ruby idioms use blocks heavily (every
            // `each`/`map`/`tap` call carries one), so these
            // contribute the most skeleton volume of any T2a
            // language. Accepted as the cost of pattern-targeting
            // DSL-style code (RSpec `describe`, Rails `validates`,
            // etc).
            "lambda",
            "block",
            "do_block",
            // Intentionally absent (deferred / out of scope):
            //   * `assignment` — too generic; every `x = 1` would
            //     emit. Pattern targeting via this kind is better
            //     served by source-text scan.
            //   * `call` — DSL constructs like `attr_accessor :foo`
            //     and `validates :email` are method calls, not
            //     definitions — niche, would explode skeleton
            //     volume.
            //   * `begin_block` / `end_block` (`BEGIN { }` /
            //     `END { }`) — top-level hooks; niche.
            //   * Constant assignments (`FOO = 1`) parse as
            //     `assignment`, not a distinct kind; excluded for
            //     the same volume reason as `assignment`.
        ],
        _ => &[],
    }
}
