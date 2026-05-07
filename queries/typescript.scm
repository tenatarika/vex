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
