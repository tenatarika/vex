;; Free functions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @fn.name))

;; Qualified functions (e.g. void Foo::bar())
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier
      name: (identifier) @fn.name)))

;; Classes (also matches inside template_declaration — tree-sitter descends into children)
(class_specifier
  name: (type_identifier) @class.name)

;; Structs
(struct_specifier
  name: (type_identifier) @struct.name)

;; Enums (plain and enum class)
(enum_specifier
  name: (type_identifier) @enum.name)

;; Type aliases — using X = Y
(alias_declaration
  name: (type_identifier) @type.name)

;; Type aliases — typedef
(type_definition
  declarator: (type_identifier) @type.name)

;; Function declarations (prototypes in headers)
(declaration
  declarator: (function_declarator
    declarator: (identifier) @fn.name))

;; Include directives as imports
(preproc_include
  path: (string_literal) @import.name)

(preproc_include
  path: (system_lib_string) @import.name)
