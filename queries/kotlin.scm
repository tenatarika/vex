;; Functions
(function_declaration
  (identifier) @fn.name)

;; Classes (covers class, data class, enum class)
(class_declaration
  "class"
  (identifier) @class.name)

;; Interfaces
(class_declaration
  "interface"
  (identifier) @interface.name)

;; Objects (singleton)
(object_declaration
  (identifier) @class.name)

;; Properties (top-level or class-level val/var)
(property_declaration
  (variable_declaration
    (identifier) @property.name))

;; Imports: import com.example.Name
(import
  (qualified_identifier
    (identifier) @import.name))
