;; Top-level mapping keys. We anchor at stream → document → block_node →
;; block_mapping so deeply nested keys don't flood the index. The scalar
;; inside flow_node is what holds the key text.
(stream
  (document
    (block_node
      (block_mapping
        (block_mapping_pair
          key: (flow_node
            [(plain_scalar (string_scalar) @property.name)
             (single_quote_scalar) @property.name
             (double_quote_scalar) @property.name]))))))
