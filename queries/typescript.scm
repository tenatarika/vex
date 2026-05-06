;; Classes
(class_declaration
  name: (type_identifier) @class.name) @class.def

;; Interfaces
(interface_declaration
  name: (type_identifier) @interface.name) @interface.def

;; Functions
(function_declaration
  name: (identifier) @fn.name) @fn.def

;; Arrow functions assigned to const/let
(lexical_declaration
  (variable_declarator
    name: (identifier) @fn.name
    value: (arrow_function))) @fn.def

;; Type aliases
(type_alias_declaration
  name: (type_identifier) @type.name) @type.def

;; Exported functions
(export_statement
  (function_declaration
    name: (identifier) @fn.name)) @fn.def
