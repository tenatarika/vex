;; Classes
(class_definition
  name: (identifier) @class.name) @class.def

;; Functions
(function_definition
  name: (identifier) @fn.name) @fn.def

;; Decorated functions (async, property, etc.)
(decorated_definition
  (function_definition
    name: (identifier) @fn.name)) @fn.def
