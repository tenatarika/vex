;; Table headers `[server]` and `[server.http]` — Class kind so they
;; stand out as section anchors in `vex outline`.
(table
  [(bare_key) (dotted_key) (quoted_key)] @class.name)

;; Array-of-tables `[[products]]`
(table_array_element
  [(bare_key) (dotted_key) (quoted_key)] @class.name)

;; Key/value pairs — Property kind. Includes both top-level pairs (direct
;; children of `document`) and pairs scoped to a table; both are useful
;; navigation targets.
(pair
  [(bare_key) (dotted_key) (quoted_key)] @property.name)
