;; Functions
(function_item
  name: (identifier) @fn.name) @fn.def

;; Structs
(struct_item
  name: (type_identifier) @struct.name) @struct.def

;; Enums
(enum_item
  name: (type_identifier) @enum.name) @enum.def

;; Traits
(trait_item
  name: (type_identifier) @trait.name) @trait.def

;; Impl blocks — type being implemented
(impl_item
  type: (type_identifier) @impl.type) @impl.def

;; Methods inside impl blocks
(impl_item
  body: (declaration_list
    (function_item
      name: (identifier) @impl.method)))

;; Type aliases
(type_item
  name: (type_identifier) @type.name) @type.def

;; Constants
(const_item
  name: (identifier) @const.name) @const.def
