;; Classes
(class_declaration
  name: (identifier) @class.name) @class.def

;; Interfaces
(interface_declaration
  name: (identifier) @interface.name) @interface.def

;; Enums
(enum_declaration
  name: (identifier) @enum.name) @enum.def

;; Methods
(method_declaration
  name: (identifier) @fn.name) @fn.def

;; Constructors
(constructor_declaration
  name: (identifier) @fn.name) @fn.def
