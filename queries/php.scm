;; Classes
(class_declaration
  name: (name) @class.name)

;; Interfaces
(interface_declaration
  name: (name) @interface.name)

;; Traits (mapped to Trait kind)
(trait_declaration
  name: (name) @trait.name)

;; Enums (PHP 8.1+)
(enum_declaration
  name: (name) @enum.name)

;; Top-level functions
(function_definition
  name: (name) @fn.name)

;; Methods (treated as functions; project schema uses fn.name for both)
(method_declaration
  name: (name) @fn.name)

;; Class constants — name lives inside const_element
(const_declaration
  (const_element
    (name) @const.name))

;; `use Foo\Bar;` and `use Foo\Bar as Baz;` — index the tail name and,
;; when present, the alias. We also handle the rarer `use Foo;` (no
;; backslash) form.
;;
;; The `!alias` guard on the bare-name pattern is critical: without it,
;; the alias node would be matched both as the bare `(name)` child AND
;; via the explicit `alias:` field, producing a duplicate import entry
;; (PHP grammar exposes `alias:` as a regular field-tagged child of type
;; `name`, so an unguarded `(namespace_use_clause (name))` pattern picks
;; it up as well).
(namespace_use_clause
  (qualified_name (name) @import.name))

(namespace_use_clause
  !alias
  (name) @import.name)

(namespace_use_clause
  alias: (name) @import.name)
