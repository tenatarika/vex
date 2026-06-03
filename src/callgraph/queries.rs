//! Per-language tree-sitter SCM `Query` sources for callgraph extraction.
//!
//! Each language returns either a multi-pattern query string (captures
//! `fn.decl`/`fn.name` for function definitions and `call.name` /
//! `module_call.name` for call sites) or `None` when callgraph extraction
//! isn't yet wired for that grammar. The dispatch fn is the single source
//! of truth for which languages contribute to the persistent call graph.
//!
//! Isolated from `extractor` so adding a language requires only a new
//! match arm here plus registration in `extractor::COMPILED_QUERIES` —
//! walker logic stays untouched. The Phase 14.6 `module_call.name`
//! capture cooperates with `extractor::MODULE_CALL_CAPTURE`.

use crate::parse::language::Language;

pub(super) fn callgraph_query(lang: Language) -> Option<&'static str> {
    match lang {
        Language::Rust => Some(
            r#"
            (function_item name: (identifier) @fn.name) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (scoped_identifier
                name: (identifier) @call.name))

            (call_expression
              function: (field_expression
                field: (field_identifier) @call.name))

            ; Phase 14.2.1 — sibling-adjacency attribute edges.
            ; `attribute_item` is a sibling of `function_item` under
            ; `source_file` (top-level) or `declaration_list` (inside
            ; an `impl` block). The host is captured with `@sibling.host`
            ; so the extractor can walk forward to find the next
            ; function_item sibling and remap the call's byte_offset.
            ; The path's rightmost identifier is captured as
            ; `@sibling.target` (the callee name). Single-identifier
            ; paths and `scoped_identifier` paths are handled by
            ; separate patterns; for scoped paths the trailing `.`
            ; anchor inside `(scoped_identifier (identifier) @x .)`
            ; locks rightmost-wins (same trick as Kotlin user_type).

            ; top-level: #[wasm_bindgen] / #[serde(...)] / #[allow(...)]
            (source_file
              (attribute_item
                (attribute (identifier) @sibling.target)) @sibling.host)

            ; top-level: #[tokio::test] etc. — scoped path, rightmost wins
            (source_file
              (attribute_item
                (attribute (scoped_identifier
                  (identifier) @sibling.target .))) @sibling.host)

            ; impl method: #[wasm_bindgen] fn ... inside impl block
            (declaration_list
              (attribute_item
                (attribute (identifier) @sibling.target)) @sibling.host)

            ; impl method: #[tokio::test] fn ... inside impl block
            (declaration_list
              (attribute_item
                (attribute (scoped_identifier
                  (identifier) @sibling.target .))) @sibling.host)
            "#,
        ),
        Language::Python => Some(
            r#"
            (function_definition name: (identifier) @fn.name) @fn.decl

            (call function: (identifier) @call.name)

            (call function: (attribute
              attribute: (identifier) @call.name))

            ; Phase 14.2 — decorator edges. `@fn.decl` captures the OUTER
            ; `decorated_definition` so the decorator call site (which
            ; lives outside the inner function_definition byte range)
            ; attributes to the wrapped function via `min_by_key`. The
            ; existing `function_definition` pattern also fires for the
            ; inner span — the smaller range wins for in-body calls.
            ; Callee = rightmost identifier (consistent with method calls).

            ; @app.get("/x") — call with attribute target
            (decorated_definition
              (decorator
                (call function: (attribute
                  attribute: (identifier) @call.name)))
              definition: (function_definition
                name: (identifier) @fn.name)) @fn.decl

            ; @login_required() — call with bare-identifier target
            (decorated_definition
              (decorator
                (call function: (identifier) @call.name))
              definition: (function_definition
                name: (identifier) @fn.name)) @fn.decl

            ; @login_required — bare identifier, no parens
            (decorated_definition
              (decorator (identifier) @call.name)
              definition: (function_definition
                name: (identifier) @fn.name)) @fn.decl

            ; @app.router — bare attribute, no parens
            (decorated_definition
              (decorator
                (attribute attribute: (identifier) @call.name))
              definition: (function_definition
                name: (identifier) @fn.name)) @fn.decl

            ; Phase 14.6 — class-level decorator edges. Classes are not
            ; FnDef symbols today, so we deliberately do NOT capture
            ; `@fn.name` / `@fn.decl` here; the decorator's call site
            ; lies outside every fn's byte range and falls to Phase 14.1's
            ; synthetic `<module:path>` caller via `caller_fn_name=""` +
            ; `caller_fn_line=0`. Call-shape decorators (`@app.get("/x")
            ; class Foo:`, `@Component() class Foo:`) are already caught by
            ; the generic `(call function: ...)` patterns above; the only
            ; bare cases this section handles are bare-identifier and
            ; bare-attribute decorators (`@dataclass class Foo:`,
            ; `@routes.cbv class Foo:`).

            ; @dataclass class Foo: — bare identifier on a class
            (decorated_definition
              (decorator (identifier) @module_call.name)
              definition: (class_definition))

            ; @app.router class Foo: — bare attribute on a class
            (decorated_definition
              (decorator
                (attribute attribute: (identifier) @module_call.name))
              definition: (class_definition))
            "#,
        ),
        Language::Java => Some(
            r#"
            (method_declaration name: (identifier) @fn.name) @fn.decl
            (constructor_declaration name: (identifier) @fn.name) @fn.decl

            (method_invocation name: (identifier) @call.name)

            ; Phase 14.2 — annotation edges. `@fn.decl` is the
            ; method_declaration itself: the `modifiers` child (which
            ; carries the annotations) is already INSIDE the method's
            ; byte range, so the inner-fn attribution works without a
            ; wider capture. Callee = rightmost identifier of the
            ; annotation name (consistent with method-call convention).

            ; @Override / @Deprecated — marker_annotation, bare identifier
            (method_declaration
              (modifiers (marker_annotation name: (identifier) @call.name))
              name: (identifier) @fn.name) @fn.decl

            ; @org.junit.Test — marker_annotation with scoped name (rightmost)
            (method_declaration
              (modifiers (marker_annotation name: (scoped_identifier
                name: (identifier) @call.name)))
              name: (identifier) @fn.name) @fn.decl

            ; @GetMapping("/x") — annotation with arguments, bare name
            (method_declaration
              (modifiers (annotation name: (identifier) @call.name))
              name: (identifier) @fn.name) @fn.decl

            ; @org.springframework.web.bind.annotation.GetMapping(...) —
            ; annotation with arguments + scoped name (rightmost)
            (method_declaration
              (modifiers (annotation name: (scoped_identifier
                name: (identifier) @call.name)))
              name: (identifier) @fn.name) @fn.decl

            ; Phase 14.6 — class-level annotation edges. Same module-scope
            ; attribution convention as Python (Phase 14.6): no `@fn.name`
            ; / `@fn.decl` capture → no enclosing FnDef → Phase 14.1
            ; sentinel rewrites to `<module:path>`.

            ; @Component class Foo {} — bare marker_annotation
            (class_declaration
              (modifiers (marker_annotation name: (identifier) @module_call.name)))

            ; @org.springframework.Component class Foo {} — scoped marker
            (class_declaration
              (modifiers (marker_annotation name: (scoped_identifier
                name: (identifier) @module_call.name))))

            ; @Component("x") class Foo {} — bare annotation with args
            (class_declaration
              (modifiers (annotation name: (identifier) @module_call.name)))

            ; @org.springframework.Component("x") class Foo {} — scoped
            (class_declaration
              (modifiers (annotation name: (scoped_identifier
                name: (identifier) @module_call.name))))
            "#,
        ),
        Language::TypeScript => Some(
            r#"
            (function_declaration name: (identifier) @fn.name) @fn.decl
            (method_definition name: (property_identifier) @fn.name) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (member_expression
                property: (property_identifier) @call.name))

            ; Phase 14.2.1 — sibling-adjacency decorator edges.
            ; Method-level decorators live under `class_body` as siblings
            ; of `method_definition`. Class-level decorators live under
            ; `class_declaration` and are intentionally NOT matched —
            ; class-level decorators are Phase 14.6 territory.
            ; `@sibling.host` is the decorator node; extractor walks
            ; forward to find the next method_definition sibling and
            ; remaps the call's byte_offset so existing FnDef attribution
            ; works. `@sibling.target` is the path's rightmost identifier.

            ; @bound method() — bare identifier
            (class_body
              (decorator (identifier) @sibling.target) @sibling.host)

            ; @d1() method() — call with bare identifier
            (class_body
              (decorator
                (call_expression
                  function: (identifier) @sibling.target)) @sibling.host)

            ; @nest.Get("/x") method() — call with member_expression,
            ; rightmost property_identifier wins
            (class_body
              (decorator
                (call_expression
                  function: (member_expression
                    property: (property_identifier) @sibling.target))) @sibling.host)

            ; Phase 14.6 — class-level decorator edges. Decorators that
            ; sit on the class itself (outside `class_body`) live as
            ; direct children of `class_declaration` in tree-sitter-
            ; typescript. No `@fn.name` / `@fn.decl` capture →
            ; attribution falls to the module-scope sentinel.

            ; @Component class Foo {} — bare identifier
            (class_declaration
              (decorator (identifier) @module_call.name))

            ; @inject() class Foo {} — call with bare identifier
            (class_declaration
              (decorator
                (call_expression
                  function: (identifier) @module_call.name)))

            ; @nest.Module() class Foo {} — call with member_expression
            (class_declaration
              (decorator
                (call_expression
                  function: (member_expression
                    property: (property_identifier) @module_call.name))))
            "#,
        ),
        Language::Go => Some(
            r#"
            (function_declaration name: (identifier) @fn.name) @fn.decl
            (method_declaration name: (field_identifier) @fn.name) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (selector_expression
                field: (field_identifier) @call.name))
            "#,
        ),
        Language::Cpp => Some(
            r#"
            (function_definition
              declarator: (function_declarator
                declarator: (identifier) @fn.name)) @fn.decl

            (function_definition
              declarator: (function_declarator
                declarator: (qualified_identifier
                  name: (identifier) @fn.name))) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (qualified_identifier
                name: (identifier) @call.name))

            (call_expression
              function: (field_expression
                field: (field_identifier) @call.name))
            "#,
        ),
        Language::Kotlin => Some(
            r#"
            ; Function declaration (Phase 14.2.2).
            ; NOTE: `init { ... }` blocks, `getter`/`setter` accessors, and
            ; lambda invocations are intentionally NOT indexed as FnDef.
            ; Calls from those sites fall to the Phase 14.1 synthetic
            ; `<module:path>` caller — documented in docs/LIMITATIONS.md.
            (function_declaration name: (identifier) @fn.name) @fn.decl

            ; Bare call: foo()
            (call_expression (identifier) @call.name)

            ; Member access call: obj.method() — trailing identifier wins.
            ; navigation_expression has two `identifier` children separated
            ; by a literal `.` token; the SECOND identifier is the callee.
            (call_expression
              (navigation_expression
                (identifier)
                (identifier) @call.name))

            ; Annotation edges (Phase 14.2.2).
            ; @JvmStatic — bare type. tree-sitter-kotlin-ng uses
            ; `identifier` (not `type_identifier`) inside `user_type`.
            ; For qualified annotations like @kotlin.jvm.JvmStatic the
            ; user_type contains multiple identifiers separated by `.`
            ; tokens; the trailing `.` anchor matches only the LAST
            ; named child (rightmost wins).
            (function_declaration
              (modifiers (annotation
                (user_type (identifier) @call.name .)))
              name: (identifier) @fn.name) @fn.decl

            ; @Named("svc") — constructor_invocation (annotation with args).
            ; `constructor_invocation` has no fields; the first child is a
            ; `user_type` (concrete subtype of the `type` supertype).
            (function_declaration
              (modifiers (annotation
                (constructor_invocation
                  (user_type (identifier) @call.name .))))
              name: (identifier) @fn.name) @fn.decl

            ; Phase 14.6 — class-level annotation edges. Kotlin's
            ; `class_declaration` carries `modifiers` like
            ; `function_declaration`. No `@fn.name` / `@fn.decl` capture
            ; → attribution falls to the module-scope sentinel.

            ; @JvmStatic class Foo — bare marker (rightmost-wins)
            (class_declaration
              (modifiers (annotation
                (user_type (identifier) @module_call.name .))))

            ; @Named("svc") class Foo — constructor_invocation form
            (class_declaration
              (modifiers (annotation
                (constructor_invocation
                  (user_type (identifier) @module_call.name .)))))
            "#,
        ),
        Language::CSharp => Some(
            r#"
            ; Method + constructor declarations (Phase 14.2.2).
            ; NOTE: property accessors (`get =>`, `set { ... }`), local
            ; functions, indexer / event accessors, and lambda invocations
            ; are intentionally NOT indexed as FnDef. Calls from those
            ; sites fall to the Phase 14.1 synthetic `<module:path>`
            ; caller — documented in docs/LIMITATIONS.md.
            (method_declaration name: (identifier) @fn.name) @fn.decl
            (constructor_declaration name: (identifier) @fn.name) @fn.decl

            ; Bare invocation: Foo()
            (invocation_expression function: (identifier) @call.name)

            ; Member access invocation: obj.Method()
            ; `member_access_expression` has a `name:` field that gives
            ; the trailing identifier — same convention as Java/TS.
            (invocation_expression
              function: (member_access_expression
                name: (identifier) @call.name))

            ; Attribute edges (Phase 14.2.2).
            ; [HttpGet("/x")] / [Authorize] — bare attribute name.
            (method_declaration
              (attribute_list (attribute name: (identifier) @call.name))
              name: (identifier) @fn.name) @fn.decl

            (constructor_declaration
              (attribute_list (attribute name: (identifier) @call.name))
              name: (identifier) @fn.name) @fn.decl

            ; [System.Web.Mvc.HttpGet] — qualified attribute, rightmost
            ; identifier wins. In tree-sitter-c-sharp `qualified_name`
            ; recurses: outer `name:` field walks toward the trailing
            ; identifier leaf.
            (method_declaration
              (attribute_list (attribute name: (qualified_name
                name: (identifier) @call.name)))
              name: (identifier) @fn.name) @fn.decl

            (constructor_declaration
              (attribute_list (attribute name: (qualified_name
                name: (identifier) @call.name)))
              name: (identifier) @fn.name) @fn.decl

            ; Phase 14.6 — class-level attribute edges. C# `class_declaration`
            ; takes `attribute_list` siblings to the keyword the same way
            ; method_declaration does. No `@fn.name` / `@fn.decl` capture
            ; → attribution falls to the module-scope sentinel.

            ; [ApiController] class Foo {} — bare attribute
            (class_declaration
              (attribute_list (attribute name: (identifier) @module_call.name)))

            ; [System.Web.Mvc.ApiController] class Foo {} — qualified
            (class_declaration
              (attribute_list (attribute name: (qualified_name
                name: (identifier) @module_call.name))))
            "#,
        ),
        _ => None,
    }
}
