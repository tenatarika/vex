---
name: vex
description: Reference for the `vex` code-search CLI — symbol search, usages (text + scope-bound `--strict`), structural AST patterns, call graph, semantic search, and content grep. Activates when picking the right vex subcommand, recalling filter flags, or checking the cross-file binder coverage matrix. Use whenever you'd otherwise reach for Read / Grep / Glob to locate code.
---

# vex — Code Search Reference

`vex` is a fast hybrid structural + semantic code search CLI. Source is parsed via tree-sitter, symbols stored in an mmap'd binary format. Supports exact name lookup (FST + bloom), structural AST patterns, persistent call graph, semantic similarity (HNSW + ONNX), and regex content search.

**Always prefer `vex` over Read / Grep / Glob for code lookup** — faster (~4ms typical), scope-aware, and avoids dragging whole files into context.

## Core Commands

- `vex search "SymbolName"` — find symbol definitions (FST + BM25 + semantic fusion)
- `vex show "SymbolName"` — extract symbol body (**use INSTEAD of `Read` for specific symbols**)
- `vex show "A" "B" "C"` — extract multiple symbols in one call
- `vex usages "SymbolName"` — find all references (text-scan baseline)
- `vex usages "SymbolName" --strict` — **type-aware refs from the scope binder**; drops string-literal / comment / wrong-scope noise. Cross-file imports resolved for Rust, TypeScript, Python, C#, C++ (others fall back to text-scan).
- `vex grep "pattern"` — regex content search (use when you need text, not symbols)
- `vex search "description" --semantic` — search by meaning (requires `--semantic` index)
- `vex outline path/to/file.py` — file structure overview
- `vex check "A" "B" "C"` — fast symbol existence check

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
- `--why` — JSON trace on stderr (currently on `search`, `pattern`; via MCP it surfaces as `_meta.why`)
- `--format compact` / `--format json` — token-efficient output for automated workflows

## Rules of Thumb

- **`vex show` instead of `Read`** when you need a specific function or class
- **`vex search` instead of `Grep`** when looking for symbol definitions
- **`vex grep` instead of `Grep`** when searching string literals, comments, or config values
- **`vex usages --strict` for refactor work** — text-scan refs lie about scope; `--strict` reads the persistent reference-edges section built from the scope binder
- **Re-index after a format-version bump**: `vex update` is incremental but won't recover from a bump; run `vex index` after upgrading the binary

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
