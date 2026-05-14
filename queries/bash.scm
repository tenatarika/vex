;; Function definitions: `foo() { ... }` and `function foo { ... }`
(function_definition
  name: (word) @fn.name)

;; `source path/to/lib.sh` and `. path/to/lib.sh` — treat the sourced
;; path as an import. We capture the literal command argument; the
;; extractor's import-name handling stores it verbatim.
(command
  name: (command_name (word) @_cmd)
  argument: [(word) (string) (raw_string)] @import.name
  (#match? @_cmd "^(source|\\.)$"))
