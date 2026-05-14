;; id="my-id" — index the value as a Constant so users can jump to anchors.
;; We capture the attribute_value inside quoted form (the common case);
;; unquoted ids are extremely rare and not worth a separate capture.
(attribute
  (attribute_name) @_attr
  (quoted_attribute_value (attribute_value) @const.name)
  (#eq? @_attr "id"))

;; Custom-element tag names contain a hyphen by HTML spec
;; (`<my-component>`). Index them as Classes so component definitions are
;; discoverable. Plain tags (`div`, `span`) are noise and excluded.
(start_tag
  (tag_name) @class.name
  (#match? @class.name "-"))

(self_closing_tag
  (tag_name) @class.name
  (#match? @class.name "-"))
