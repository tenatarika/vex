# Supported languages

Vex parses source code via [tree-sitter](https://tree-sitter.github.io/).
Each language has a dedicated grammar crate plus an `.scm` query that
selects the AST nodes we promote to indexed symbols.

This table is the source of truth for which languages are supported and
which grammar version each release ships against.

## Version matrix

Last verified: **2026-05-14** (vex 1.4.x line).

| Language | Extensions | Grammar crate | Grammar version | Symbol kinds extracted |
|---|---|---|---|---|
| Rust       | `.rs`                                   | `tree-sitter-rust`        | 0.24 | function, struct, enum, trait, impl, method, constant |
| Python     | `.py`                                   | `tree-sitter-python`      | 0.25 | class, function (incl. decorated), import |
| TypeScript / TSX / JavaScript / JSX | `.ts`, `.tsx`, `.js`, `.jsx` | `tree-sitter-typescript`  | 0.23 | class, interface, enum, function, arrow function (const), type alias, import |
| Go         | `.go`                                   | `tree-sitter-go`          | 0.25 | function, method, struct, interface, type alias, import |
| Kotlin     | `.kt`, `.kts`                           | `tree-sitter-kotlin-ng`   | 1.1  | function, class, interface, object, data class, property, import |
| Java       | `.java`                                 | `tree-sitter-java`        | 0.23 | class, interface, enum, method, constructor, import |
| C#         | `.cs`                                   | `tree-sitter-c-sharp`     | 0.23 | class, interface, struct, enum, method, property |
| Ruby       | `.rb`                                   | `tree-sitter-ruby`        | 0.23 | class, module (as class), method, singleton method |
| Swift      | `.swift`                                | `tree-sitter-swift`       | 0.7  | class, struct, enum, actor (mapped to class), protocol (as interface), function, import |
| C++        | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx`, `.h` | `tree-sitter-cpp`     | 0.23 | function, class, struct, enum (incl. enum class), type alias (using/typedef), include |
| SQL (PostgreSQL flavour) | `.sql`                    | `tree-sitter-sequel`      | 0.3  | table, view, materialized view, schema, type (enum), function, trigger, index, sequence, extension |
| Markdown   | `.md`, `.markdown`                      | `tree-sitter-md`          | 0.5  | ATX headings (`#` through `######`) |
| PHP        | `.php`, `.phtml`                        | `tree-sitter-php`         | 0.24 | class, interface, trait, enum, function, method, class constant, `use` import |
| Bash       | `.sh`, `.bash`                          | `tree-sitter-bash`        | 0.25 | function, `source` / `.` imports |
| Lua        | `.lua`                                  | `tree-sitter-lua`         | 0.5  | function (top-level, local, `Mod.fn`, `Class:method`), `require` imports |
| CSS        | `.css`                                  | `tree-sitter-css`         | 0.25 | class selectors, id selectors (as constants), `@keyframes` names, custom properties (`--var`) |
| HTML       | `.html`, `.htm`                         | `tree-sitter-html`        | 0.23 | `id` attribute values (as constants), custom-element tag names (hyphenated) |
| YAML       | `.yaml`, `.yml`                         | `tree-sitter-yaml`        | 0.7  | document-root mapping keys (top-level only) |
| TOML       | `.toml`                                 | `tree-sitter-toml-ng`     | 0.7  | table headers (`[name]`, `[[name]]`), key/value pairs |

Tree-sitter core itself is currently pinned at `0.26`. The grammar version
column lists the crate's caret range from `Cargo.toml`; the locked patch
version is in `Cargo.lock`.

## How extension → language mapping works

`Language::from_extension` in `src/parse/language.rs` is the single source
of truth. Anything not in that match arm is ignored during indexing.

The TSX grammar (`tree_sitter_typescript::LANGUAGE_TSX`) is a superset of
plain TypeScript, so we use it for both `.ts` and `.tsx`. JavaScript is
parsed with the same grammar — it has wider symbol coverage and tolerates
plain JS input.

C/C++ headers (`.h`) are parsed with the C++ grammar. If a project has C
headers that the C++ grammar mis-handles, file an issue.

## How to upgrade a grammar

1. Bump the crate version in `Cargo.toml`.
2. `cargo update -p tree-sitter-<lang>`.
3. `cargo test --test <lang>_query_test` — the regression tests at
   `tests/<lang>_query_test.rs` are the early-warning system. If a node
   was renamed or the ABI changed, those tests fail with a clear
   diagnostic.
4. If a test fails, inspect the new grammar's `node-types.json` and adapt
   `queries/<lang>.scm`. Cargo extracts the grammar to
   `~/.cargo/registry/src/index.crates.io-*/tree-sitter-<lang>-<ver>/src/node-types.json`.
5. Re-run `cargo test` end-to-end.

If an ABI mismatch slips through (grammar requires a newer tree-sitter
core than we ship), the user will now see a clear stderr warning of the
form `warning: skipped N CSharp file(s) — failed to load CSharp grammar:
csharp query: Incompatible language version 15. Expected minimum 13,
maximum 14.` rather than a silent empty index. The plumbing for that
lives in `src/parse/queries.rs::try_get_query` and the aggregation in
`src/index/pipeline.rs::parse_files`.

## How to add a new language

1. Add the grammar crate to `Cargo.toml`.
2. Add a `Language::<Name>` variant in `src/parse/language.rs`, including
   `from_extension`, `ts_language`, and `as_str`.
3. Add a `<lang>_QUERY` static in `src/parse/queries.rs` and add the match
   arm to `lookup`.
4. Add the new variant to the `inheritance_query` match in
   `src/hierarchy/mod.rs` (return `None` unless the language has
   class-hierarchy semantics to support `vex implementations`).
5. Add the canonical CLI name(s) to the `Commands::Pattern` arm in
   `src/cli/mod.rs` so `vex pattern --lang <name>` works for spellings
   that aren't file extensions (e.g. `--lang shell` for Bash).
6. Author `queries/<lang>.scm` with the capture names listed in
   `src/parse/extractor.rs` (`fn.name`, `class.name`, etc).
7. Add `tests/<lang>_query_test.rs` covering at minimum: grammar loads on
   empty input + one canonical example per symbol kind.
8. Update this file's version matrix and trim the Roadmap candidates.

## Roadmap

Remaining Phase 4 candidates: Scala, Haskell. File an issue with a use
case if you want one prioritised.
