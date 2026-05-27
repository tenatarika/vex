# Changelog

All notable changes to vex are documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

- **Phase 14.4 — wire-format honesty rename.** `usages --why` JSON trace
  `mode` field now emits `"fst_lookup"` instead of `"text_scan"` on the
  non-strict path; the underlying data source is and always was an FST
  lookup, not a text scan. A new `mode_legacy` field carries the v1.8.x
  label (`"text_scan"`) for back-compat with consumers that learned the
  contract before the rename. `mode_legacy` will be removed in v1.12.

### Added

- **Phase 14.2.1 — TypeScript + Rust decorator edges via sibling
  adjacency.** TypeScript method decorators on class methods
  (`class C { @Get("/x") handler() {} }`) and Rust outer attributes on
  function / method items (`#[tokio::test] fn it_works() {}`,
  `impl Foo { #[wasm_bindgen] fn bar() {} }`) now emit forward call
  edges `decorated_fn → decorator_target`. Unlike Python/Java/Kotlin/C#
  where the decorator/annotation lives INSIDE the function's byte range,
  TS `decorator` and Rust `attribute_item` nodes are SIBLINGS of the
  function under a shared parent (`class_body` / `source_file` /
  `declaration_list`). The new sibling-adjacency pass in
  `extract_callgraph` walks each captured decorator host forward to
  find the next function-shaped sibling and remaps the synthetic
  call's `byte_offset` onto the function's start so the existing
  attribution logic lands the edge on the right caller. Callee = the
  rightmost identifier of the decorator/attribute PATH (the part
  before the optional argument list). For multi-segment paths the
  rightmost wins: `@nest.Get("/x")` → `Get`, `#[tokio::test]` →
  `test`. For single-segment paths the path itself is the callee:
  `@bound` → `bound`, `#[wasm_bindgen]` → `wasm_bindgen`. Arguments
  are NEVER part of the path: `#[serde(rename = "x")]` → `serde`
  (not `rename`); `#[allow(dead_code)]` → `allow` (not `dead_code`).
  Rust `#[derive(...)]` is filtered out by attribute-path head-name
  (compile-time codegen, not runtime call edges); arguments to other
  attributes are never inspected, so `#[some_attr(derive = "x")]`
  still emits an edge to `some_attr`. JavaScript files inherit the
  same decorator coverage via the TSX grammar. Class-level decorators,
  TS property/parameter decorators, and `#[derive(...)]` remain out of
  scope (Phase 14.6 / intentional exclusion). Performance budget:
  re-indexing the vex self-repo gave median **306.56ms** vs 14.2.2
  baseline 287.5ms (+6.6%), within the ≤317ms +10% ceiling; the
  cost driver is a per-call-capture ancestor walk that guards against
  the standard `call_expression` pattern double-firing inside
  decorators.
- **Phase 14.2.2 — Kotlin + C# call graph and annotation edges.** Kotlin
  and C# now have first-class callgraph support: `function_declaration`,
  `method_declaration`, and `constructor_declaration` produce caller
  FnDefs; direct calls, `navigation_expression` (`obj.method()`), and
  `invocation_expression` over `member_access_expression`
  (`obj.Method()`) produce callee edges. On top of the base callgraph,
  Kotlin annotations (`@JvmStatic fun foo()`) and C# method/constructor
  attributes (`[HttpGet("/x")] public Response GetUsers()`) emit forward
  edges `annotated_fn → annotation_target`. Qualified names follow the
  rightmost-identifier convention from Phase 14.2 —
  `@kotlin.jvm.JvmStatic` → `JvmStatic`, `[System.Web.Mvc.HttpGet]` →
  `HttpGet`. Both languages join the `COMPILED_QUERIES` LazyLock so the
  added query compiles stay off the hot path. No format bump — reuses
  the existing CallEdge shape. Performance budget: re-indexing the vex
  self-repo, release binary, 6 cold-cache runs over 3668 symbols, gave
  median **287.5ms** (+2.7% vs the 14.2 baseline of 280ms; well under
  the +10% ceiling), best **268.01ms** (−4.3%). Remaining gaps:
  TypeScript / Rust decorators (Phase 14.2.1) and class-level decorators
  (Phase 14.6).
- **Phase 14.2 — decorator edges (Python + Java).** Function and method
  decorators in Python (`@app.get("/x") def list_items()`) and method-level
  annotations in Java (`@GetMapping("/x") public Response listItems()`)
  now emit forward call edges `decorated_fn → decorator_target`.
  `vex callers get` lists every FastAPI route handler; `vex callers
  GetMapping` lists every Spring handler. Callee resolves to the rightmost
  identifier of the decorator name (consistent with method-call captures).
  No format bump — reuses the existing CallEdge shape. Performance
  budget: re-indexing the vex self-repo stayed within +0% of pre-14.2
  baseline (mean 280ms vs 297ms — pattern-matching cost is below noise).
  TypeScript and Rust deferred to Phase 14.2.1 (sibling-adjacency in
  grammar); class-level decorators deferred to Phase 14.6.
- **Phase 14.1 — module-level callers.** `vex callers <fn>` now reports
  module-scope call sites via a synthetic per-file `<module:path>` caller
  (`SymbolKind::Module = 13`). Module symbols are excluded from `vex search`,
  `vex outline`, and ranked search results — they appear only as resolved
  callers in the call graph. Class-body call sites also attribute to the
  synthetic Module symbol (broader coverage; string-resolved refs remain
  Phase 15 territory). No binary format bump — older readers see `Module`
  as `kind="unknown"` and gracefully ignore.
- CI on pull requests: separate `cli-tests`, `msrv` (1.80), `beta`
  (informational, allowed to fail), and `benches` (`cargo bench --no-run`)
  jobs.

## [1.9.1] - 2026-05-25

Windows hotfix for the Phase 13.12 ranking-evaluation harness. v1.9.0's
`path_matches` compared the index-side host-separator paths
(`src\store\reader.rs` on Windows) against the forward-slash golden
TOML entries directly — every shape failed string-equality, so the
Windows CI run reported `nDCG / recall / MRR = 0.0` on all 16 golden
queries and tripped the 0.85 regression floor. macOS / Linux were
unaffected. The README honesty pass that landed alongside the fix is
also part of this patch.

### Fixed

- **Windows path-separator regression in `vex eval`** — `path_matches`
  now normalizes `\` → `/` once at the comparison boundary, restoring
  the cross-platform contract documented in
  `docs/RANKING-EVAL.md`. Unit test
  `path_matches_normalizes_windows_separator` pins the four canonical
  shapes (exact / trailing-`/file` / dir-prefix) against Windows-style
  paths and re-validates the H2 neighbour-directory protection.

### Documentation

- README v1.9.0 refresh with honest coverage caveats — adds a
  `## What Vex isn't` section between Why Vex? and How It Compares,
  names the three language tiers behind `--strict usages` / indexed
  pattern prefilter / baseline structural search, calls out the
  function-scope limit on `vex callers`, and replaces the
  maximum-anchored "6-88x" token-efficiency framing with a
  median-anchored "typically 6-10x; up to 88x on minified" pair.
  Quick Start gains a v1.9 section covering bundle / diff-context /
  show truncation / eval / capabilities. Commands table adds
  `vex capabilities`, `vex eval`, `vex self-update`, and the Phase
  13.3 truncation flags on `vex show`.

## [1.9.0] - 2026-05-25

Phase 13 lands the agent-integration foundation: a versioned response
envelope with per-result `signals` and `rank_percentile`, a ranking-
evaluation harness pinning nDCG@10/recall@10/MRR, smart `show`
truncation, diff-context filters across every search-shaped command,
LLM-tuned MCP tool descriptions, and `vex bundle` — one CLI subcommand
plus one MCP tool that replaces the four-round-trip
`show → callers → callees → similar` loop with a single envelope.
Format stays v6; `MIN_SUPPORTED_VERSION` stays at 3 — older indexes
keep opening. The pre-1.9 bare-array `vex search --format json` shape
is opt-in via `VEX_JSON_ENVELOPE=0` and slated for removal in v2.0.

Also closes a long-running honesty gap surfaced by the v1.8.2 external
review: a new `docs/LIMITATIONS.md` documents `callers`-is-function-
scoped, the T1/T2 `usages` quality tiers, and the dynamic-dispatch
patterns that static analysis cannot see. `--strict` help text no
longer promises a deferral that shipped three minors ago.

### Added

- **`vex bundle` (Phase 13.2)** — unified multi-source bundle primitive.
  One command, three modes, replaces the 4-round-trip agent loop
  `show → callers → callees → similar` with a single call. The MCP
  surface is one `bundle` tool with a **flat schema** (mode-specific
  args validated server-side; no JSON-Schema `oneOf`).
  - `--mode symbol <name>` — body + direct callers + direct callees +
    semantic-similar matches. Body extraction is full (no truncation;
    `vex show --signature-only` is the per-symbol truncation surface).
    Defaults: `--callers-max 10`, `--callees-max 10`, `--similar-max 5`.
  - `--mode pr-impact --base <rev>` — changed symbols since `<rev>`
    plus transitive callers (depth=2 default) plus tests that
    transitively reach the changes. Test classification by path
    (`/tests/`, `_test.`, `__tests__/`, `spec/`) or signature
    attribute (`#[test]`, `#[cfg(test)]`, `#[tokio::test...]`).
    `_meta.vex.dev/diff_filter` carries `{ scope, changed_paths,
    retained, dropped }` so agents can correlate with `git diff`.
  - `--mode project [--top-n N] [--path-glob G]` — top-N symbols by
    **reverse call-graph indegree** (count of distinct callers).
    Experimental; documented as `scoring: "reverse_indegree"`. No
    PageRank — see roadmap for revival path under 13.12.
  - Response envelope reuses Phase 13.0 `{ protocol_version,
    capabilities, _meta, results }`. `results.items[i]` carries the
    13.11 `signals` block plus a `role` discriminator (`body | caller
    | callee | similar | changed | transitive_caller | test | top`),
    a *global* monotonic-descending `rank_percentile`, and a
    per-role 0-indexed `role_rank`. `results.mode_hints` is a
    mode-specific JSON blob (counts, truncation flags, scoring label,
    `empty_reason` when the items list is empty).
  - `capabilities.bundle_modes` now advertises `["symbol", "pr-impact",
    "project"]` (was `[]` in v1.9.0-pre).
  - Latency baseline (Criterion, `benches/bundle.rs`): pr-impact BFS
    on 50 changed symbols × depth=2 ≈ **86 µs**; project indegree scan
    over ~500 functions ≈ **44 µs**; symbol mode full pipeline (FST +
    body + callers + callees + similar guard) ≈ **139 µs**. All three
    well under the 100 ms threshold that would justify rayon-izing
    the pr-impact BFS outer loop.
