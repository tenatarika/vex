;; Classes
(class_declaration
  (type_identifier) @class.name) @class.def

;; Objects
(object_declaration
  (type_identifier) @class.name) @class.def

;; Functions
(function_declaration
  (simple_identifier) @fn.name) @fn.def

;; Interfaces
(class_declaration
  (type_identifier) @interface.name) @interface.def

;; Properties
(property_declaration
  (variable_declaration
    (simple_identifier) @property.name)) @property.def
