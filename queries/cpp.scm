;; Free functions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @fn.name))

;; Qualified functions (e.g. void Foo::bar())
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier
      name: (identifier) @fn.name)))

;; Classes (also matches inside template_declaration — tree-sitter descends into children)
(class_specifier
  name: (type_identifier) @class.name)

;; Structs
(struct_specifier
  name: (type_identifier) @struct.name)

;; Enums (plain and enum class)
(enum_specifier
  name: (type_identifier) @enum.name)

;; Type aliases — using X = Y
(alias_declaration
  name: (type_identifier) @type.name)

;; Type aliases — typedef
(type_definition
  declarator: (type_identifier) @type.name)

;; Function declarations (prototypes in headers)
(declaration
  declarator: (function_declarator
    declarator: (identifier) @fn.name))

;; Method declarations inside class/struct body (prototypes — `int do_charge();`
;; inside `class Foo {}`). These are `field_declaration` nodes whose name lives
;; at `function_declarator → field_identifier`, distinct from the
;; file-level `declaration → identifier` shape above. Without this, every C++
;; method declared in a header is invisible to `vex search`/`vex usages
;; --strict`. v1.14.1 follow-up to the v1.14 cross-file refs work.
(field_declaration
  declarator: (function_declarator
    declarator: (field_identifier) @impl.method))

;; Inline method definitions inside class body — `int bar() { return 0; }`
;; inside `class Foo {}`. Tree-sitter parses these as `function_definition`
;; (same node kind as free functions) but the inner declarator is
;; `field_identifier` rather than `identifier`. The free-fn query above
;; only matches `identifier` so without this entry, inline class methods
;; with bodies aren't indexed either.
(function_definition
  declarator: (function_declarator
    declarator: (field_identifier) @impl.method))

;; Include directives as imports
(preproc_include
  path: (string_literal) @import.name)

(preproc_include
  path: (system_lib_string) @import.name)