- `vex capabilities` CLI subcommand returning the Phase 13 capability
  matrix as JSON (`protocol_version`, `signals`, `why`, `scope_filters`,
  `metadata_filters`, `empty_reason`, `bundle_modes`, `auto_update`).
  Agents can probe this once at startup instead of re-reading help text.
- Per-result `signals` (`fst_hit`, `bm25_rank`, `semantic_rank`,
  `fuzzy_distance`) and a normalized `rank_percentile` field in
  `vex search --format json` envelope. `rank_percentile` spans
  `[0.0, 1.0]` inclusive — the top result is `1.0`, the bottom is `0.0`,
  and a lone result is `1.0`.
- MCP responses now carry `protocol_version`, `capabilities`, and a
  namespaced `_meta` block (`vex.dev/index_age_ms`, `traceparent`,
  `ttlMs`, `cacheScope`) on top of `structuredContent.results`. Signals
  live in `structuredContent` only — `_meta` is invisible to the LLM
  per the MCP spec.

### Changed

- `vex search --format json` now emits an envelope
  `{ protocol_version, capabilities, _meta, results: [...] }` instead
  of a bare array. Set `VEX_JSON_ENVELOPE=0` (also accepts `false` /
  `off`, case-insensitive) to opt out and restore the pre-1.9
  bare-array shape. The opt-out is a migration aid only and will be
  removed in v2.0.

### Documentation

- New `docs/LIMITATIONS.md` documents static-analysis coverage gaps
  surfaced by the v1.8.2 external review: `vex callers` is function-
  scoped (module-level call sites and decorator dispatch are
  invisible); `vex usages` quality varies across the T1 / T2 language
  tiers; dynamic dispatch (`getattr`, factory strings, decorator
  routing) cannot be statically resolved. Includes a coverage matrix,
  concrete repros, and the `vex grep` escape-hatch recommendation.
