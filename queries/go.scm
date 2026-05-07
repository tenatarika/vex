;; Functions
(function_declaration
  name: (identifier) @fn.name) @fn.def

;; Methods (function with receiver)
(method_declaration
  name: (field_identifier) @impl.method) @method.def

;; Structs
(type_declaration
  (type_spec
    name: (type_identifier) @struct.name
    type: (struct_type))) @struct.def

;; Interfaces
(type_declaration
  (type_spec
    name: (type_identifier) @interface.name
    type: (interface_type))) @interface.def

;; Imports: import "fmt"
(import_spec
  (interpreted_string_literal
    (interpreted_string_literal_content) @import.name))
