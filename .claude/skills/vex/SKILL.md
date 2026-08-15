---
name: vex
description: Reference for the `vex` code-search CLI — symbol search, usages (text + scope-bound `--strict`), structural AST patterns, call graph, semantic search, and content grep. Activates when picking the right vex subcommand, recalling filter flags, or checking the cross-file binder coverage matrix. Use whenever you'd otherwise reach for Read / Grep / Glob to locate code.
---

# vex — Code Search Reference

`vex` is a fast hybrid structural + semantic code search CLI. Source is parsed via tree-sitter, symbols stored in an mmap'd binary format. Supports exact name lookup (FST + bloom), structural AST patterns, persistent call graph, semantic similarity (HNSW + ONNX), and regex content search.

**Always prefer `vex` over Read / Grep / Glob for code lookup** — faster (~4ms typical), scope-aware, and avoids dragging whole files into context.

## Core Commands

### Exact symbol lookup (you know the name)

- `vex check "SymbolName"` — **does it exist?** Fast yes/no + locations, no ranker noise. Always reach here FIRST when you have a literal name to find. `vex check "A" "B" "C"` for batch.
- `vex show "SymbolName"` — extract the symbol body (**use INSTEAD of `Read` for specific symbols**). `vex show "A" "B" "C"` for multiple in one call.
- `vex usages "SymbolName" --strict` — **type-aware refs from the scope binder**; drops string-literal / comment / wrong-scope noise. Cross-file imports resolved for Rust, TypeScript, Python, C#, C++ (others fall back to text-scan).
- `vex usages "SymbolName"` — same lookup without the binder; text-scan baseline.

### Fuzzy / keyword exploration (you don't know the exact name)

- `vex search "timeout retry"` — multi-word / keyword search via FST + BM25 + semantic RRF fusion. Use for "find me something about X". **Returns ranked NEIGHBORS** when no symbol literally matches — that's the design, not a bug. For "I know the name, just find it" use `vex check` instead.
- `vex search "handle alert" --semantic` — search by meaning (requires `--semantic` index).
- `vex grep "pattern"` — regex content search; use when you need TEXT (string literals, comments, config values), not symbols.
- `vex outline path/to/file.py` — file structure overview.

## Structural Patterns (AST)

- `vex pattern 'class $NAME(BaseModel):' --lang python` — AST pattern matching
- Metavariables:
  - `$X` — single token
  - `$$ARGS` — argument list (multi-line)
  - `$$$BODY` — block (multi-line)
  - Same name = back-reference (must agree across captures)
- Composition: ` && ` (intersect — captures must agree) and ` || ` (union). `&&` binds tighter than `||`. Both must be space-flanked at depth 0.
- `vex pattern '... --why'` — JSON trace on stderr explaining indexed vs live-scan fallback.

## Call Graph

- `vex implementations "BaseClass"` — types extending a class/interface (includes generic-parameterised bases)
- `vex callers "function_name"` / `vex callees "function_name"` — direct edges from the persistent call graph (~4ms)
- `vex paths A B` — all caller chains from A to B (multi-hop, max 6 hops)
- `vex reachable Target` — every symbol that transitively calls Target
- `vex tests-for Target` — test functions that transitively cover `Target` (post-filters `reachable` by test-path globs + name heuristic; rows carry a `framework` label so an agent can pick the right runner). `--include-fixtures` to also surface test-path helpers. `--test-pattern '<glob>'` (repeatable) REPLACES the default pattern set.

## Diff & Similarity

- `vex diff [--base <rev>]` — symbol-level diff (added / removed / moved / body-changed) over files touched on the branch
- `vex similar "SymbolName"` / `vex duplicates` — semantic similarity (requires `--semantic` index)
- `--explain` — adds identifier-overlap reasoning + unified diff to `similar` / `duplicates`

## Filters

Apply to most search-shaped commands:

- `--include '<glob>'` / `--exclude '<glob>'` — repeatable path globs (case-sensitive). Replaces older single-path `--filter`.
- `--kind fn,struct` — boost or restrict to result kinds (multi-value; aliases `def` / `comment` / `test`)
- `--visibility public|private|crate` — symbol metadata post-filter
- `--async-only` / `--no-async` / `--static-only` / `--sealed-only` — language-agnostic metadata gates
- `--threshold 0.8` (a.k.a. `--min-score`) — score cutoff for `similar` / `duplicates`
- `--exclude-generated` — drop machine-generated files (protoc / sqlc / bindgen / Diesel /
  OpenAPI Generator banners) from `search` results. Header heuristic: under-reports rather
  than hiding hand-written code. Use when vendored `*.pb.go` / `*_pb2.py` bury real hits.
- `--why` — JSON trace on stderr (currently on `search`, `pattern`; via MCP it surfaces as `_meta.why`)
- `--format compact` / `--format json` — token-efficient output for automated workflows

## Cross-Language (polyglot repos, microservices)

**vex has no cross-language symbol edges, by design.** `usages` / `callers` stop at the
language boundary — a TS `fetch` call and the Go handler that serves it share no symbol.

To cross the boundary, search for the **shared string**, then pivot back to structure:

```bash
vex grep 'api/v1/invoices'        # route path — matches the TS call site AND the Go route
vex grep 'invoice.created'        # queue topic
vex grep 'CreateInvoiceRequest'   # proto message → its generated stubs in every language
vex show InvoiceHandler           # then pivot to structure once you know the name
```

Match the **stable prefix**, not the full path: route params differ per framework
(`/invoices/{id}` vs `/invoices/:id` vs an interpolated template literal). `vex grep` is
backed by a trigram skip-index, so a full-repo string scan is cheap.

Add `--exclude-generated` to `search` when vendored stubs bury the hand-written code.

Full walkthrough: `docs/COOKBOOK.md` → Recipe 6.

## Rules of Thumb

- **`vex check <Symbol>` instead of `Grep <Symbol>`** when you know the exact name. Bypasses the ranker; gives an honest hit/miss + path:line. `vex search` may surface neighbors (callers, imports) if the symbol isn't defined locally — fine for fuzzy exploration, wrong for "does it exist".
- **`vex show` instead of `Read`** when you need a specific function or class.
- **`vex grep` instead of `Grep`** when searching string literals, comments, or config values (text, not symbols).
- **`vex usages --strict` for refactor work** — text-scan refs lie about scope; `--strict` reads the persistent reference-edges section built from the scope binder.
- **`vex search "keyword phrase"`** is for FUZZY / multi-word / "find me something about X" — not for exact identifier lookup. v1.15.0 prints a stderr drift hint when an identifier-shaped query returns 0 FST hits, suggesting `check`/`show`/`usages --strict`.
- **Re-index after a format-version bump**: `vex update` is incremental but won't recover from a bump; run `vex index` after upgrading the binary.

## Indexing

```bash
vex index             # structural only (fast)
vex index --semantic  # with embeddings (slower, enables semantic search + similar/duplicates)
vex update            # incremental update — only changed files
```

## Cross-File Binder Coverage (for `usages --strict`)

| Language   | Import resolved cross-file                                              |
|------------|-------------------------------------------------------------------------|
| Rust       | `use foo::Bar;`                                                         |
| TypeScript | `import { Bar } from './foo'` (named / default / namespace / type-only) |
| Python     | `import foo`, `from foo import Bar` (incl. aliases)                     |
| C#         | `using A.B.C;` (simple / `static` / `Alias = ...` / `global`)           |
| C++        | `using std::vector;`, `using V = T;`, `namespace alias = ns;`           |

Wildcard forms fall back to text-scan refs: C++ `#include`, C++ `using namespace`, Python `from x import *`, Rust `use foo::*`.

## Common Pitfalls

- **Meaningful-identifier filter**: pure-lowercase identifiers without `_` (e.g. `compute`, `calc`, `total`) are rejected by the indexer, so `usages --strict` on those bails to text-scan. Use snake_case / mixed-case symbols when designing test fixtures.
- **Cache directory**: keyed by `xxh3` of the canonical project path — mismatched paths land in different cache dirs, so don't `cd` through symlinks if you expect to hit warm cache.
- **Bench / format bump recovery**: `vex update` is incremental and **will not** rebuild a format-incompatible index. After a `vex` binary upgrade that bumps the format version, run `vex index` once to recreate.
