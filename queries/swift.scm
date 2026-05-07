;; Protocols
(protocol_declaration
  name: (type_identifier) @interface.name) @interface.def

;; Enums
(enum_declaration
  name: (type_identifier) @enum.name) @enum.def

;; Functions
(function_declaration
  name: (simple_identifier) @fn.name) @fn.def

;; Classes (class_declaration covers class, struct, actor — all mapped to class)
(class_declaration
  name: (type_identifier) @class.name) @class.def

;; Imports: import Foundation
(import_declaration
  (identifier) @import.name)
