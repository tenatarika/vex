;; .my-class — capture the identifier inside class_name
(class_selector
  (class_name (identifier) @class.name))

;; #my-id — id_name is a leaf node, capture it directly
(id_selector
  (id_name) @const.name)

;; @keyframes name { ... }
(keyframes_statement
  (keyframes_name) @fn.name)

;; --my-custom-prop: value;  CSS custom properties (variables).
;; The grammar models them as a `property_name` inside a `declaration`;
;; we filter to those that start with `--` so we don't index every plain
;; property like `color` or `margin`.
(declaration
  (property_name) @property.name
  (#match? @property.name "^--"))
