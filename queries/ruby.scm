;; Classes
(class
  name: (constant) @class.name) @class.def

;; Modules
(module
  name: (constant) @class.name) @class.def

;; Methods
(method
  name: (identifier) @fn.name) @fn.def

;; Singleton methods (class methods)
(singleton_method
  name: (identifier) @fn.name) @fn.def
