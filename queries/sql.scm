;; Tables
(create_table
  (object_reference
    (identifier) @class.name))

;; Views
(create_view
  (object_reference
    (identifier) @class.name))

;; Materialized views
(create_materialized_view
  (object_reference
    (identifier) @class.name))

;; Types (CREATE TYPE mood AS ENUM ...)
(create_type
  (object_reference
    (identifier) @enum.name))

;; Schemas
(create_schema
  (identifier) @class.name)

;; Functions
(create_function
  (object_reference
    (identifier) @fn.name))

;; Triggers
(create_trigger
  (object_reference
    (identifier) @fn.name))

;; Indexes
(create_index
  (identifier) @property.name)

;; Sequences
(create_sequence
  (object_reference
    (identifier) @property.name))

;; Extensions
(create_extension
  (identifier) @property.name)

;; Refs: ALTER TABLE captures the table being modified
(alter_table
  (object_reference
    (identifier) @import.name))
