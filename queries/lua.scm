;; Top-level `function foo() ... end` and `local function foo() ... end`
(function_declaration
  name: (identifier) @fn.name)

;; `function Module.bar() ... end` — capture the final field name
(function_declaration
  name: (dot_index_expression
    field: (identifier) @fn.name))

;; `function Class:method() ... end` — capture the method name
(function_declaration
  name: (method_index_expression
    method: (identifier) @fn.name))

;; `require("module")` — `variable` is a supertype, so the tree exposes
;; `identifier` directly under the `name:` field. The string node may
;; omit `string_content` when there are no escape sequences, so we match
;; the string itself and let the extractor read the literal text (with
;; surrounding quotes — acceptable for an import reference).
(function_call
  name: (identifier) @_call
  arguments: (arguments
    (string) @import.name)
  (#eq? @_call "require"))
