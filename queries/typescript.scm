;; Classes
(class_declaration
  name: (type_identifier) @class.name)

;; Interfaces
(interface_declaration
  name: (type_identifier) @interface.name)

;; Enums
(enum_declaration
  name: (identifier) @enum.name)

;; Functions
(function_declaration
  name: (identifier) @fn.name)

;; Arrow functions assigned to const/let
(lexical_declaration
  (variable_declarator
    name: (identifier) @fn.name
    value: (arrow_function)))

;; Class methods (regular, static, get/set, constructor). Tree-sitter-
;; typescript uses `method_definition` inside `class_body`; the name is
;; a `property_identifier`. Without this entry, methods declared inside
;; a TS/TSX class are invisible to `vex search`/`vex usages --strict` —
;; v1.14.1 follow-up to close the same SCM gap that bit C++.
(method_definition
  name: (property_identifier) @impl.method)

;; Abstract method signatures in abstract classes (`abstract foo(): void;`).
;; Same shape but `abstract_method_signature` rather than `method_definition`.
(abstract_method_signature
  name: (property_identifier) @impl.method)

;; Method signatures inside interfaces/type literals
;; (`interface Foo { bar(): void; }`). Tree-sitter-typescript emits
;; these as `method_signature` nodes whose name is a `property_identifier`.
(method_signature
  name: (property_identifier) @impl.method)

;; Type aliases
(type_alias_declaration
  name: (type_identifier) @type.name)

;; Imports: import { X, Y } from 'z'
(import_statement
  (import_clause
    (named_imports
      (import_specifier
        name: (identifier) @import.name))))

;; Imports: import X from 'z'
(import_statement
  (import_clause
    (identifier) @import.name))

;; Imports: import * as X from 'z'
(import_statement
  (import_clause
    (namespace_import
      (identifier) @import.name)))