- `--strict` help text on `vex usages` no longer claims the
  `reference_edges` section is "deferred until 11.1.3" — Phase 11.1
  shipped in v1.8.0. Replaced with an accurate description that names
  the five binder-supported languages (Rust / TypeScript / Python /
  C# / C++) and points at `docs/LIMITATIONS.md`.
- `--why` help text on `vex usages` documents that the `mode:
  "text_scan"` label is historical — the underlying data path is the
  FST lookup, populated from an AST identifier walk on T1 languages
  and a line-scan on T2.
- README gains a `## Known limitations` section linking the same
  surface. `vex callers --help` and `vex usages --help` doc-comments
  point at `docs/LIMITATIONS.md` so agents reading the schema discover
  the coverage gaps without a separate fetch.

## [1.8.2] - 2026-05-23

Closes Phase 11.4 T2 with the final four language allowlists (Kotlin /
Swift / PHP / Ruby — T2a now covers 12 languages) and ships Phase 11.10:
`--why` is now available on `usages`, `similar`, and `duplicates` in
addition to the existing `search` and `pattern` surfaces. No format
change; v6 stays compatible. All Phase 11 items are complete.

### Added — pattern skeletons populated for

- **Kotlin** — `class_declaration` (umbrella for class / interface /
  data class / enum class / sealed class) / `object_declaration` /
  `companion_object` (named or anonymous) / `function_declaration` /
  `property_declaration` (`variable_declaration > identifier` walker
  for sigil-free ident) / `type_alias` (uses `type:` field) /
  `secondary_constructor` (anonymous) / `enum_entry` (positional
  `identifier` walker) / `anonymous_initializer` (anonymous `init` block)
  / `lambda_literal` (anonymous, `has_block=false`) / `anonymous_function`.
- **Swift** — `class_declaration` (umbrella class / struct / enum /
  actor / extension via the `declaration_kind:` field — `extension`
  surfaces the extended type as ident) / `protocol_declaration` /
  `function_declaration` / `property_declaration` (pattern-based name) /
  `typealias_declaration` (uses `name:` — distinct from Kotlin's `type:`)
  / `enum_entry` / `associatedtype_declaration` /
  `protocol_function_declaration` + `protocol_property_declaration`
  (body-less, `has_block=false`) / `init_declaration` /
  `deinit_declaration` / `subscript_declaration` /
  `operator_declaration` / `lambda_literal`.
- **PHP** — `class_declaration` / `interface_declaration` /
  `trait_declaration` / `enum_declaration` (PHP 8.1+) /
  `function_definition` / `method_declaration` (abstract methods
  report `has_block=false`) / `property_element` (granular per-name
  emit so `public $a, $b` yields one skeleton per element with the `$`
  sigil stripped) / `const_element` / `enum_case` /
  `namespace_definition` (block-form vs semicolon-form separated by
  `has_block`) / `anonymous_function` / `arrow_function`
  (`has_block=false` — expression body) / `anonymous_class`.
- **Ruby** — `class` / `module` / `method` / `singleton_method`
  (surfaces the method name only, not the receiver) / `singleton_class`
  (anonymous `class << self` block) / `alias` (returns the new alias
  name via the `name:` field) / `lambda` / `block` (brace form) /
  `do_block`. Endless methods (`def foo() = 42`) report
  `has_block=false` since the body is an `_arg` rather than a
  `body_statement`.

### Added — `has_body_block` markers

Cross-language body kinds expand to cover the new allowlist entries:
`enum_class_body` (Kotlin + Swift), `protocol_body` (Swift),
`enum_declaration_list` (PHP), `body_statement` + `block_body` +
`do_block` (Ruby). `function_body` is now shared across SQL, Kotlin,
and Swift — pinned by a cross-grammar regression test.

### Added — `--why` extended to usages / similar / duplicates (Phase 11.10)

Three new structured-trace shapes alongside the existing `search` /
`pattern` traces, emitted as one-line JSON on stderr and surfaced
via `_meta.why` in MCP responses. See `docs/MCP-SCHEMA.md` for the
canonical vocabulary table.

- `usages --why` — `mode` (`"strict"` / `"text_scan"`),
  `hits_before_filter` / `hits_after_filter`, `prefix_suggestions`
  (count from the `Did you mean` fallback when zero exact hits),
  `filter_applied`.
- `similar --why` — `seed_resolved`, `threshold_applied`,
  `candidates_before_filter` / `candidates_after_filter`,
  `filter_applied`. When `--why` is on, the handler over-fetches
  (`fetch_limit = symbol_count()`) so the pre-filter count reflects
  the un-truncated HNSW return list rather than an internally-capped
  one.
- `duplicates --why` — `threshold_applied`, `min_body_lines_applied`,
  `pairs_before_filter` / `pairs_after_filter`, `filter_applied`.
  Same over-fetch logic as `similar`.

MCP schemas for all three tools gained a `why: boolean` field;
`extract_why_trace` continues to pick up the trace via stderr.

### Fixed

- **Skeleton walker now gates emission on `node.is_named()`.** Ruby is
  the first language whose grammar reuses kind strings between named
  rules and anonymous keyword tokens (`class`, `module`, `alias`).
  Without the gate, every Ruby declaration site would emit a second
  phantom skeleton from the keyword token. The gate is safe across
  T1/T2a because every allowlisted kind is a named grammar rule;
  pinned by the Ruby happy-path tests.
- **Arrow-lambda `do/end` body now triggers `has_block=true` for Ruby.**
  `->(x) do ... end` puts a `do_block` directly under `lambda`;
  `do_block` joined the universal body markers so the contract
  mirrors Kotlin `anonymous_function`.

### Other

- `t2_language_returns_empty_until_rolled_out` canary repointed
  permanently from Ruby → TOML (a T3 anchor we never plan to fill);
  pairs with `t3_language_short_circuits_to_empty` on YAML.
- Phase 11 status: **all 10 items complete** (11.1 type-aware refs,
  11.2 diff, 11.3 generic implementations, 11.4 structural patterns +
  T2 train, 11.5 multi-hop, 11.6 metadata filters, 11.7 scope filters,
  11.8 similar reasoning, 11.9 result-kind ranking, 11.10 MCP arg +
  `--why`).

## [1.8.1] - 2026-05-23

Phase 11.4 T2 pattern-skeleton rollout — eight new languages graduate
from the empty-allowlist short-circuit to full prefilter coverage —
plus a cross-file scope-binder follow-up for C# `using` and C++ `using`
declarations. No format change; v6 stays compatible.

### Added — pattern skeletons populated for

- **Go** — `function_declaration` / `method_declaration` / `type_spec`
  / `type_alias` / `var_spec` / `const_spec` / `func_literal`.
- **C++** — `function_definition` (with declarator-chain ident reused
  from the scope binder), `class_specifier` / `struct_specifier` /
  `union_specifier` / `enum_specifier` / `namespace_definition` /
  `template_declaration` (anonymous wrapper) / `alias_declaration` /
  `type_definition` / `lambda_expression`.
- **C#** — `class_declaration` / `interface_declaration` /
  `struct_declaration` / `enum_declaration` / `record_declaration` /
  `method_declaration` / `constructor_declaration` /
  `destructor_declaration` / `property_declaration` /
  `delegate_declaration` / `local_function_statement` /
  `namespace_declaration` / `file_scoped_namespace_declaration` /
  `lambda_expression` / `anonymous_method_expression`.
- **SQL** — `create_table` / `create_index` / `create_view` /
  `create_materialized_view` / `create_function` / `alter_table` /
  `drop_table` / `drop_view` / `drop_function` / `drop_index`.
- **Markdown** — `atx_heading` / `setext_heading` /
  `fenced_code_block` (language tag as ident).
- **Java** — `class_declaration` / `interface_declaration` /
  `enum_declaration` / `record_declaration` /
  `annotation_type_declaration` / `method_declaration` /
  `constructor_declaration` / `compact_constructor_declaration` /
  `lambda_expression`.
- **CSS** — `rule_set` (full selector chain as ident) /
  `keyframes_statement` / `media_statement` (anonymous).
- **HTML** — `element` / `script_element` / `style_element` with
  `tag_name` extraction from `start_tag`.

JavaScript already uses the TypeScript grammar (the extension map
routes `.js` / `.jsx` to `Language::TypeScript`), so its skeletons
were covered by the T1 TypeScript allowlist from v1.8.0 on — no
separate row needed.

### Added — cross-file scope binders (`vex usages --strict`)

C# and C++ now resolve import bindings across files, joining Rust /
TypeScript / Python:

- **C#** — `using A.B.C;`, `using static A.B.C;`,
  `using Alias = A.B.C;`, `global using A.B.C;`, and
  `using global::A.B;` (the `global::` qualifier is stripped from
  the resolved path).
- **C++** — `using std::vector;`, `using V = std::vector<int>;`
  (template arguments stripped from the path), and
  `namespace alias = ns::sub;`.

`#include` and `using namespace` stay deferred as wildcard imports
(no name-keyed `UsePath` representation).

### Fixed

- `is_root_kind` is now language-aware: `document` is suppressed for
  Markdown and HTML (both use it as the file root) but not for YAML
  (which uses it as a non-root subtree under `stream`) — prevents a
  silent `parent_kind` corruption when YAML is eventually rolled out.
- C# top-level decls no longer leak `parent_kind=Some("compilation_unit")`
  (added `compilation_unit` to the suppression set).
- C++ root-namespace `using ::Foo;` no longer produces a doubled-up
  `UsePath` (`["Foo", "Foo"]`); C# `using global::App.Lib.X;` no
  longer corrupts the first path segment.

### Other

- `tests/pattern_fixtures/**` is force-LF via `.gitattributes` so the
  multi-line `$$$BODY` capture pins don't break on Windows checkouts
  with `core.autocrlf=true`.

## [1.8.0] - 2026-05-22

Combined "type-aware usages + first-class structural patterns"
release — two large trains landed back-to-back:

- **Phase 11.1 (type-aware `usages`)** — LSP-style scope binder for
  Rust, TypeScript, Python, C#, and C++ with a persisted v5
  `reference_edges` section. Default `vex usages` is unchanged; the
  precision upgrade is opt-in via `--strict`.
- **Phase 11.4 (first-class structural patterns)** — `vex pattern`
  promoted from cold live-scan to indexed search via a persisted v6
  `pattern_skeletons` section, with multi-line `$$$BODY` / `$$ARGS`
  metavars and `&&` / `||` composition. All existing patterns keep
  working; the new syntax is additive.

`MIN_SUPPORTED_VERSION` stays at 3 — v3 / v4 indexes still open and
auto-rebuild on first use after upgrade.

### Format change — v6 (Phase 11.4)

- **`VERSION = 6`** with a new `PatternSkeletonHeader` (168 bytes)
  immediately after `V5SectionHeader`. Carries offsets and lengths for
  four sub-sections — `SkeletonRecord` array (24 B records sorted by
  `file_id`), `kind_path_arena` (inline null-terminated kind names
  plus per-record path entries), `ident_pool` (length-prefixed UTF-8),
  `file_index` (sorted `{file_id, first_skel_idx}` for O(log n)
  `skeletons_for_file`) — plus a `[u32; 32]` per-language
  `grammar_fingerprints` array. (11.4 Inc 3)
- **`MIN_SUPPORTED_VERSION` stays at 3** — v3/v4/v5 indexes still
  open; `vex pattern` falls back to live-scan on them with `--why`
  reason `"no-skeleton-section"`. New indexes auto-rebuild on first
  use after upgrade.
- **`Language::lang_id() -> u8`** with explicitly-assigned stable IDs
  (Rust=1..Toml=19). Slot 0 is reserved for "not fingerprinted".
  Adding a new language gets the next integer; removing one leaves a
  reserved gap so the on-disk fingerprint slot index stays stable.
- **Adversarial coverage**: 3 new tests in `tests/adversarial_format_-
  test.rs` cover the v6 header gate (`pattern_skeleton_header_-
  has_nonzero_size`), backward-compat (`v5_index_has_no_pattern_-
  skeleton_header`), and truncation
  (`v6_truncated_at_pattern_skeleton_header_rejected`).

### Added — pattern syntax (Phase 11.4)

- **Named multi-line metavars**: `$$$BODY` (block-spanning) and
  `$$ARGS` (arg-list-spanning). Functionally identical — the two
  syntaxes coexist for readability (`fn $F($$ARGS) { $$$BODY }` reads
  naturally). Both capture and enforce back-reference equality on
  repeat occurrences, just like single-token `$NAME`. (11.4 Inc 6)
- **AND / OR composition** via space-flanked ` && ` and ` || ` at
  bracket / quote depth 0. AND requires both sub-patterns to match in
  the same file with consistent captures across shared metavars; OR
  takes the union, deduped by `(path, line)`. `&&` binds tighter than
  `||` (standard). The depth-aware split keeps `record($X, $X)` and
  `f($X && $Y)` as single patterns. Empty middle conjuncts/disjuncts
  bail with a clear "empty pattern" error. (11.4 Inc 7)
- **Greedy `>` fix for `Result<$T, $E>`-shape patterns**: the
  identifier scanner now stops one byte short of the next literal's
  starting character, so `$E` correctly captures `Error` instead of
  `Error>`. Pre-existing limitation, surfaced by the Inc 6 fixtures.

### Added — scan modes (Phase 11.4)

- **Indexed prefilter**: when a v6 index is present and the per-lang
  grammar fingerprint matches the live grammar, `vex pattern` walks
  only the candidate files whose persisted skeletons satisfy the
  language and (when inferable from the pattern's leading keyword)
  the root node-kind. Falls back to live-scan with explicit reasons
  — `"no-index"`, `"index-open-error"`, `"no-skeleton-section"`,
  `"empty-section"`, `"grammar-drift"`, `"partial-section"` — none
  of which is fatal. (11.4 Inc 5)
- **`--why`** on `vex pattern`: appends a JSON `ScanTrace` to stderr
  with `mode` (indexed / live_scan), `root_kind_inferred`,
  `candidate_files` / `total_files`, and the optional
  `fallback_reason`. The same trace surfaces under `_meta.why` in the
  MCP response. (11.4 Inc 5 + Inc 8 wrapper fix)
- **Root-kind inference** strips Rust visibility (`pub`,
  `pub(crate)`, `pub(super)`), Rust `async` / `unsafe` / `const`
  modifiers, TS `export` / `default` / `async`, and Python `async`
  before matching the keyword — so `pub async fn $F` infers
  `function_item` instead of silently disabling the prefilter.
- **`partial-section` fallback after `vex update`**: incremental
  builds leave skeletons empty for unchanged files (mirroring the
  `bound_refs` / `call_edges` convention). The new
  `Manifest.pattern_index_full` flag distinguishes a full `vex index`
  from a `vex update`; when `Some(false)` the prefilter degrades to
  live-scan to avoid silently under-reporting matches.

### Added — CLI / MCP (Phase 11.4)

- **`--no-pattern-index`** on `vex index`, `vex update`, and `vex
  watch` skips the v6 skeleton section. Same sticky semantics as
  `--no-call-graph` / `--no-bm25` — opt-out is recorded in
  `Manifest.pattern_index` and honoured by subsequent `vex update`
  unless explicitly overridden. `.vex.toml` gains `pattern_index =
  false` for project-wide opt-out. (11.4 Inc 4)
- **MCP `pattern` tool** schema documents Inc 6-7 syntax (block
  metavars, composition, indexed prefilter) and gains a `why:
  boolean` field. When `why: true` the wrapper extracts the
  `ScanTrace` JSON from the CLI's stderr and places it under
  `_meta.why` in the JSON-RPC response. The same plumbing benefits
  the existing `search --why` MCP flow. (11.4 Inc 8)

### Internal (Phase 11.4)

- **Per-file skeleton extractor** (`src/pattern/skeleton.rs`): pure
  function that walks tree-sitter once per file and emits a
  `Skeleton` per pattern-targetable node. T1 allowlists for Rust (10
  kinds), TypeScript (8), and Python (4). T2 / T3 languages
  short-circuit to `Vec::new()` so the section is empty for files
  that don't participate; `vex pattern --lang <x>` still works via
  live-scan for those. (11.4 Inc 2)
- **Grammar fingerprints** = `xxh3_64(concat(kind_name, 0; …))`
  truncated to `u32`, computed at index time per T1 language present
  in the parsed set. Stored in the `PatternSkeletonHeader`
  fingerprint array. The reader compares against the live grammar's
  fingerprint and falls back to live-scan on mismatch — closes the
  R-A grammar-drift hazard surfaced by the planner.
- **`CompositePattern { disjuncts: Vec<Vec<PatternTree>>, lang }`**
  with helpers `parse_composite_pattern`, `split_top_level`,
  `find_matches_composite`, `match_conjunct`, `captures_agree_-
  with_map`, `build_normalised_map`. The map projection is built once
  per `(anchor, other_tree)` pair instead of per candidate, avoiding
  the O(anchors × trees × candidates × |captures|) HashMap rebuild
  that the first cut had.
- **Writer entry sprawl** (transitional): `write_index_with_call_-
  graph_and_skeletons_and_fingerprints` is the new primary entry.
  Two back-compat shims (`write_index_with_call_graph`,
  `write_index_with_call_graph_and_skeletons`) forward to it for
  legacy and test callers; both are flagged `#[allow(dead_code)]`
  pending a future consolidation pass.
- **Fixture-based regression suite** (`tests/pattern_fixtures/`): one
  baseline + five scope-B fixtures across Rust / TypeScript /
  Python. Each fixture is `input.<ext>` + `spec.toml` (pattern,
  expected lines, expected captures); the harness in
  `tests/pattern_fixture_test.rs` runs `vex pattern --format json`
  per fixture and asserts. RED in Inc 1, all GREEN after Inc 6+7.


### Format change — v5 (Phase 11.1)

- **`VERSION = 5`** with a new `V5SectionHeader` (48 bytes) immediately
  after `CallGraphHeader`. The header carries offsets and lengths for
  three new sub-sections: `reference_edges` (fixed-size 16-byte `RefEdge`
  records), an FST keyed on stringified `to_sym_idx`, and posting lists
  of edge indices. (11.1.3a–b)
- **`MIN_SUPPORTED_VERSION` stays at 3** — v3/v4 indexes still open;
  `vex usages --strict` bails on those with a "re-run `vex index`"
  message. New indexes auto-rebuild on first use after upgrade.
- **Adversarial coverage**: 4 new tests in `tests/adversarial_format_-
  test.rs` cover the V5SectionHeader truncation arm and each of the
  three sub-section past-EOF arms; `tests/integration_test.rs` pins
  cross-file `Imported` resolution for Rust, TypeScript, and Python.

### Added — scope binders (Phase 11.1)

- **AST-aware ref extraction** (11.1.1): identifiers inside line / block
  comments, doc-strings, and string literals are filtered out before
  they enter the refs FST for Rust, TypeScript, Python, C#, and C++.
  Drops the loudest false-positive class from `vex usages` without
  touching the format.
- **Per-language scope binders** (11.1.2 through 11.1.6) for Rust,
  TypeScript, Python, C#, and C++. Each binder builds a `ScopeTree`
  arena and emits `BoundRef` records tagged with `BindTarget::Local`,
  `BindTarget::ModuleSymbol`, `BindTarget::Imported`, or
  `BindTarget::Unresolved`. Local + mod-level resolution shipped per
  language; cross-file `use` / `import` resolution shipped for Rust /
  TypeScript / Python. C# `using` and C++ `#include` remain deferred.
- **Shared `Walker` scaffolding** (`src/parse/scope/walker.rs`):
  single 169-line module that owns `Walker`, `add_binding`,
  `add_import_binding`, `emit_ref`, `resolve`, and `walk_children`.
  Each language file is the dispatch + grammar-specific helpers.

### Added — CLI / MCP (Phase 11.1)

- **`vex usages --strict`** reads the persisted `reference_edges`
  section. Without `--strict` the legacy refs FST keeps backing the
  command (covers the 14 languages without a binder yet). MCP `usages`
  tool schema gained `"strict": { "type": "boolean", "default": false }`.
- **`vex implementations` finds generic-parameterised subclasses**
  (11.1.6): `impl Iterator<T> for Foo` (Rust), `implements Handler<T>`
  (TypeScript), `class Box(Container[int])` (Python), and
  `: public Repository<T>` (C++) now match alongside the bare-name
  forms. Java + C# already had the band-aid since the 11.7 train.

### Internal (Phase 11.1)

- Pass-2 cross-file resolution at index-write time. `BindTarget::-
  Imported(use_path)` records are rewritten to `ModuleSymbol(global_-
  idx)` via a `HashMap<&str, Vec<u32>>` built from `sym_entries` —
  first-hit on ambiguity, matching the documented behaviour.
- `Walker::file_symbols_by_name: HashMap<&str, u32>` replaces the
  O(R × S) linear scan in `resolve()`. Closes a rust-reviewer MEDIUM
  before binders ran across an entire repo at index time.
- `debug_assert!` hardening on `RefEdge::col_and_kind` 24-bit column
  ceiling and `base_idx` overflow in the writer's Pass-2 — silent
  corruption in the unreachable 4 B-symbol case is now a loud failure
  in tests.

## [1.7.0] - 2026-05-21

The "search ergonomics + trust gaps" release. Built around an honest
audit of where users still had to cross-check vex with `ast-index` or
`Grep` — nine point features across the three pain clusters (filter
power, structural understanding, agent ergonomics) plus security
hardening on the embedding pipeline. No format change; all v1.6.x
indexes continue to open.

### Added — filter power

- **`--include <glob>` / `--exclude <glob>`** on every search-shaped
  command (`search`, `usages`, `pattern`, `show`, `grep`,
  `implementations`, `callers`, `callees`, `similar`, `duplicates`).
  Repeatable, gitignore-style semantics via `globset`
  (`literal_separator(true)` — `*` stays within a segment, `**`
  crosses `/`). Per-call scoping that doesn't require re-indexing;
  composes AND with the existing `--filter <substring>`. MCP tools
  carry the matching `include` / `exclude` arrays. Hardened against
  CPU exhaustion with a 64-pattern / 256-char-per-pattern cap. (11.7)
- **Multi-value `--kind`** with comma + repeated values:
  `--kind fn --kind struct` or `--kind fn,struct`. Four new
  meta-selectors (`def`, `comment`, `test`, `ref`) join the
  canonical SymbolKind names. Default ranking now demotes Markdown
  headings unless the user explicitly opts in via `--kind comment` —
  defs-first ordering in mixed result sets. (11.9)
- **Symbol metadata post-filters**: `--visibility public|private|
  protected|internal`, `--async-only` / `--no-async`,
  `--static-only`, `--sealed-only` on `vex search` / `vex show`.
  Lexical match against the symbol's captured signature line — no
  format bump, no re-parsing. Per-language alias coverage: `pub` /
  `public` / `export`; `async` and Kotlin `suspend`; `sealed`
  (Kotlin/C#) and Java `final class` (gated on co-occurring type
  keyword so method parameters don't false-positive). Default-
  visibility inference is deliberately not done — only explicit
  keywords match. (11.6)

### Added — structural understanding

- **`vex paths <from> <to>`** enumerates all caller chains over the
  v4 persistent call graph from 1.5.0. DFS-backward from `to` with
  per-chain cycle prevention, bounded by `--max-hops` (default 6)
  and `--max-paths` (default 50). Output reverses to natural
  `A → … → B` reading order. (11.5)
- **`vex reachable <target>`** returns the transitive set of
  symbols whose callees reach `target`, with the BFS depth label
  per row. Same call-graph fast path, bounded by `--max-hops` and
  `--limit`. Both commands warn loudly when a node hits the
  per-step fetch cap (1024 callers) so wide-fan-in saturation is
  visible. (11.5)
- **`vex diff --base <rev>`** lists symbol-level changes between
  an arbitrary git revision and the working tree: added / removed /
  moved-within-file / body-changed entries. Per-file pairing by
  `(name, kind)` with a coarse line-range body hash; deliberately
  uses `git diff --no-renames` so a `git mv` surfaces both halves
  of the move. Available as MCP tool too. (11.2)
- **Generic-parameterised `implementations`** for Java
  (`extends Repository<T>`), C# (`: Repository<T>`), and Kotlin
  (`class Foo : Repository<T>()`). The Kotlin inheritance query
  was rewritten end-to-end — the v1.4.x version used wrong node
  names (`type_identifier` vs the actual `identifier`,
  `delegation_specifier_list` vs `delegation_specifiers`) and
  silently matched nothing on `tree-sitter-kotlin-ng`. (11.3)
- **`vex pattern` metavar back-references**: a repeated `$NAME` in
  a pattern now requires the same capture across occurrences.
  `record($X, $X)` matches `record(state, state)` and rejects
  `record(state, other)`. Whitespace-normalised on the back-ref
  comparison so `assertEqual((a + b), (a+b))` unifies on
  formatting differences. New MCP tool exposes the command. (11.4)

### Added — agent ergonomics

- **`--explain` on `vex similar` / `vex duplicates`**: emits an
  identifier-set Jaccard overlap plus a truncated unified diff
  (via `similar` crate) between the seed and each match. Cosine
  scores already surfaced; the diff + jaccard close the
  "I never act on these blind" gap. `--min-score` alias for
  `--threshold` for discoverability. (11.8)
- **`--why` / MCP `why: true`** on `vex search` appends a JSON
  trace to stderr: normalised query, per-channel hit counts
  (FST / BM25 / semantic), fallbacks engaged (e.g. fuzzy),
  and the active filter snapshot. Useful when results look wrong
  and you want to know what was actually searched. (11.10)
- **MCP schema canonical vocabulary**: `query` / `symbol` /
  `symbols` / `path` / `pattern` / `filter` / `include` / `exclude`
  across all tools. Renames with back-compat — old `name` /
  `file` / `names` aliases still work and emit a
  `_meta.deprecated_args: [...]` notice in the JSON-RPC response.
  New `docs/MCP-SCHEMA.md` documents the vocabulary and back-compat
  policy. (11.10)

### Fixed / Hardened

- **Pinned SHA-256 integrity check on the MiniLM ONNX model**.
  `fastembed` / `hf_hub` previously downloaded the ~86 MB ONNX
  with no signature in the supply chain; a poisoned CDN entry or
  compromised HF host would have landed arbitrary ONNX into a
  process that executes it. The expected digest
  (`bbd7b466…46f0c5` at fastembed snapshot
  `5f1b8cd7…3af89a079`) is now verified after fastembed init.
  `VEX_EMBEDDER_SKIP_CHECK=1` escape hatch with both an
  `eprintln!` and `tracing::warn!` on bypass so it's visible
  regardless of `RUST_LOG`. Multi-snapshot caches pick
  deterministically by lex-sorted directory name. (10.4)
- **MCP `vex {sub} failed` error now includes a truncated stdout
  snippet**. The vex CLI emits its structured JSON-error body on
  stdout, not stderr — the previous wrapper was silently dropping
  the actual reason in the JSON-RPC response. Capped at 512 bytes
  with `floor_char_boundary` UTF-8-safe slicing. (10.6 Pass 2)
- **`vex outline` grammar-load failure** now includes the language
  variant alongside the file extension
  (`failed to load grammar for csharp (.cs): …`). (10.6 Pass 2)
- **README troubleshooting section** documents `RUST_LOG=vex=warn`
  for surfacing parse/store warnings, points at
  `vex search Foo --why 2>trace.json` and `docs/MCP-SCHEMA.md`.
  (10.6 Pass 2)
- **`vex diff` distinguishes "file did not exist at base" from
  real git failures** (corrupt object store, permission denied).
  The previous handler `.ok()`-swallowed every non-zero exit code,
  so a broken repo silently reported every symbol as `Added`. Now
  the missing-object path returns `Ok(None)` and everything else
  bubbles up with stderr context. (11.2 fixup)
- **Java's `final` keyword on method parameters** no longer
  false-positives into `--sealed-only`. The metadata filter now
  requires a type-introducing keyword (`class`, `interface`,
  `enum`, `record`) to co-occur with `final` before treating the
  signature as sealed. (11.6 fixup)
- **MCP `async_only` + `no_async`** sent together previously
  surfaced clap's generic `conflicts_with` error template in the
  JSON-RPC response. Explicit `bail!` guard in `push_metadata`
  produces an intent-aware error instead. (11.6 fixup)

### Internal / Docs

- New `docs/MCP-SCHEMA.md` — canonical vocabulary, alias table,
  `_meta.deprecated_args` contract, and the `--why` trace shape.
- New modules: `src/search/explain.rs` (jaccard + truncated
  unified diff), `src/search/metadata.rs` (signature-line filter),
  `src/search/trace.rs` (post-hoc search trace builder),
  `src/callgraph/bfs.rs` (closure-abstracted multi-hop layer),
  `src/cli/scope.rs` (glob compilation with caps),
  `src/diff/mod.rs` (symbol-level diff against a git revision),
  `src/embed/integrity.rs` (SHA-256 verification + cache walker).
- 5 rust-reviewer passes + 1 security-reviewer pass + 1 aqa-agent
  pass across the train; ~30 reviewer-flagged items applied as
  in-line fixes or new regression tests. Two fixup commits
  (`ed92f7a`, `a7295d2`) bundle the cross-cutting polish.
- New runtime dependencies: `globset = "0.4"` (was transitive via
  `ignore`), `similar = "2"` (was transitive via `insta` dev-dep),
  `sha2 = "0.10"` (was transitive). All small, leak-clean,
  audited crates.
- ~120 new tests across the train (unit + integration). Full
  workspace ~360 tests; clippy clean under `-D warnings`;
  fmt clean; rust-version 1.80 MSRV preserved.

## [1.6.4] - 2026-05-19

Feature + UX polish release. Headline change is the new indexing-time
opt-out flags (`--no-call-graph` / `--no-bm25`) that let monorepos and
CI hot-paths skip the call-graph and BM25 build steps that v1.5.0
added unconditionally — recovering most of the v1.4.3-era indexing
speed when the user does not need those search channels.

### Added
- **`vex index --no-call-graph` / `--no-bm25`** (also on `update` and `watch`). v1.5.0 made `vex index` ~6× slower than v1.4.3 because the persistent call-graph and BM25 sections were always built. Users who only need structural + vector search can now opt out per-build or globally via `.vex.toml`. With `--no-call-graph` set, `vex callers` / `vex callees` transparently fall back to the live tree-sitter scan they always supported; with `--no-bm25`, hybrid search drops the BM25 channel and uses structural (+ semantic if enabled). The opt-out is recorded in the manifest so a subsequent `vex update` does not silently re-add the section.
- **`.vex.toml` keys `call_graph` and `bm25`** for project-wide opt-out. Resolution precedence: CLI `--no-...` flag > `.vex.toml` > previous manifest > default (true).
- **`vex callers` / `vex callees` accept `--auto-update` and `--no-stale-check`**, symmetric with the other index-backed commands. With `--auto-update`, a missing index is bootstrapped on first call so the persistent call-graph FST (~4 ms) becomes available immediately; the existing live-scan fallback is preserved when no index and no auto-update are in play. MCP wrappers gained the matching `auto_update` schema field.

### Fixed
- **`auto_update`-driven bootstrap no longer re-runs the staleness probe** on the freshly built index. A just-bootstrapped manifest is guaranteed `Freshness::Fresh`, so the manifest read + git HEAD probe that fired immediately after the bootstrap was pure waste. Behaviourally invisible; just trims the first-search latency on `auto_update = true` projects.
- **`IndexReader::open` errors now carry the index file path** in every `bail!` and `File::open`/`Mmap::map` context. Previously a corrupt or version-mismatched index surfaced as a path-less "index file is corrupted (bad magic)" — users had to grep stderr or read source to find the file. Same treatment for the embedder-mismatch and stale-embedder-switch bails (now include the manifest path), and the cache-dir `..`-traversal warning (now shows both the raw config value and the tilde-expanded form).
- **`vex callers` / `vex callees` live-scan fallback reason is now printed via `eprintln!`** when an index exists but fails to open. Previously logged via `tracing::warn!` which `RUST_LOG` hides by default — users saw the ~seconds live-scan latency without knowing why the fast path was skipped.
- **Bootstrap `pipeline::run` failure now includes the project root** via `.with_context()`. Bare pipeline errors during first-run bootstrap no longer drop the root path.
- **CI release-zip Deflate guard** introduced in v1.6.3 contained an f-string syntax error inside a PowerShell here-string that crashed on the next tag push. The dict lookup is now pulled into a local variable so the f-string only references a bare identifier. (Affects only the release pipeline; published binaries on v1.6.3 are unchanged.)

### Internal
- New `pipeline::IndexOptions { with_embeddings, with_call_graph, with_bm25 }` struct replaces the bare `with_embeddings: bool` parameter on `pipeline::run` / `pipeline::update`. Default is `with_embeddings = false, with_call_graph = true, with_bm25 = true`. All seven library-test callsites migrated to `IndexOptions::default()`.
- `Manifest` gains `call_graph: Option<bool>` and `bm25: Option<bool>` fields (back-compat: `None` on pre-10.4 manifests is treated as enabled).
- New `resolve_section_enabled(cli_no_flag, cfg_value, manifest_value) -> bool` helper in `cli::mod` encodes the precedence rule in one place.
- `ensure_index_exists` now returns `IndexAvail { path, just_bootstrapped }` instead of bare `PathBuf`. A new `ensure_index_ready` wrapper composes ensure + staleness, skipping the latter on a fresh bootstrap. Six existing call sites (search/show/usages/check/similar/duplicates) collapsed from a pair of calls to a single helper.
- `cmd_callgraph` gains `auto_update`, `no_stale_check`, `local_cache_active`, `cfg` parameters and a bootstrap-or-staleness block that runs before the existing fast-path / live-scan branching. The live-scan fallback when neither an index nor auto-update is available is preserved exactly.
- `Update` / `Watch` arms canonicalize `root` once at the top of the arm and `?`-propagate `Manifest::load` errors so a partially-written manifest does not silently re-enable opted-out sections.
- 11 new tests: 4 integration tests for callers/callees auto-update bootstrap and live-scan fallback, 4 integration tests for the opt-out persistence + sticky-update invariant, 2 behavioural tests proving callers/search still work with the opt-out, 1 unit test suite (4 cases) for `resolve_section_enabled` covering all precedence levels.

## [1.6.3] - 2026-05-19

Windows-only patch for the `vex self-update` failure reported on v1.6.2.

### Fixed
- **Windows zip archive now uses standard Deflate (method 8) instead of Deflate64.** PowerShell's `Compress-Archive` on the `windows-latest` runner emits Deflate64 in some configurations — .NET can decompress it, but the `zip` crate inside `self_update 0.42` cannot, surfacing as `Compression method not supported` when `vex self-update` tries to extract the new release. Release packaging now invokes `7z a -tzip -mm=Deflate` (preinstalled on `windows-latest`) which forces compatible compression. zipsign continues to preserve the method, so signed archives are also Deflate-compressed end-to-end.
- **Migration**: v1.6.1 → v1.6.2 self-update on Windows failed for users on v1.6.1 trying to land on v1.6.2 (and v1.6.0 → v1.6.1 in retrospect was likely affected too). v1.6.2 → v1.6.3 should self-update normally because v1.6.3 ships standard Deflate, which the existing v1.6.2 binary can read. If `vex self-update` from v1.6.2 still fails for any reason, download `vex-x86_64-pc-windows-msvc.zip` from the v1.6.3 release page once — from 1.6.3 onwards updates are clean.

## [1.6.2] - 2026-05-19

UX/perf hotfix on top of v1.6.1, motivated by Windows users hitting two
sharp edges as soon as they tried `auto_update = true` in `.vex.toml`.

### Fixed
- **`auto_update = true` now bootstraps a missing index on first use.** The previous code path bailed with "No index found" before the auto-update logic ever ran, and `handle_staleness` could only do incremental updates anyway. A bare `vex search` / `show` / `usages` / `check` / `similar` / `duplicates` in a fresh project will now print "Bootstrapping…" and run the equivalent of `vex index` before continuing. `Similar` and `Duplicates` bootstrap with `--semantic` automatically since they require embeddings
- **Improved "no index" error message** when `auto_update` is *not* set — now lists both fixes: run `vex index` or set `auto_update = true` in `.vex.toml`
- **MiniLM embedding model is now cached globally**, not in `./.fastembed_cache/` relative to the working directory. The ~86 MB ONNX model lives at `<platform-cache>/vex/embeddings/` and is shared across every project. Previously every project re-downloaded the same model and dropped a `.fastembed_cache/` folder into the project tree. Existing `.fastembed_cache/` directories inside user projects can be deleted safely — the model will not be re-downloaded
- **`local_cache = true` + bootstrap now writes the `.gitignore` safeguard**. The auto-bootstrap path previously skipped the project-`.gitignore` creation that the explicit `vex index` command performs, so users with both flags set could end up committing the binary index. Now the bootstrap mirrors the `Commands::Index` behaviour
- **Semantic bootstrap announces the model download.** `vex similar` / `vex duplicates` on a fresh machine print "Note: first semantic index downloads the MiniLM ONNX model (~86 MB)…" before fastembed's progress bar appears
- **Clearer embed cache errors.** `MiniLMEmbedder::new` now wraps `create_dir_all` with `.with_context(...)`, so a failed cache-dir create surfaces with the actual path instead of hiding behind fastembed's generic "failed to load MiniLM" message

### Fixed (CI)
- Replaced `orhun/git-cliff-action@v3` with a direct install via `taiki-e/install-action`. The Docker image used by the action was based on debian:buster which is EOL — `apt-get update` returned 404 and the release job failed before the body could be generated. v1.6.1 release shipped via this fix; documenting here for the changelog trail

### Internal
- New `ensure_index_exists(root, auto_update_flag, needs_semantic, local_cache_active, cfg)` helper in `cli::mod` replaces the same six-line `if !index_path.exists() { bail!(...) }` block that appeared in every index-backed command. Returns the resolved `index.vex` path so callers do not recompute it
- New `util::config::embed_cache_dir()` returns `<cache-root>/embeddings/` without appending the project-hash subdir — every project that shares the platform cache root also shares the model. Users on `local_cache = true` consciously opt in to a portable layout and pay the per-project cost
- 9 new CLI integration tests in `tests/cli_bootstrap_test.rs` drive the actual binary via `assert_cmd`. Cover the auto-bootstrap path, the helpful-error path, the `.gitignore` safeguard, path-traversal rejection end-to-end, the `--no-stale-check` interaction, and the `--check` / `--yes` mutual exclusion. Each test scopes its cache to a tempdir so the shared platform cache is never touched

## [1.6.1] - 2026-05-18

Follow-up patch for v1.6.0. Adds the `vex self-update` subcommand that
Windows users needed (no Homebrew tap), and fixes the two Windows path
issues that surfaced once CI started running on `windows-latest` for the
first time.

### Added
- **`vex self-update` subcommand** — fetches the latest GitHub release, picks the archive for the running target triple, and replaces the binary in place. Works on Linux, macOS, and Windows. `--check` reports the latest version without modifying anything; `-y/--yes` skips the interactive confirmation. Closes the upgrade-flow gap on platforms without a package manager
- **Windows install section in README** — step-by-step for downloading `vex-x86_64-pc-windows-msvc.zip`, extracting `vex.exe`, and putting it on `PATH`

### Fixed
- **`is_test_path` on Windows** — the test-file down-rank heuristic in `search::rerank` checked for `/test/`, `/tests/`, etc. as substrings. Indexed paths preserve the host separator, so on Windows the heuristic silently never matched and test files received no rank penalty. Now normalizes backslashes before the scan, allocating the extra string only when needed
- **`grep::tests::grep_with_path_filter`** asserted `matches[0].path.contains("api/")` which fails on Windows (paths are `api\routes.py`). Assertion now contains "api" without the trailing slash, OS-agnostic

### Security
- **ed25519 signature verification** for `vex self-update`. Every release archive is signed in CI via [`zipsign`](https://github.com/Kijewski/zipsign); the 32-byte verifying key is embedded in the binary at compile time. The download path refuses to extract an archive whose signature does not match. Mitigates the "compromised GitHub account / CDN tamper" scenario in the v1.6.0 self-update threat model
- `vex self-update` continues to use rustls (no openssl C dep), enforces certificate validation by default, and refuses to apply a downgrade
- Key rotation and one-time setup documented in [`docs/RELEASING.md`](docs/RELEASING.md)

### Internal
- `--check` and `-y/--yes` are mutually exclusive at the clap layer (the combination would have silently dropped one flag)
- `self_update::version::bump_is_greater` parse failures surface via `?` instead of `.unwrap_or(false)` so an unexpectedly-tagged release is reported as an error, not a silent "no action needed"
- Named-argument comments at the `cmd_self_update` call site guard against a future refactor swapping the two `bool` positionals

## [1.6.0] - 2026-05-18

Configurable cache location, first-class Windows support, and a thread-count
limit for parallel indexing. The original trigger was a Windows-only MCP
bug — `dirs_home()` had no Windows branch and fell back to `/tmp`, producing
mangled paths like `/tmp\.cache\vex\<hash>`. Fixing that path resolver turned
into a broader rework of cache management.

### Added
- **`--cache-dir <PATH>` global flag** plus `$VEX_CACHE_DIR` env var to override the cache root per-invocation
- **`cache_dir = "..."` in `.vex.toml`** — accepts absolute paths, `~/...`, and paths relative to the config file (e.g. `"./.vex/cache"`)
- **`local_cache = true` in `.vex.toml`** — store the index at `<project>/.vex_cache/` without a project-hash subdirectory. vex auto-writes a `.gitignore` so the cache is not committed. Useful when the cache should travel with the project (renames, copies, moves)
- **`-j/--jobs N` flag** on `index`, `update`, `watch` to cap the worker pool. Mirrored by `$VEX_JOBS` and `jobs = N` in `.vex.toml`
- **80% default thread count** (rounded up, floor 1) when no explicit jobs setting is supplied. Leaves headroom for the editor / browser / language server sharing the machine. Pass `0` to keep using every core
- **Windows cache locations**: `%LOCALAPPDATA%\vex` (with `%USERPROFILE%\AppData\Local\vex` and `$HOME\AppData\Local\vex` as fallbacks). No more `/tmp` literal anywhere in the resolver

### Fixed
- **MCP server now forwards `--auto-update`** to the seven index-backed commands (`search`, `find_symbol`, `find_similar`, `show`, `usages`, `check`, `similar`, `duplicates`). Previously a stale index surfaced as a tool failure to the MCP client even though the bare CLI handled the same condition transparently
- **MCP cache path mangling on Windows** — `HOME` fell back to `/tmp` and downstream paths became `/tmp\.cache\vex\<hash>\index.vex`. Fixed by the new platform-aware resolver

### Changed
- `.vex.toml` now accepts `cache_dir`, `local_cache`, and `jobs` fields (all optional). Existing configs continue to parse unchanged
- Default worker count for parallel indexing dropped from "all cores" to "ceil(80%)". To restore the previous behaviour, set `jobs = 0` in `.vex.toml`, `$VEX_JOBS=0`, or pass `-j 0`. The change only affects indexing commands; one-shot search/show calls do not eagerly initialize the rayon pool

### Security
- **Path-traversal blocker** for `cache_dir`. A `.vex.toml` shared via a monorepo cannot redirect index writes outside the project root via `..` segments, including post-tilde-expansion cases like `~/../etc/evil`. Rejected paths produce a warning and fall back to the platform default
- **Atomic `.gitignore` creation** for `local_cache` uses `OpenOptions::create_new` so a planted symlink at `.vex_cache/.gitignore` cannot be overwritten

### Internal
- `VexConfig` records the directory of the `.vex.toml` that produced it (`source_dir`) so relative `cache_dir` values resolve against the config file, not the cwd
- Cache override is installed once at `dispatch()` via `OnceLock<CacheLayout>`, avoiding a per-call-site parameter through 20+ index/manifest/hnsw path accessors
- New helpers in `util::config`: `resolve_cache_root` (`ResolvedCache { root, skip_hash_subdir }`), `resolve_jobs`, `resolve_explicit_jobs`, `default_thread_count`, `expand_user`, `write_local_cache_gitignore`, `set_cache_override`, `init_rayon_pool`
- Test env helper uses an RAII `Drop` guard so a panicking test cannot leak mutated env into the next; poisoned-mutex recovery via `unwrap_or_else(|e| e.into_inner())`
- 17 new unit tests covering the cache-resolution priority chain, tilde expansion, path-traversal rejection (relative and tilde-bypass), local_cache layout, VEX_JOBS opt-ins, and the 80%-default formula

## [1.5.0] - 2026-05-16

Phase 9 ships hybrid search v2: three orthogonal channels (structural FST,
BM25 over body tokens, semantic HNSW) fused via 3-way RRF; persistent call
graph delivering ~1000× speedups on `callers`/`callees`; a pluggable embedder
trait that unblocks future code-specific models; and two new commands
(`similar`, `duplicates`) on top of the existing vector index. The on-disk
format bumps to v4 with backwards-compatible read of v3.

- 19 commands (+2), 17 MCP tools (+2), 649 tests (+108 vs v1.4.3)

### Added — Phase 9.4 (BM25 channel in hybrid search)
- **BM25 inverted index** as the third channel in `vex search` alongside structural FST and semantic HNSW. Closes the gap between "exact symbol name" (structural) and "general meaning" (semantic) — finds rare body terms like `timeout`, `singlestore`, `idempotency_key` that aren't part of the symbol name
- **`vex::store::bm25` module** — `Bm25IndexBuilder` (per-symbol term bags), `Bm25Reader` (zero-copy scoring), `tokenize_query`/`tokenize_document`, Okapi BM25 with `K1 = 1.2`, `B = 0.75`. Minimal English stop-word filter applied at query time only (keeps the index size unchanged when the stop-word list evolves)
- **`vex::search::bm25::search(reader, query, top_k)`** — adapter that converts BM25 hits to `SearchResult` tagged with new `MatchType::Bm25`
- **3-way RRF fusion** — `vex::search::fusion::{fuse3, fuse_many}`. `Hybrid` label when a result appears in ≥2 channels; original `MatchType` preserved when in just one
- **CLI integration** — BM25 auto-on when the index has BM25 data. Flag `--no-bm25` to disable on a per-call basis
- **Pipeline integration** — `pipeline::build_bm25_index` aggregates per-symbol terms from `name + signature + body_tokens + doc` and emits the FST/posting/stats triple
- **20 new integration tests** in `tests/bm25_test.rs` plus 7 BM25 unit tests in `src/store/bm25.rs` — format size, writer/reader roundtrip (with and without BM25), pipeline emission, IDF discrimination (rare term ranks above common), short-doc preference under same TF, 3-way fusion with Hybrid labelling, MatchType tagging, Unicode tokenization, empty/stopword-only query handling

### Changed — Phase 9.4
- `CallGraphHeader` extended from 80 → 128 bytes (10 call-graph fields + 6 BM25 fields: fst/postings/stats offset+len). Internal name kept for back-compat with 9.3 callers; will be renamed to a generic `V4SectionHeader` in a follow-up. **No version bump v4 → v5** — v4 was unreleased outside this repo, and extending the existing v4 envelope avoids a second migration event
- `writer::write_index_with_call_graph` signature gains `Option<Bm25Sections>` parameter (a `(&[u8], &[u8], &[u8])` triple of pre-built BM25 bytes)
- `search::fusion::fuse` retained as a back-compat 2-way wrapper around `fuse_many`; new code uses `fuse3` / `fuse_many`
- `MatchType` gains the `Bm25` variant

### Note — format compatibility
- v3 indexes still readable; `has_bm25() == false` → search falls back to structural-only behavior (plus semantic if `--semantic` and vectors present)
- v4 indexes built between Phase 9.3 and 9.4 are also readable; `bm25_*_len == 0` → BM25 channel disabled until next `vex index` rebuilds

### Added — Phase 9.3 (persistent call graph + format v4)
- **Format bump v3 → v4**: new sections persisted at index time — `CallEdge` records, callers FST, callees FST + posting lists. The `Header` struct stays byte-identical to v3; a new `CallGraphHeader` (80 B) is placed at offset `Header::SIZE` when `version >= 4`. v3 indexes still open (back-compat read path)
- **`vex callers <name>` / `vex callees <name>` fast path** — FST lookup (~4ms) on v4 indexes. Falls back to live tree-sitter scan when (a) no index exists, (b) index is v3, (c) v4 index has no call graph, or (d) index exists but fails to open (with a warning logged so corrupt/locked indexes don't silently degrade to the slow path)
- **`crate::callgraph::extract_call_edges(content, lang)`** — returns `(caller_fn_name, caller_fn_line, callee_name, call_line)` quadruples. Used by `pipeline::parse_file` to populate `ParsedFile.call_edges`
- **`store::call_graph` module** — `CallEdgeBuilder`, `build_callers_fst`, `build_callees_fst`, `CallGraphFstReader`, `find_callers_fast`, `find_callees_fast`, `encode_caller_key`
- **Incremental update preserves call edges** — `reconstruct_unchanged` re-emits edges from the old index for files whose hash didn't change; only changed/deleted files get fresh extraction
- **25 new integration tests** in `tests/call_graph_test.rs` — format sizes (Header::SIZE = 144, CallGraphHeader::SIZE = 80, CallEdge::SIZE = 16), writer/reader roundtrip, callers/callees FST fast paths with case-insensitivity and dedup, same-name-across-files isolation via `caller_sym_idx` keying, end-to-end pipeline → fast path, `extract_call_edges` for Rust + unsupported langs, incremental update preserves edges, regression test for same-name-within-file disambiguation (CRITICAL fix from Phase 9.3 review)

### Changed — Phase 9.3
- `ParsedFile` gains `call_edges: Vec<RawCallEdge>` — populated at parse time, consumed by writer
- `pipeline::resolve_call_edges` keys on `(path, name, line)` (not just `(path, name)`) so two functions sharing a name in the same file (overloaded methods, duplicate `impl` blocks) get distinct caller symbol indices
- `IndexReader::open` accepts versions in `[MIN_SUPPORTED_VERSION..=VERSION]` (v3..v4) plus legacy v2 — section-bounds validation extended for the new v4 sections
- Writer aligns `call_edges_offset` to a 4-byte boundary so `CallEdge` records can be safely mmap-cast on strict-alignment platforms
- `vex::store::writer::write_index_full` now delegates to a new `write_index_with_call_graph` (used by `pipeline`)

### Fixed — Phase 9.3 (review findings)
- **CRITICAL**: same-name symbols within a single file used to all resolve to the first occurrence's `caller_sym_idx`, attributing every later duplicate's call sites to the wrong caller. Fixed by including the symbol's definition line in the resolution key. Regression test in `tests/call_graph_test.rs::duplicate_function_name_in_same_file_resolves_to_correct_caller`
- **HIGH**: `find_callers_fast` / `find_callees_fast` now early-return on `!has_call_graph()` instead of relying on callers to check the invariant
- **HIGH**: `cmd_callgraph` logs a `tracing::warn!` when an existing index fails to open (corrupt/locked) instead of silently falling back to the slow path
- Writer alignment bug surfaced during test authoring — `call_edges_offset` rounded up to a 4-byte boundary so subsequent mmap reads succeed alignment checks

### Note — format compatibility
- v3 indexes remain readable. `has_call_graph()` returns false → CLI falls back to live scan. Run `vex index` to rebuild as v4 and unlock fast paths.

### Added — Phase 9.1 (pluggable embedder backend)
- **`Embedder` trait** — `id()`, `dim()`, `char_budget()`, `embed()`, `embed_batch()`. Unblocks future code-specific embedders (BGE, GraphCodeBERT, CodeT5+)
- **Registry**: `embed::make_embedder(id)`, `embed::embedder_dim(id)`, `embed::known_embedders()`. Unknown ID returns an error listing known IDs
- **`MiniLMEmbedder`** — current default and only implementation. Output 384-dim, char budget 1100, ID `"minilm-l6-v2"`. Replaces the previous concrete `Embedder` struct
- **`embedder` option** in `.vex.toml` and `--embedder <id>` flag for `index`/`update`/`watch`. Priority: CLI > config > `DEFAULT_EMBEDDER` (=`minilm-l6-v2`)
- **`Manifest.embedder_id: Option<String>`** — records the embedder a semantic index was built with. Missing field on pre-9.1 manifests is interpreted as the default for back-compat
- **Embedder mismatch detection** at `vex search --semantic`: when the requested embedder differs from `manifest.embedder_id`, search bails with a rebuild hint instead of producing nonsense results
- **`writer::write_index_full`** now takes `vector_dim: u32` instead of hardcoding 384. `Header.vector_dim` is filled from the embedder, opening the door to non-384 models
- **21 new integration tests** in `tests/embedder_test.rs` covering trait registry, dim lookup, resolve priority, mismatch detection (including back-compat with missing `embedder_id`), manifest roundtrip + skip-serializing-when-none, config parsing, `build_context` budget, writer variable dim + length validation

### Changed — Phase 9.1
- `embed::build_context` signature: takes `budget: usize` instead of hardcoded `EMBEDDING_CHAR_BUDGET`. Callers pass `embedder.char_budget()` to fit the model's token window
- `semantic::search_with_embedder` now accepts `&mut dyn Embedder` (was `&mut Embedder` concrete struct)
- `pipeline::run` and `pipeline::update` now take `embedder_id: &str`
- `watch::watch` takes `embedder_id: &str`
- `src/embed/model.rs` slimmed to `build_context` only; the model struct lives in the new `src/embed/minilm.rs`

### Note — format compatibility
- **No binary format bump in 9.1.** Embedder identification lives in `manifest.json`, not the index Header. Format v4 will land with Phase 9.3 (persistent call graph) when binary section additions are actually needed. This avoids breaking v3 read-back.

### Added — Phase 9.2 (semantic similarity)
- **`vex similar <symbol>`** — find symbols semantically close to a given one. Resolves the symbol's stored embedding, runs HNSW (or brute-force fallback), returns nearest neighbors with cosine similarity. Flags: `--limit`, `--threshold`, `--filter`, `--auto-update`. Requires `vex index --semantic`
- **`vex duplicates`** — list pairs of near-duplicate symbols by embedding similarity. Canonical pair dedup `(min_idx, max_idx)` — never both `(A, B)` and `(B, A)`. Flags: `--threshold` (default 0.9), `--limit` (default 50), `--min-body-lines` (default 5, filters trivial 1-liner matches via approximated body length), `--filter`
- **MCP tools**: `similar` (by existing symbol — distinct from existing `find_similar` which queries by description) and `duplicates`
- **18 new integration tests** in `tests/similar_test.rs` covering self-exclusion, threshold filtering, canonical pair dedup, body-length filtering, empty index, missing vectors, unknown symbol errors, descending sort

### Changed
- `cosine_similarity` promoted from `fn` to `pub(crate) fn` in `src/search/semantic.rs` so `src/search/similar.rs` shares the same implementation

## [1.4.3] - 2026-05-14

### Added
- **+7 languages** (12 → 19): PHP, Bash, Lua, CSS, HTML, YAML, TOML. Each ships a tree-sitter grammar crate, an `.scm` query, and a per-language regression test (`tests/<lang>_query_test.rs`) on the v1.4.2 pattern
  - PHP: class, interface, trait, enum, function, method, class constant, `use` import
  - Bash: function definitions, `source`/`.` imports
  - Lua: function (top-level, local, `Mod.fn`, `Class:method`), `require` imports
  - CSS: class selectors, `#id` selectors, `@keyframes`, custom properties (`--var`)
  - HTML: `id="..."` attribute values, hyphenated custom-element tags
  - YAML: document-root mapping keys (anchored to avoid nested-key noise)
  - TOML: table headers (`[name]`, `[[name]]`), key/value pairs
- **`vex implementations` for PHP and Ruby** — 7 → 10 OOP languages supported
  - PHP: `extends`, `implements` (multi-arg, qualified names), `interface extends`, **PHP 8.1+ `enum implements`**, **trait `use TraitName`** (in class & trait bodies, labelled `(uses)`)
  - Ruby: `class Foo < Bar` (labelled `(inherits)`), `include`/`extend`/`prepend Mixin` inside class and module bodies (labelled `(include)`)
  - Relation labels dispatched via `tree_sitter::QueryMatch::pattern_index` with named thresholds (`PHP_TRAIT_PATTERN_START`, `RUBY_MIXIN_PATTERN_START`)
- **Multi-case identifier scanner** in the refs FST — `extract_references` now picks up `snake_case`, `camelCase`, and `SCREAMING_SNAKE_CASE` identifiers in addition to `PascalCase`. `vex usages process_order` now works in Python/Rust/Go/Lua codebases. Filter keeps plain lowercase words (`total`, `amount`) out of the FST to prevent prose-noise bloat
- 41 new tests (451 → 541): per-language grammar regressions for the new 7, hierarchy tests for PHP/Ruby/traits, scanner case-style coverage, symmetric threshold guards, negative tests against silent query degradation

### Fixed
- **Ruby `#match?` predicate misplaced** — the include/extend/prepend filter sat OUTSIDE the enclosing `(class ...)` S-expression, which tree-sitter silently treated as a no-op. The query degraded to "any method call inside a class body with a Constant argument", which would falsely match `assert_equal Mixin, foo.class` and `describe Mixin do`. Caught during code review; `ruby_non_mixin_call_does_not_match` now guards it
- **PHP `use Foo as Log;` alias was duplicated** in the imports index because two `namespace_use_clause` patterns matched the same `name` node — pattern 2 (bare `(name)`) fired alongside pattern 3 (`alias:` field). Added `!alias` negative-field guard to pattern 2
- **Lua `require("util")` stored quotes** — `string_content` is absent for strings without escape sequences in tree-sitter-lua 0.5, so the query captured the whole `(string)` node verbatim. New `strip_import_quotes` in `extract_symbols_and_imports` peels off `"..."`, `'...'`, `<...>` (C/C++ system includes), and `[[...]]` (Lua long brackets) from any `import.name` capture. `vex usages util` now resolves the `require` site
- **Markdown heading kind glyph in compact output** was `?` because `cli/output.rs::compact_kind` had no arm for `"heading"`; now emits `H`
- **`clippy::collapsible_match`** failure on `relation_label` PHP arm — refactored to match guards

### Changed
- **Documentation** — `docs/SUPPORTED_LANGUAGES.md` adds an `implementations` support column and lists `src/hierarchy/mod.rs` and `src/cli/mod.rs::Pattern` as match sites to keep in sync; README badges updated to 19 languages / 17 commands / 541 tests
- `extract_references` filter — only structurally-shaped identifiers (`contains '_'` or mixed-case) reach the FST; plain lowercase words like `total`/`amount` are skipped to keep the refs FST tight

## [1.4.2] - 2026-05-13

### Fixed
- **C# parsing was silently broken** in 1.4.1 — `tree-sitter-c-sharp` 0.23.5 shipped grammar ABI 15 while `tree-sitter` 0.24 only supported up to ABI 14. The `LazyLock` initializer panicked on the first `.cs` file, every subsequent file hit a poisoned cell, and the failure was buried in `tracing::warn!` that `RUST_LOG` filters out by default. Result: 0 C# symbols extracted, no user-visible error. Reported externally — 487 `.cs` files indexed as empty
- **Swift parsing was silently broken** since the language was added — `queries/swift.scm` referenced `enum_declaration`, a node that does not exist in `tree-sitter-swift` 0.7. Same `LazyLock`-poisoned-cell failure mode as C#. Discovered only after adding the new per-language regression tests
- **Grammar load failures no longer poison the parser** — `queries::get_query` returned `LazyLock<Query>` and panicked on init; replaced with `LazyLock<Result<Query, String>>` and new `try_get_query` so a compile failure is cached as an error and surfaces as `GrammarLoadError` to the indexing pipeline, not as a thread panic
- **Grammar failures are now visible** — `index/pipeline.rs` aggregates per-language failures and emits a single structured `tracing::warn!` at the end of a run (`language=<X> skipped=<N> error=<reason>`) so users see why their language has zero symbols
- **TOCTOU on file size check** — `discover_files` did `fs::metadata().len() <= 1MB` then `fs::read_to_string()`; a file that grew between the two could exhaust memory. New `read_capped` uses `File::open` + `Read::take(1MB+1)` so the cap is enforced on the actual read
- **Mutex poison handling** in `parse_files` — both lock sites now use `.unwrap_or_else(|e| e.into_inner())` instead of `.unwrap()`, so an unrelated worker panic cannot block aggregation
- Stale CLI message in `vex show` referenced "Kotlin, TypeScript pending" — both have had queries for releases; replaced with a generic grammar-load error
- `WalkBuilder::follow_links(false)` is now set explicitly (was relying on the crate default) — defensive: a malicious symlink cannot smuggle in `../../.ssh/id_rsa`

### Added
- **`docs/SUPPORTED_LANGUAGES.md`** — version matrix listing all 12 grammars, their crate versions, extracted symbol kinds, plus runbooks for upgrading a grammar and adding a new language
- **Per-language regression tests** (11 new files, `tests/<lang>_query_test.rs`) — each verifies the grammar loads on empty input and that core symbol kinds are extracted with the right `SymbolKind`. The first regression check fires *before* any real source file is indexed. Total suite: 451 tests (up from 243)
- `GrammarLoadError` typed error in `parse::extractor` — `pipeline.rs` downcasts to it via `anyhow::Error::downcast_ref`, replacing the previous string-matching heuristic
- Negative-assertion tests for Swift enum/struct/class (regression guard against tree-sitter's no-short-circuit query semantics — multiple patterns matching the same node would double-index)

### Changed
- **Grammar version bumps** (all to crates.io latest as of 2026-05-13):
  - `tree-sitter`: 0.24 → 0.26 (adds ABI 15 support)
  - `tree-sitter-rust`: 0.23 → 0.24
  - `tree-sitter-python`: 0.23 → 0.25
  - `tree-sitter-go`: 0.23 → 0.25
  - `tree-sitter-md`: 0.3 → 0.5
- README: updated test count (243 → 451), added per-language grammar regression section, linked `docs/SUPPORTED_LANGUAGES.md`, refreshed Swift row in the language table to reflect actual extracted kinds

### Removed
- `queries::get_query` — dead-weight back-compat wrapper after CLI gate migrated to `try_get_query`. Removing it prevents future callers from accidentally silently swallowing grammar-load errors

## [1.4.1] - 2026-05-11

### Added
- **Staleness detection** — `vex search` warns when the index is stale (git HEAD changed or files modified since last index); uses a single `git rev-parse` call (~0.1ms) with mtime fallback for non-git repos
- `--auto-update` flag for search/show/usages/check — runs `vex update` inline before search when stale
- `--no-stale-check` flag — skip staleness check entirely
- `auto_update` option in `.vex.toml` — enable auto-update by default
- **Context-aware reranking** — `--kind fn` boosts results matching a specific symbol kind, `--context-path src/file.rs` boosts results near the file you are editing (path overlap + module proximity)
- **90 new tests** (172 → 243):
  - Property-based (proptest): rerank preserves length, sorted output, no NaN/negative, fusion commutativity
  - Reranking stress: NaN/Infinity/zero scores, 10K results, edge context paths
  - Adversarial binary format: 20 crafted index tests — overflow offsets, alignment attacks, truncated records
  - Unicode/encoding: BOM, mixed CRLF, unicode identifiers, null bytes, empty files
  - Incremental update: file rename, symbol move between files, empty file, new file added
  - Concurrency: parallel index (lock serialization), concurrent readers, read during reindex
  - Cross-language: same name across 3 languages, wrong extension, 1K-symbol file, deep nesting, error recovery
  - Path edges: spaces, 20-level nesting, symlinks, absolute vs relative, Windows backslashes
- `proptest` added to dev-dependencies

### Fixed
- **NaN propagation in reranker** — `NaN * boost = NaN` silently corrupted scores; now sanitized to 0.0
- **Infinity overflow in reranker** — `f64::MAX * boost = +inf`; now clamped to `f64::MAX / 2.0`
- **Single-result NaN bypass** — `rerank()` early-returned for len ≤ 1, skipping sanitization
- Kind hint mismatch was too aggressive (0.7x demotion) — changed to neutral 1.0 (boost-only, no penalty)
- Path overlap double-counted filename as shared component — now compares directory components only
- Module proximity boost no longer overlaps with path overlap for same-directory results
- `read_git_head()` moved outside advisory lock in `write_output` to reduce lock hold time

### Changed
- `Freshness` enum is `#[must_use]`; `changed_count` is `Option<usize>` (None = count not computed)
- `RerankContext` struct with `kind_hint: Option<SymbolKind>` and `context_path: Option<&str>`
- `rerank()` signature extended: `rerank(query, &RerankContext, results)`
- Manifest extended with `git_head` and `indexed_at` fields (backward-compatible via `serde(default)`)
- README: updated commands table, test count (243), added Staleness Detection section, updated CLAUDE.md integration rules

## [1.4.0] - 2026-05-08

### Added
- C/C++ support (12th language) — classes, structs, functions, methods, templates, enums, `#include` refs
- Heuristic search result reranking — PascalCase → type boost, snake_case → function boost, exact name match boost, test path demotion
- Fuzz testing suite — 3 targets (index reader, refs FST, symbol FST) covering all `unsafe` code paths in the binary format reader
- 21 new integration tests — binary format roundtrip, corrupted index handling, vector roundtrip, fuzzy search, refs FST, incremental update, multi-language parsing, RRF fusion
- CI test gate on release — `cargo fmt` + `cargo clippy` + `cargo test` must pass before build
- Live CI badge in README (GitHub Actions)

### Fixed
- **3 bugs found by fuzzing**: out-of-bounds read on crafted `symbol_count`, misaligned pointer dereference on odd `symbols_offset`, unchecked section offsets exceeding file size
- `IndexReader::open()` now validates all section offsets against actual file size — rejects corrupted/truncated index files instead of crashing
- `symbol()` and `vector()` use runtime alignment checks (not just `debug_assert`) — returns `None` on misaligned data instead of UB
- Overflow-safe arithmetic in `symbol()`, `vector()`, `read_string()`
- `file_paths()` caps allocation to what fits in mmap — prevents OOM on crafted headers
- Unicode-safe string truncation in body token extraction and doc comments
- Real incremental update — `vex update` now only re-parses changed files and reconstructs unchanged symbols from the existing index (previously did a full rebuild despite detecting changes)

### Changed
- `SymbolKind`: added `TryFrom<u8>` — replaces hardcoded `symbol_kind_str()` magic numbers in search modules
- Eliminated code duplication: merged `indices_to_results` / `indices_to_results_typed`, extracted shared `discover_source_files()` from callgraph and hierarchy modules
- README: removed fake test count badge, added honest comparison notes (pre-built index vs full scan tradeoff), added Testing section with fuzz test documentation
- Release workflow now requires tests to pass before building binaries

## [1.3.0] - 2026-05-08

### Added
- Body-aware semantic search — embeddings now include identifiers and string literals extracted from symbol bodies, not just names/signatures/docstrings
- `.vex.toml` config file — exclude patterns, default format, semantic on/off
- `vex init` — generates default config with commented examples
- `--no-semantic` flag to override config-enabled semantic mode
- Fuzzy search — automatic Levenshtein fallback when exact + prefix returns nothing ("IndxReader" finds "IndexReader")
- Markdown support (11th language) — headings indexed as symbols, `vex outline README.md` shows document structure, `vex show "Installation"` extracts full section

### Changed
- `SymbolKind` now uses `#[repr(u8)]` with explicit discriminants for binary format stability
- File walking centralized via `util::walk` module with configurable exclude patterns
- Config loaded from project root (resolved `--path`), not cwd

## [1.1.0] - 2026-05-08

### Added
- Shell completions for bash, zsh, fish (`vex completions <shell>`)
- Release script for version bumps (`scripts/release.sh`)

### Fixed
- Atomic index writes — write to temp file, rename on success (no corruption on crash)
- Advisory file locking during `vex index` / `vex update` (prevents concurrent writes)
- Skip binary and minified files by content heuristic (control chars, long lines)
- Catch tree-sitter panics in rayon workers instead of crashing the indexer
- Inline index validation with specific error messages (size, magic, version)
- Bounds-check `read_string` to prevent panic on corrupt index

### Changed
- README updated: 15 commands, 115 tests, full MCP tools list, completions docs

## [1.0.1] - 2026-05-07

### Fixed
- Drop x86_64-apple-darwin from release CI matrix

## [1.0.0] - 2026-05-07

### Added
- `vex check` — fast symbol existence check via FST
- `vex callers` / `vex callees` — callgraph navigation (5 languages)
- `vex implementations` — find types extending a base class/trait/interface (7 languages)
- `vex show A B C` — bulk symbol body extraction
- `vex outline --kind fn` — filter outline by symbol kind
- `vex search --filter`, `vex show --filter` — path-based result filtering
- Semantic search quality: tokenized names, path keywords, docstrings in embeddings
- MCP server expanded to 15 tools (full CLI parity)
- CI/CD: GitHub Actions for tests and releases
- CLAUDE.md integration instructions in README

### Fixed
- Pattern matching enabled for Kotlin, TypeScript, and SQL

## [0.2.0] - 2026-05-07

### Added
- `vex grep` — regex content search with `--filter` path support
- Homebrew installation (`brew install tenatarika/tap/vex`)

## [0.1.0] - 2026-05-07

Initial release.

### Added
- Structural search — FST-based symbol lookup in ~4ms
- Semantic search — MiniLM-L6-v2 embeddings + HNSW index + RRF fusion
- `vex index` / `vex update` / `vex watch` — full, incremental, and watch-mode indexing
- `vex search`, `vex usages`, `vex show`, `vex outline`, `vex status`
- `vex pattern` — AST pattern matching with metavariables ($NAME, $$$)
- 10 languages: Rust, Python, Go, Java, C#, Ruby, Swift, Kotlin, TypeScript, SQL
- MCP server (`vex-mcp`) for AI agent integration
- Custom mmap'd binary format — zero-copy reads, 10x smaller than SQLite
- Parallel parsing via rayon, incremental updates via content hashing
- Compact output format (`--format compact`) for LLM token efficiency
- JSON output (`--format json`) for tool integration

[Unreleased]: https://github.com/tenatarika/vex/compare/v1.5.0...HEAD
[1.5.0]: https://github.com/tenatarika/vex/compare/v1.4.3...v1.5.0
[1.4.3]: https://github.com/tenatarika/vex/compare/v1.4.2...v1.4.3
[1.4.2]: https://github.com/tenatarika/vex/compare/v1.4.1...v1.4.2
[1.4.1]: https://github.com/tenatarika/vex/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/tenatarika/vex/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/tenatarika/vex/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/tenatarika/vex/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/tenatarika/vex/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/tenatarika/vex/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/tenatarika/vex/compare/v0.2.0...v1.0.0
[0.2.0]: https://github.com/tenatarika/vex/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tenatarika/vex/releases/tag/v0.1.0
