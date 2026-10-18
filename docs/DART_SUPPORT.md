# Dart indexing

Vex pins `tree-sitter-dart-orchard` 0.7.0 for `.dart` files. Indexing is
offline and does not execute or require the Dart SDK.

The extractor indexes classes, enums, mixins, extensions, extension types,
functions, methods, constructors, type aliases, enum constants, and imports.
Static methods and unnamed constructors use bare method and class names,
respectively. Named constructors use qualified identities such as `Repo.named`.
Project declarations resolve static call and reference names to the bare method
name while preserving named-constructor identities. Top-level functions retain
their declared names, including names that start with an uppercase letter.
Incremental index updates retain raw qualified Dart references, calls, and
declaration facts in the Dart parse cache and a versioned index sidecar. This
lets updates rebind unchanged callers after declaration-only edits without
reading or reparsing unchanged Dart source files.
Call edges are syntactic; hierarchy edges retain `extends`, `implements`, and
`with` relations. References use the shared identifier filter and are not
resolved by a compiler or across imported libraries.

Fixtures cover selected declarations, constructors, calls, hierarchy
relations, aliases, imports, interpolation, and test paths. They do not prove
complete Dart 3 syntax coverage. Unsupported syntax may produce partial trees;
records, patterns, class modifiers, and other modern forms remain incompletely
verified. There is no strict compiler-backed Dart reference binder.

See [Supported languages](SUPPORTED_LANGUAGES.md) for the support matrix.
