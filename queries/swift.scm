;; Protocols
(protocol_declaration
  name: (type_identifier) @interface.name) @interface.def

;; tree-sitter-swift folds class / struct / enum / actor / extension into a
;; single `class_declaration` node, distinguished by the `declaration_kind`
;; field. Each variant gets its own pattern so the right SymbolKind is
;; assigned and the same node is NOT captured twice (tree-sitter query
;; patterns do not short-circuit — every structurally matching pattern
;; fires).
(class_declaration
  declaration_kind: "enum"
  name: (type_identifier) @enum.name) @enum.def

(class_declaration
  declaration_kind: "class"
  name: (type_identifier) @class.name) @class.def

(class_declaration
  declaration_kind: "struct"
  name: (type_identifier) @struct.name) @struct.def

(class_declaration
  declaration_kind: "actor"
  name: (type_identifier) @class.name) @class.def

;; Functions
(function_declaration
  name: (simple_identifier) @fn.name) @fn.def

;; Imports: import Foundation
(import_declaration
  (identifier) @import.name)
