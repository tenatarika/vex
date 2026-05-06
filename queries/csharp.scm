;; Classes
(class_declaration
  name: (identifier) @class.name) @class.def

;; Interfaces
(interface_declaration
  name: (identifier) @interface.name) @interface.def

;; Structs
(struct_declaration
  name: (identifier) @struct.name) @struct.def

;; Enums
(enum_declaration
  name: (identifier) @enum.name) @enum.def

;; Methods
(method_declaration
  name: (identifier) @fn.name) @fn.def

;; Properties
(property_declaration
  name: (identifier) @property.name) @property.def
