# Vex

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/tenatarika/vex/actions/workflows/ci.yml/badge.svg)](https://github.com/tenatarika/vex/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)
[![Commands](https://img.shields.io/badge/commands-25-blue.svg)]()
[![Languages](https://img.shields.io/badge/languages-19-blueviolet.svg)]()
[![Tests](https://img.shields.io/badge/tests-1973-green.svg)]()

Fast hybrid structural + semantic code search. **V**ector + ind**ex**.

[Why Vex?](#why-vex) · [How It Compares](#how-it-compares) · [Installation](#installation) · [Quick Start](#quick-start) · [Commands](#commands) · [Configuration](#configuration) · [How Search Works](#how-search-works) · [Benchmarks](#benchmarks) · [Supported Languages](#supported-languages) · [Integration](#integration) · [Testing](#testing) · [Architecture](#architecture)

```
$ vex check "TelemetryProcessor"           # 4ms — does it exist? where? (exact name)
$ vex show "TelemetryProcessor"            # extract the class body (not the whole file)
$ vex usages "Config" --strict             # who references this symbol? (binder-resolved, no noise)
$ vex callers "process_event"              # who calls this function? (~4ms; covers module-scope + Python/Java decorators)
$ vex implementations "BaseService"        # who extends/implements this?
$ vex search "timeout retry"               # fuzzy / multi-word — BM25 finds rare body terms
$ vex search "handle alert" --semantic     # find by meaning, not just name
$ vex pattern 'fn $NAME($$$) -> Result'    # AST pattern matching (like ast-grep)
$ vex similar "PaymentService"             # semantically close symbols
$ vex duplicates --threshold 0.95          # near-duplicate pairs
$ vex bundle --mode symbol --symbol Foo    # body + callers + callees + similar in 1 call
```

**Pick the right tool**: `vex check` for "does `Foo` exist?", `vex search` for "find me something about retries". `search` is a *ranked blend* — it surfaces neighbors (callers / imports) when no symbol literally matches, which is great for exploration and wrong for exact-name lookup. v1.15.0 prints a stderr hint when an identifier-shaped `search` returns 0 FST hits.

## Why Vex?

- **~4ms search** after indexing — FST-based O(query_len) lookup, not O(symbols). Requires a pre-built index (indexing takes 20ms-600ms+ depending on project size)
- **3-channel hybrid search** — structural FST (names) + BM25 (rare body terms) + semantic HNSW (meaning), fused via Reciprocal Rank Fusion. Find symbols when you don't know the exact name AND when generic semantic-only search would be too noisy
- **Persistent call graph** — `vex callers`/`vex callees` reads from an FST built at index time (~4ms), not a live tree-sitter scan (seconds). Module-scope expressions are reported via synthetic `<module:path>` callers (Phase 14.1); Python + Java function/method decorators (Phase 14.2), Kotlin annotations + C# method/constructor attributes (Phase 14.2.2), and TypeScript method decorators + Rust outer attributes (Phase 14.2.1) emit forward edges to their targets. Class-level decorators remain invisible — see [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md)
- **Pluggable embedder** — `Embedder` trait + registry; swap MiniLM-L6-v2 for future code-specific models (BGE, CodeBERT) without touching call sites
- **Token-efficient** — compact output saves typically 6-10x fewer tokens than grep on average lookups (up to 88x on minified JS/CSS); `vex show` extracts just the symbol body instead of the whole file
- **19 languages** indexed via tree-sitter, with three coverage tiers: **type-aware `--strict usages`** on 5 binder languages (Rust / TypeScript / Python / C# / C++); **indexed pattern prefilter** on 12 T1+T2a languages; baseline structural + semantic search on all 19 (see [Supported Languages](#supported-languages) for the matrix)
- **Single binary, zero config** — no LSP servers, no databases, no Docker. Just `vex index && vex check Foo`

## What Vex isn't

vex is a **static-analysis indexing tool**, not a language server. Set expectations honestly:

- **Not an LSP replacement.** No go-to-definition into third-party packages, no rename refactoring, no type-checking, no hover docs. For those, keep your LSP.
- **`vex search` is a ranked blend, not an exact-name lookup.** Structural FST + BM25 + semantic fused via RRF return *relevance-ordered* results — when no symbol literally named `Foo` lives in the index (imported from a dependency, deleted, typo), BM25 may surface callers / imports as if they were the definition. For exact-symbol questions ("does it exist?", "show me the body", "who calls it?") use `vex check Foo` / `vex show Foo` / `vex usages Foo --strict` — they bypass the ranker. **v1.15.0** prints a one-line stderr hint when an identifier-shaped query gets zero FST hits.
- **No dynamic-dispatch visibility.** Decorator routing (`@router.get("/path")`), string-resolved factories (`uvicorn.run("main:app")`), reflection (`getattr(obj, name)()`), and macro-expanded references are all invisible to every vex command. `vex grep '\bname\b'` is the textual escape hatch.
- **`vex callers` has uneven coverage outside function scope.** Module-level expressions like `app = create_app()` are reported via synthetic `<module:path>` callers (Phase 14.1). Python + Java function/method decorators (Phase 14.2), Kotlin annotations + C# method/constructor attributes (Phase 14.2.2), and TypeScript method decorators + Rust outer attributes on fns/methods (Phase 14.2.1) emit forward edges — `vex callers GetMapping` lists every Spring handler, `vex callers HttpGet` every ASP.NET action, `vex callers test` every `#[tokio::test]`. Class-level decorators (14.6) remain on the roadmap.
- **`vex usages` quality varies by language.** 5 binder-supported languages get refactor-grade `--strict` refs; the other 14 use an identifier scanner with a higher false-positive rate.

See [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) for the full coverage matrix, concrete repros, and recommended workarounds per query type. **Read it before evaluating vex on a Python/FastAPI/Django codebase** — the framework patterns are the most-flagged gaps.

## How It Compares

|  | **vex** | **ripgrep** | **ast-index** | **ast-grep** | **Serena** |
|---|---|---|---|---|---|
| **What it searches** | Symbol definitions | All text | Symbol definitions | AST patterns | Symbols (via LSP) |
| **Requires indexing?** | Yes (20ms-600ms+) | No | Yes | No | No |
| **Search speed** | **~4ms** (pre-built FST) | 75-120ms (disk scan) | 22-60ms (SQLite) | ~30ms (scan) | LSP-dependent |
| **Semantic search** | HNSW + embeddings | -- | -- | -- | -- |
| **Pattern matching** | `fn $NAME($$$)` | regex only | -- | `fn $NAME($$$)` | regex only |
| **Index size** | **5 MB** / 20K syms | no index | 190 MB / 20K syms | no index | no index |
| **Token efficiency** | **6-88x** fewer than rg | baseline | ~3x fewer than rg | N/A | N/A |
| **Symbol body extraction** | `vex show` | -- | -- | -- | -- |
| **Languages** | 19 | any | 10+ | 10+ | 40+ (LSP) |
| **Refactoring** | -- | -- | -- | -- | rename, move, inline |
| **Runtime deps** | none | none | none | none | Python + LSP |

**Note**: vex search speed assumes a pre-built index. Ripgrep and ast-grep require no upfront indexing and work immediately on any directory. The tradeoff is amortized: if you search the same codebase many times (typical in agent workflows), the one-time indexing cost pays for itself.

**Best for**: fast symbol search in AI agent workflows where token efficiency matters. Not a replacement for LSP-based tools (no refactoring, no go-to-definition in dependencies).

## Installation

```bash
# Homebrew (macOS/Linux)
brew tap tenatarika/tap
brew install vex

# From source (any platform with a Rust toolchain)
git clone https://github.com/tenatarika/vex.git
cd vex
cargo build --release
cp target/release/vex ~/.local/bin/
```

### Linux

Pre-built `vex` ships in every GitHub Release for `x86_64-unknown-linux-gnu`:

```bash
curl -L https://github.com/tenatarika/vex/releases/latest/download/vex-x86_64-unknown-linux-gnu.tar.gz | tar -xz
mv vex ~/.local/bin/      # or: sudo mv vex /usr/local/bin/
vex --version
```

Built on the current `ubuntu-latest` GitHub runner (glibc-linked). For older glibc distros, musl-based distros (Alpine, NixOS without `nix-ld`), or `aarch64` Linux (Graviton, Pi 5, Ampere) — build from source via `cargo build --release`.

### Windows

Pre-built `vex.exe` ships in every GitHub Release.

1. Download `vex-x86_64-pc-windows-msvc.tar.gz` from the [latest release](https://github.com/tenatarika/vex/releases/latest)
2. Extract `vex.exe` somewhere stable (e.g. `C:\Users\<you>\bin\`) — `tar -xzf vex-x86_64-pc-windows-msvc.tar.gz` from a recent PowerShell, or 7-Zip / WinRAR via right-click.
3. Add that folder to `PATH` (System Properties → Environment Variables → edit `Path` → add the folder)
4. Open a fresh terminal and run `vex --version`

To update, run `vex self-update` — it fetches the latest release, picks the right archive for your platform, and replaces the binary in-place. Same command works on macOS and Linux too.

## Quick Start

```bash
# Index a project (structural only — fast)
vex index --path /path/to/project

# Index with semantic embeddings (slower first time, downloads 86 MB model)
vex index --path /path/to/project --semantic

# Exact-name lookup (does this symbol exist?)
vex check "PaymentService"

# Extract a symbol's body (no whole-file read)
vex show "PaymentService"

# Fuzzy / multi-word search (returns ranked neighbors when no symbol matches)
vex search "payment processing" --semantic

# Find all usages of a symbol (--strict drops string-literal / comment / wrong-scope noise)
vex usages "IndexReader" --strict

# File structure outline
vex outline src/main.rs

# Find implementations of a trait/interface
vex implementations "Iterator"

# Callgraph: who calls / is called by a function (fast path via persistent index)
vex callers "process_event"
vex callees "process_event"

# Multi-hop call graph (v1.7)
vex paths "main" "process_event"          # all caller chains from main → process_event
vex reachable "process_event"             # everything that transitively reaches it

# Symbol-level diff against a branch (v1.7)
vex diff --base main                      # what symbols did this branch change?

# Semantic similarity by existing symbol — explain what's actually similar (v1.7)
vex similar "PaymentService" --limit 5 --min-score 0.7 --explain

# Near-duplicate pairs with reasoning (v1.7)
vex duplicates --min-score 0.95 --min-body-lines 5 --explain

# Search with per-call scope + metadata filters (v1.7)
vex search "Repository" --include 'src/**' --exclude '**/*.gen.*' --visibility public --async-only

# Why did the search return these results? (v1.7)
vex search "Foo" --why 2>trace.json

# Bundle: 4 round-trips → 1 envelope (v1.9, Phase 13.2)
vex bundle --mode symbol --symbol PaymentService          # body + callers + callees + similar
vex bundle --mode pr-impact --base origin/main            # changed symbols + transitive callers + tests
vex bundle --mode project --top-n 30                      # top-N by reverse call-graph indegree

# Diff-context filters on every search-shaped command (v1.9, Phase 13.7-D3)
vex search "Repository" --since-branched                  # only files changed since branching from main
vex usages "Config" --since HEAD~3                        # refs within the last 3 commits
vex callers "Foo" --changed-only                          # working-tree changes only

# Extract just a symbol's body — replaces Read for a specific function/class
vex show "PaymentService"                                 # full body of the class / fn
vex show "Foo" "Bar" "Baz"                                # multiple symbols in one call

# Smart show truncation for token efficiency (v1.9, Phase 13.3)
vex show "BigClass" --signature-only                      # just the signature line
vex show "PaymentService" --head 20                       # first 20 lines of the body
vex show "Foo" --no-body                                  # signature + docstring, no body

# Ranking-eval harness — CI regression guard (v1.9, Phase 13.12)
vex eval --bench benches/ranking_golden/queries.toml      # nDCG@10 / recall@10 / MRR per query
vex eval --min-ndcg 0.85                                  # fail if mean nDCG drops below threshold

# Capability discovery for MCP clients (v1.9, Phase 13.0)
vex capabilities                                          # JSON: protocol_version, signals, bundle_modes, …

# Fast existence check
vex check "Foo" "Bar" "Baz"

# Incremental update (re-parses only changed files, reuses unchanged from index)
vex update

# Watch mode (re-indexes on file changes)
vex watch

# Show index stats
vex status

# Shell completions
vex completions zsh > ~/.zfunc/_vex
```

## Commands

| Command | Description |
|---------|-------------|
| `vex index [--path .] [--semantic] [--embedder ID] [--history [--history-depth N]]` | Build full index. `--semantic` generates embeddings + HNSW + BM25. `--embedder` selects embedding model (default `minilm-l6-v2`). **`--history` (v1.15.0)** builds the Phase 14.8 persistent history-symbol section (`<index_dir>/index.git_history`) so `vex history <Symbol>` runs in FST-lookup time. `--history-depth N` caps the walk at N newest commits (global, not per-file). |
| `vex search <query> [--semantic] [--no-bm25] [--limit N] [--kind def,fn,…] [--visibility V] [--async-only] [--why]` | Hybrid search: structural + BM25 + semantic (when `--semantic`). 3-way RRF fusion. Multi-value `--kind` (canonical names + meta-selectors `def`/`comment`/`test`/`ref`). Metadata post-filters narrow by signature keywords. `--why` appends a JSON trace to stderr. **v1.15.0 search-drift hint**: when the query is identifier-shaped (`compile_query`, `Foo`, `_internal`) and the structural FST finds zero matches, vex prints a one-line stderr hint pointing at `vex check` / `vex show` / `vex usages --strict` — the typical "imported-from-dependency" case where BM25 would otherwise surface callers as if they were the definition. See [`docs/COOKBOOK.md`](docs/COOKBOOK.md) FAQ. |
| `vex show <symbol> [--limit N] [--context N] [--kind fn] [--visibility V] [--async-only] [--signature-only \| --head N \| --no-body]` | Extract symbol body from source (saves tokens vs full file read). Same metadata + kind filters as `search`. **v1.9 (Phase 13.3):** smart truncation flags — `--signature-only` keeps only the declaration line, `--head N` keeps the first N body lines, `--no-body` returns signature + docstring only. Mutually exclusive. |
| `vex similar <name> [--limit N] [--min-score T] [--explain]` | Find symbols semantically close to an existing one (HNSW nearest neighbors). `--explain` adds identifier-Jaccard + truncated unified diff per match. `--min-score` is an alias for `--threshold`. |
| `vex duplicates [--min-score T] [--min-body-lines N] [--explain]` | List near-duplicate symbol pairs by embedding similarity. `--explain` shows what's actually different between the bodies. |
| `vex usages <name> [--limit N]` | Find all references/usages of a symbol (FST lookup). |
| `vex pattern '<pat>' --lang <lang> [--why]` | AST pattern matching with metavariables (`$NAME`, `$_`, `$$$`, plus the v6 named multi-line forms `$$$BODY` / `$$ARGS`). Repeated metavars enforce back-references. Space-flanked ` && ` / ` || ` compose sub-patterns (AND requires both shapes in the file with shared captures agreeing; OR takes the union). When a v6 index is present an indexed prefilter narrows candidates to lang-matching files with the right root kind; falls back to live-scan otherwise. `--why` surfaces a JSON `ScanTrace` (mode / root_kind / candidate vs total / fallback reason) on stderr — and under `_meta.why` in the MCP response. |
| `vex outline <file> [--kind fn]` | Show file structure, optionally filter by symbol kind. |
| `vex implementations <name>` | Find types that extend/implement a base class, trait, or interface (incl. generic-parameterised: `class Foo : Repository<T>`). |
| `vex callers <name>` | Direct callers of a function (fast path via persistent call graph; falls back to live tree-sitter scan when the index is missing). |
| `vex callees <name>` | Direct callees of a function (same fast path). |
| **`vex paths <from> <to> [--max-hops N]`** | **NEW.** Enumerate all caller chains from `from` to `to` over the persistent call graph. Bounded DFS with cycle prevention; default `--max-hops 6`. |
| **`vex reachable <target> [--max-hops N] [--limit N]`** | **NEW.** Transitive set of symbols whose callees reach `target`, with the BFS depth labelled per row. Blast-radius analysis. |
| **`vex diff --base <rev> [--limit N]`** | **NEW.** Symbol-level diff between an arbitrary git revision and the working tree: added / removed / moved-within-file / body-changed entries. `git diff --no-renames` semantics so a `git mv` surfaces both halves. |
| **`vex bundle --mode <symbol\|pr-impact\|project> [...]`** | **NEW (v1.9, Phase 13.2).** Unified multi-source bundle — replaces 4 round-trips (`show → callers → callees → similar`) with one. `--mode symbol --symbol Foo` returns body + callers + callees + semantic similar. `--mode pr-impact --base origin/main` returns changed symbols + transitive callers (depth=2 default) + tests. `--mode project [--top-n 30]` returns top-N by reverse call-graph indegree (experimental — see `docs/MCP-SCHEMA.md#bundle-modes-v19` for the response shape and `mode_hints` per-mode keys). Always emits the v1 envelope `{ protocol_version, capabilities, _meta, results }`. |
| `vex check <name> [name...]` | Fast existence check — which symbols exist in the index? |
| `vex grep <pattern> [--filter path/]` | Regex content search (no index needed). |
| `vex update [--path .] [--semantic] [--embedder ID] [--history \| --no-history]` | Incremental update — re-parse only changed files, reuse unchanged symbols from existing index. **`--history` (v1.15.0)** is sticky via the manifest: if the prior build had a history section, `vex update` keeps it fresh via a 3-branch walker (fast-path skip on no-new-commits, incremental on linear history, full rebuild on force-push). `--no-history` drops the section + nulls the manifest fields. |
| `vex watch [--path .] [--semantic] [--embedder ID]` | Watch filesystem, auto re-index on changes. |
| `vex status [--path .]` | Show index stats: symbol count, size, embeddings, call graph, BM25. |
| `vex completions <shell>` | Generate shell completions (bash, zsh, fish). |
| `vex init` | Create a default `.vex.toml` config file in the project root. |
| **`vex capabilities`** | **NEW (v1.9, Phase 13.0).** Print the machine-readable capability matrix (`protocol_version`, `signals`, `why`, `scope_filters`, `metadata_filters`, `empty_reason`, `bundle_modes`, `auto_update`). MCP / agent clients probe this once at startup instead of re-reading help text. |
| **`vex eval [--bench PATH] [--min-ndcg F] [--json]`** | **NEW (v1.9, Phase 13.12).** Run the ranking-evaluation harness against a hand-curated golden query set; reports nDCG@10 / recall@10 / MRR per query and aggregated. CI regression guard — fails when mean nDCG drops below `--min-ndcg`. Default golden set: `benches/ranking_golden/queries.toml`. |
| **`vex history <Symbol> [--depth N] [--limit N] [--branch REV] [--no-index] [--since YYYY-MM-DD] [--until YYYY-MM-DD] [--author SUBSTR] [--kind KIND] [--diff] [--exact-presence]`** | **NEW (v1.15.0); expanded in v1.16.0 (Phase 14.9).** Every historical version of a symbol reachable from a chosen tip. With `vex index --history` previously run, queries hit a persistent FST sidecar (~10 ms — 1640× faster on tokio-scale repos than the walker). Without the section, shells out to `git log` (~seconds). Indexed mode also finds symbols whose name has been **deleted** from HEAD — the walker can't. **v1.16.0 additions:** date/author/kind filters (lex YYYY-MM-DD compare); `--diff` renders unified diffs between consecutive versions (only signature lines change shape, head of group keeps full sig); `--exact-presence` enumerates the exact commit set where each entry's blob lived (revert-aware, capped by `--exact-presence-max-commits`); prefix-FST fallback on the indexed path for identifier-shaped queries length ≥ 3; JSON envelope ported to standard `ResponseEnvelope` shape (BREAKING for MCP consumers reading `results.items[]`). See `docs/HISTORY-INDEX.md` for the full pipeline + cookbook. |
| `vex self-update [--check] [--yes]` | Update vex to the latest GitHub release. Replaces the running binary in place. Works on Linux, macOS, and Windows. |

### Per-query filters (every search-shaped command)

All search-shaped commands (`search`, `usages`, `pattern`, `show`, `grep`, `implementations`, `callers`, `callees`, `paths`, `reachable`, `similar`, `duplicates`, `diff`, `bundle`) accept:

- **`--include <glob>` / `--exclude <glob>`** (repeatable, gitignore syntax) — per-call path scoping that doesn't require re-indexing. `--exclude` wins over `--include`. Example: `vex search Foo --include 'src/**' --exclude '**/*.gen.*'`.
- **`--filter <substring>`** — older path-substring filter, still supported. Composes AND with the globs.

`vex search` / `vex show` additionally accept:

- **`--visibility <public|private|protected|internal>`** — keep only symbols whose signature carries the explicit keyword. Defaults aren't inferred (bare Rust `fn foo()` does NOT match `--visibility private`).
- **`--async-only`** / **`--no-async`** — keep or exclude async / Kotlin-`suspend` symbols.
- **`--static-only`**, **`--sealed-only`** — restrict to static class members or sealed (or Java-`final`) types.

### Reasoning flags

- **`vex search --why`** prints a JSON trace to stderr (the result list stays on stdout): `normalized_query`, per-channel hit counts (FST / BM25 / semantic), fallbacks engaged (`fuzzy`), and the active filter snapshot.
- **`vex pattern --why`** prints a JSON `ScanTrace` to stderr after the result list: `mode` (`indexed` / `live_scan`), `root_kind_inferred`, `candidate_files` / `total_files`, and `fallback_reason` when the indexed prefilter was skipped (`no-index`, `no-skeleton-section`, `empty-section`, `grammar-drift`, `partial-section`, `index-open-error`). MCP callers see the same JSON under `_meta.why`.
- **`vex similar --explain`** / **`vex duplicates --explain`** add a `jaccard` overlap score plus a truncated unified diff between the two bodies, so you can decide whether two semantically-clustered symbols are actually duplicates before acting.

## Configuration

Create a `.vex.toml` in your project root to customize vex behavior:

```bash
vex init  # generates .vex.toml with commented defaults
```

```toml
# .vex.toml

# Glob patterns to exclude from indexing (gitignore syntax, on top of .gitignore)
exclude = [
    "vendor/**",
    "node_modules/**",
    "*.generated.go",
]

# Output format — "compact" (default since v1.10.1; single-line records),
# "text" (verbose multi-line), or "json" (envelope for MCP / tools).
# format = "text"

# Enable semantic embeddings by default
semantic = true

# Automatically update index before search if stale
# auto_update = false
```

CLI flags always override config values. Use `--no-semantic` to explicitly disable semantic mode when the config enables it.

### Staleness Detection

Vex detects when the index is stale and warns before search:

```
$ vex search "Config"
Warning: index may be stale (HEAD changed). Run `vex update`.
```

**How it works**: on every search, vex compares the git HEAD stored at index time with the current HEAD (~0.1ms, single `git rev-parse`). If HEAD changed → stale. For non-git repos, falls back to mtime comparison — and since v1.11 (H11), when mtime fires, vex streams a `xxh3_64` content hash of the file and compares it to the manifest. If the hash matches, the touch was cosmetic (`git checkout`, `rustfmt` no-op, `rsync --times`) and the file stays `Fresh`; only a real content change re-triggers indexing.

**Auto-update**: skip the warning and update inline:

```bash
# Per-command
vex search "Config" --auto-update

# Always (in .vex.toml)
auto_update = true

# Disable staleness check entirely
vex search "Config" --no-stale-check
```

## Output Formats

```bash
# Compact single-line records — default since v1.10.1 (token-efficient, agent-friendly)
vex search "Foo"

# Verbose multi-line / human-readable
vex search "Foo" --format text

# JSON envelope (for MCP / tool integration; what `vex-mcp` parses)
vex search "Foo" --format json
```

Pin a different default in `.vex.toml` via `format = "text"` if you want the verbose multi-line view at the terminal.

### JSON envelope (v1.11.0 — BREAKING for bare-array parsers)

Every `--format json` subcommand wraps its payload in the Phase 13
envelope. Single shape, easy to detect via `protocol_version`:

```json
{
  "protocol_version": "v1",
  "capabilities": { /* see `vex capabilities` */ },
  "_meta": { "vex.dev/index_age_ms": 1200, "ttlMs": 30000, "cacheScope": "project" },
  "results": [ /* the actual data, shape depends on the subcommand */ ]
}
```

Pre-v1.11 only `search` and `bundle` returned this envelope; the other
~14 subcommands (`show`, `usages`, `pattern`, `grep`, `implementations`,
`callers`, `callees`, `paths`, `reachable`, `check`, `similar`,
`duplicates`, `diff`, `outline`, `index`, `update`, `status`, `eval`)
emitted bare arrays / objects. **Migration**: pre-1.11 `jq '.[0].name'`
or `data[0]['name']` now needs `jq '.results[0].name'` /
`data['results'][0]['name']`. Detect the envelope via
`response.get('protocol_version') == 'v1'` to support both shapes
during a rollout window.

## How Search Works

### Structural Search (default)
Searches by symbol name using an inverted index with CamelCase splitting:
- `"PaymentService"` — exact match
- `"Payment"` — prefix match, finds PaymentService, PaymentGateway
- `"payment"` — case-insensitive, also finds via CamelCase tokens

### Semantic Search (`--semantic`)
Embeds your query with MiniLM-L6-v2 (384-dim vectors) and finds symbols with similar meaning:
- `"parse source code files"` finds `parse_file`, `extract_refs`, `parse_file_symbols`
- `"database storage"` finds `populate_db`, `create_10k_db`, `add_root_persists_to_db`
- `"find implementations of an interface"` finds `find_implementations`, `test_interface_extends`

### BM25 Channel (auto-on when index has BM25 data)
A classic Okapi BM25 (`K1=1.2`, `B=0.75`) over symbol body tokens — identifiers, signatures, docstrings. Closes the gap between "exact name" (structural) and "general meaning" (semantic): finds **rare body terms** like `timeout`, `retry`, `singlestore`, `idempotency_key` that aren't part of any symbol name. Since v1.11 (Phase 8.4) body tokens are also extracted from TOML / YAML / HTML / CSS values, so `vex search "production endpoint" --semantic` can hit a `[server]` table with `endpoint = "https://..."`. Pass `--no-bm25` to disable per-call.

### Hybrid Search (3-way RRF)
When the index has all three channels (built with `--semantic`), `vex search` fuses structural + BM25 + semantic using **Reciprocal Rank Fusion**. Symbols hit by ≥2 channels rank as `Hybrid`; symbols unique to one keep their original match type. Cuts both structural-noise and semantic-blur in the same query.

### Usages (FST)
References stored in an FST (Finite State Transducer) — zero-copy lookup from mmap with prefix search support.

### Type-aware refs (`--strict`)

`vex usages --strict <name>` reads the v5 `reference_edges` section
written by an LSP-style scope binder. For the languages with a
binder (Rust, TypeScript, Python, C#, C++) every ref is resolved at
index time against an in-file scope chain plus an import/use graph,
then serialised against the global symbol the user actually meant —
not just any line that mentions the spelling.

What this changes for the user:

- Identifiers inside comments, doc-strings, string literals, and
  regex bodies are dropped (this filter is on for everyone, not just
  `--strict`).
- A name shadowed by a `let` / `const` / fn param resolves to the
  inner scope, not the outer.
- A `use ext::Foo;` / `import { Foo } from './ext'` / `from ext import
  Foo` makes a ref to `Foo` resolve cross-file to whatever defines it
  in the index. For C++, **quoted `#include "..."`** (v1.14+) walks
  the transitive include graph via BFS to resolve `Foo` against
  symbols defined in any reachable header. System headers
  `<vector>` / `<string>` and macro includes (`#include MY_HEADER`)
  stay unresolved by design.
- A name imported but never defined in the index stays `Unresolved`
  and produces no edge — better than a coincidental match.

Without `--strict` `vex usages` still works for every supported
language via the legacy refs FST; `--strict` simply trades recall
breadth for precision on the five binder languages. v3 / v4 indexes
predating the binder bail with a "re-run `vex index`" message.

### Structural Patterns (`vex pattern`)

Match code by shape rather than text. Live-scan today for every
language vex parses; indexed prefilter (via the v6 `pattern_skeletons`
section) for Rust, TypeScript, and Python.

**Syntax**:

- `$NAME` — capture a single identifier or balanced expression. Same
  name appearing twice enforces a back-reference: `record($X, $X)`
  matches `record(state, state)` and rejects `record(state, other)`.
- `$_` — wildcard (matches without capturing).
- `$$$` — anonymous ellipsis (matches anything up to the next literal;
  spans newlines).
- `$$$BODY` / `$$ARGS` — **named** multi-line ellipsis. Functionally
  identical to `$$$` but captures the consumed text under the given
  name; `$$$BODY` reads naturally for block bodies, `$$ARGS` for
  parameter lists. Back-reference equality also applies.
- ` && ` (space-flanked) — AND composition. Both sub-patterns must
  match in the same file, and shared metavar names must capture the
  same text in both: `struct $S && impl $S` matches files that have
  both shapes for the same `$S`.
- ` || ` (space-flanked) — OR composition (union, deduped by
  `(path, line)`). `&&` binds tighter than `||`.
- Composition operators only fire at bracket / quote depth 0, so
  `record($X, $X)` and `f($X && $Y)` stay single patterns.

**Indexed prefilter**: when a v6 index is present, the leading literal
keyword of the pattern (`fn`, `struct`, `class`, `def`, `impl`, …) is
mapped to a tree-sitter node kind, and `vex pattern` walks only the
files whose persisted skeletons contain that kind. Visibility / async
/ export modifiers in front of the keyword are stripped before the
match (`pub async fn $F` infers `function_item` correctly). Falls
back to live-scan on grammar drift, missing section, or a partial
section after `vex update` — `--why` reports the exact reason.

**Examples**:

```bash
# Multi-line function body with named captures
vex pattern 'fn $NAME($$ARGS) -> Result<$T, $E> { $$$BODY }' --lang rust

# Both struct and impl for the same type in one file
vex pattern 'struct $S && impl $S' --lang rust

# Interface OR class with the same name
vex pattern 'interface $N || class $N' --lang typescript

# See which mode and what narrowing happened
vex pattern 'fn $N($$$)' --lang rust --why 2>trace.json
```

## Benchmarks

Compared against [ast-index](https://github.com/defendend/Claude-ast-index-search) v3.31.0 (SQLite + FTS5) and [ripgrep](https://github.com/BurntSushi/ripgrep) 14.x.

### Indexing

| Project | vex | ast-index | Speedup | vex size | ast-index size |
|---------|-----|-----------|---------|----------|----------------|
| Small (2K lines Rust) | **16 ms** | 48 ms | **3.0x** | 43 KB | 490 KB |
| Medium (31K lines Rust) | **37 ms** | 112 ms | **3.0x** | 314 KB | 3.4 MB |
| Large (1247 Python files) | **183 ms** | 633 ms | **3.5x** | 1.8 MB | 15.9 MB |

Index size: **10-11x smaller** than ast-index (mmap binary + FST vs SQLite + FTS5).

Note: projects with `--semantic` indexing are slower due to ONNX embedding generation.

### Search: vex vs ast-index vs ripgrep

#### Medium project (31K lines Rust, avg 10 runs)

| Query | vex | ast-index | rg -w | vex vs rg |
|-------|-----|-----------|-------|-----------|
| Query A | **4.9 ms** | 9.5 ms | 54.2 ms | **11x** |
| Query B | **4.6 ms** | 9.5 ms | 8.9 ms | **1.9x** |
| Query C | **4.5 ms** | 9.2 ms | 8.6 ms | **1.9x** |
| Query D | **5.0 ms** | 12.1 ms | 9.3 ms | **1.9x** |

#### Large project (20K symbols, Python/JS/SQL, avg 10 runs)

| Query | vex | ast-index | rg -w | vex vs rg | Results (def/text) |
|-------|-----|-----------|-------|-----------|-------------------|
| Symbol 1 | **6.0 ms** | 59.7 ms | 84.6 ms | **14x** | 1 / 4 |
| Symbol 2 | **3.7 ms** | 44.5 ms | 78.5 ms | **21x** | 2 / 5 |
| Symbol 3 | **3.9 ms** | 22.7 ms | 76.7 ms | **20x** | 1 / 20 |
| Symbol 4 | **3.8 ms** | 43.1 ms | 77.5 ms | **21x** | 1 / 2 |
| Symbol 5 | **3.6 ms** | 33.7 ms | 77.3 ms | **21x** | 1 / 22 |
| Symbol 6 | **3.8 ms** | 43.3 ms | 76.9 ms | **20x** | 1 / 8 |
| Symbol 7 | **4.0 ms** | 42.5 ms | 74.9 ms | **19x** | 1 / 6 |
| Symbol 8 | **3.7 ms** | 42.8 ms | 78.4 ms | **21x** | 1 / 2 |

**Key takeaway**: vex search is constant ~4 ms (FST O(query_len)), regardless of project size — but this assumes a pre-built index. The comparison with ripgrep is not apples-to-apples: rg scans raw text with no indexing, while vex looks up a pre-built index. The real advantage is amortized: vex returns only symbol definitions (precise, token-efficient), while rg returns all text occurrences (noisy, expensive in LLM contexts).

### Pattern Matching (vex only)

| Pattern | Time | Matches |
|---------|------|---------|
| `fn $NAME($$$) -> Result` | 31 ms | 50 |
| `pub struct $NAME` | 32 ms | 45 |
| `fn $NAME($$$)` | 31 ms | 50 |

ast-index and ripgrep do not support AST pattern matching.

### Semantic Search

Queries where structural search returns 0 results but semantic finds relevant symbols:

| Query | Structural | Semantic |
|-------|-----------|----------|
| "parse source code files" | 0 | **19** |
| "database storage" | 0 | **20** |
| "find implementations of an interface" | 0 | **20** |
| "file system directory walker" | 0 | **20** |
| "handle errors and exceptions" | 0 | **20** |

### HNSW vs Brute-Force (semantic vector search)

Semantic search embeds the query via ONNX (~55ms) then searches stored vectors. HNSW (usearch) replaces brute-force O(N) scan with O(log N) approximate nearest neighbor search:

| Symbols | Brute-force | HNSW | Speedup |
|---------|-------------|------|---------|
| 333 | ~3 ms | ~3 ms | 1x |
| 11K | ~8 ms | ~3 ms | **2.3x** |
| 20K | ~11 ms | ~3 ms | **4x** |
| 100K (projected) | ~55 ms | ~3 ms | **~18x** |

HNSW stays constant ~3ms regardless of index size. Brute-force grows linearly. Total semantic search latency is dominated by ONNX embedding (~55ms), so end-to-end speedup is modest for small codebases but critical at scale.

| Mode | Latency |
|------|---------|
| Structural only | ~4 ms |
| Hybrid (structural + semantic) | ~58 ms (HNSW) / ~66 ms (brute-force) |

### LLM Token Efficiency

When an AI agent searches code, the output goes directly into the context window. Grep-based tools return every text occurrence — including comments, strings, variable usage, and matches in minified files — consuming tokens without adding signal.

vex returns only symbol definitions in a compact one-line format, drastically reducing token consumption:

| | vex compact | rg (grep) | Reduction |
|---|---|---|---|
| 7 symbol lookups (typical) | **~220 tokens** | ~1,300 tokens | **6x** |
| Queries hitting minified JS/CSS | **~270 tokens** | ~58,700 tokens | **217x** |

Example — searching for a class name on a large project:

```
# rg: 20 matches across imports, usage sites, comments, tests (2,045 chars)
$ rg -w "PreAggregatedConfig" .
./models.py:3602:class PreAggregatedConfig(models.Model):
./models.py:3610:    pre_aggregated_config = PreAggregatedConfig.objects.get(...)
./serializers.py:48:from .models import PreAggregatedConfig
./tests.py:12:    config = PreAggregatedConfig(...)
... (16 more lines)

# vex: 1 definition (93 chars)
$ vex search "PreAggregatedConfig" --format compact
C PreAggregatedConfig models.py:3602 class PreAggregatedConfig(models.Model):
```

For an agent making 10-20 code lookups per task, vex saves **5,000-20,000 tokens per session** compared to grep — reducing cost and leaving more context window for reasoning.

## Supported Languages

19 languages indexed via tree-sitter. The capability columns:

- **Binder** — does `vex usages --strict` resolve refs through an
  LSP-style scope chain (Phase 11.1)? `cross-file` includes
  `use` / `import` resolution; `in-file` resolves within a file but
  treats imports as unresolved. The remaining languages fall back to
  the line-based scanner used by plain `vex usages`.
- **Patterns** — does `vex pattern` get the v6 indexed prefilter
  (Phase 11.4)? `indexed` means a persisted skeleton section narrows
  candidate files at query time; `live-scan` means tree-sitter walks
  every lang-matching file on each query. All 19 languages work with
  `vex pattern` syntax (`$NAME`, `$$$BODY`, `&&` / `||`); the
  prefilter just speeds up discovery for the three T1 languages.

| Language | Extensions | Symbols | Imports | Binder | Patterns |
|----------|------------|---------|---------|--------|----------|
| Rust | `.rs` | functions, structs, enums, traits, impls, types, constants | `use` declarations | cross-file | indexed |
| TypeScript/JS | `.ts`, `.tsx`, `.js`, `.jsx` | classes, interfaces, enums, functions, arrows, type aliases | `import` | cross-file | indexed |
| Python | `.py` | classes, functions (incl. async, decorated) | `import`, `from..import` | cross-file | indexed |
| C# | `.cs` | classes, interfaces, structs, enums, methods, properties | — | in-file | live-scan |
| C/C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx`, `.h` | classes, structs, functions, methods, templates, enums | `#include` | cross-file (v1.14 BFS over quoted `#include "..."`; class methods still in-file) | live-scan |
| Go | `.go` | functions, methods, structs, interfaces | `import` | — | live-scan |
| Java | `.java` | classes, interfaces, enums, methods, constructors | `import` | — | live-scan |
| Kotlin | `.kt`, `.kts` | classes, interfaces, objects, functions, properties | `import` | — | live-scan |
| Ruby | `.rb` | classes, modules, methods | — | — | live-scan |
| Swift | `.swift` | classes, structs, enums, actors, protocols, functions | `import` | — | live-scan |
| PHP | `.php`, `.phtml` | classes, interfaces, traits, methods, functions | `use`, `require` | — | live-scan |
| SQL | `.sql` | tables, views, functions, triggers, indexes, schemas, types, sequences | `ALTER TABLE` refs | — | live-scan |
| Markdown | `.md`, `.markdown` | headings (section structure) | — | — | live-scan |
| Bash | `.sh`, `.bash` | functions | — | — | live-scan |
| Lua | `.lua` | functions, local functions, tables | `require` | — | live-scan |
| CSS | `.css` | rules, selectors, `@keyframes` | — | — | live-scan |
| HTML | `.html`, `.htm` | custom elements (hyphenated tag names) | — | — | live-scan |
| YAML | `.yaml`, `.yml` | top-level keys | — | — | live-scan |
| TOML | `.toml` | bare keys, dotted keys, tables | — | — | live-scan |

See [docs/SUPPORTED_LANGUAGES.md](docs/SUPPORTED_LANGUAGES.md) for grammar
versions, ABI level, and the runbook for adding a language or upgrading a
grammar. Adding a language to the indexed-Patterns tier is one
allowlist edit in `src/pattern/skeleton.rs` — see the Phase 11.4
follow-up notes for the planned Go → Java → Kotlin → C# → C++ → Swift
→ PHP → Ruby promotion order.

## Index Location

```
macOS:   ~/Library/Caches/vex/<hash>/index.vex
Linux:   $XDG_CACHE_HOME/vex/<hash>/index.vex
```

Each project gets its own index based on a hash of the project root path.

## Known limitations

vex is a static-analysis tool — some real call sites and references are
invisible by construction. The headline gaps:

- **`vex callers` outside function scope** — Module-level expressions
  are reported via synthetic `<module:path>` callers (Phase 14.1).
  Python + Java function/method decorators (Phase 14.2), Kotlin
  annotations + C# method/constructor attributes (Phase 14.2.2), and
  TypeScript method decorators + Rust outer attributes on fns/methods
  (Phase 14.2.1) emit forward edges. Remaining gap: class-level
  decorators (Phase 14.6); Rust `#[derive(...)]` is intentionally
  filtered.
- **`vex usages` quality depends on language.** Rust / TypeScript /
  Python / C# / C++ get `--strict` (binder-resolved refs from the
  v5 `reference_edges` section, Phase 11.1). Other languages use a
  line-based identifier scan with a higher false-positive rate.
- **Dynamic dispatch is invisible.** String-resolved factories
  (`uvicorn.run("main:app")`), task queues (`celery_task.delay()`),
  reflection (`getattr(obj, name)()`) — none of these produce edges.
- **Workaround**: `vex grep '\bname\b'` is the exhaustive textual
  fallback. Slower (~50 ms) but never misses a hit.

See [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) for the full coverage
matrix, repros, and recommendations per query type.

## Troubleshooting

### Surfacing internal warnings

Vex emits structured logs via the `tracing` crate at `parse`/`store`
boundaries — failed grammar loads, mmap reopens, manifest mismatches,
and so on. By default `RUST_LOG` is unset, so only the most critical
diagnostics make it to stderr.

When a search returns surprising results or an index command behaves
oddly, raise the log level:

```bash
RUST_LOG=vex=warn vex search Foo
RUST_LOG=vex=info vex index   # noisier — file-level progress
```

For *what the search engine actually did* (per-channel hit counts,
fuzzy fallback engagement, applied filters), use the structured trace
instead:

```bash
vex search Foo --why 2>trace.json   # trace lands on stderr as JSON
```

See [`docs/MCP-SCHEMA.md`](docs/MCP-SCHEMA.md) for the `--why` /
`why: true` JSON shape.

## Integration

### Claude Code (CLI Integration)

The recommended way to integrate vex with Claude Code is via `CLAUDE.md` rules (see below). Vex runs as a CLI tool — Claude Code calls it directly via Bash, no MCP server needed.

**Setup:**

```bash
# Install vex
brew tap tenatarika/tap && brew install vex

# In your project
cd /path/to/project
vex init              # create .vex.toml
vex index             # build index (add --semantic for meaning-based search)
```

Then add `.vex.toml` config for auto-update so Claude always searches a fresh index:

```toml
# .vex.toml
auto_update = true
# format = "compact"   # already the default since v1.10.1 — set "text" if you'd rather see verbose output
```

### Claude Code (MCP Server)

Alternatively, vex includes an MCP server (`vex-mcp`) that exposes all commands as MCP tools. Since **v1.11.2** a prebuilt `vex-mcp` binary ships in every release alongside `vex` for the three triples the build matrix covers: `aarch64-apple-darwin` (macOS Apple Silicon), `x86_64-unknown-linux-gnu` (Linux), and `x86_64-pc-windows-msvc` (Windows). Intel-Mac and other triples still require the source build below.

```bash
# 1. Download the prebuilt for your platform from
#    https://github.com/tenatarika/vex/releases/latest
#    e.g. vex-mcp-aarch64-apple-darwin.tar.gz / vex-mcp-x86_64-pc-windows-msvc.tar.gz
# 2. Extract and put the binary on PATH (or remember the full path).

# Source build (if you prefer or are on an unsupported triple)
cargo build --release -p vex-mcp

# Add to Claude Code MCP config (~/.claude/claude_desktop_config.json)
{
  "mcpServers": {
    "vex": {
      "command": "/path/to/vex-mcp",
      "env": {
        "VEX_ROOT": "/path/to/your/project"
      }
    }
  }
}
```

**MCP Tools (23):**
- `search` — 3-way hybrid (structural + BM25 + semantic); accepts `filter` / `include` / `exclude` / `kind` / `context_path` / `no_bm25` / `--why` / metadata filters / diff-scope (`since` / `since_branched` / `changed_only`)
- `find_symbol` — exact name lookup
- `find_similar` — semantic search by free-form description
- `similar` — nearest neighbors of an existing symbol (`explain` adds Jaccard + diff); diff-scope
- `duplicates` — near-duplicate symbol pairs (`explain` shows what differs); diff-scope
- `show` — extract symbol body from source; Phase 13.3 truncation flags (`signature_only` / `head` / `no_body` / `collapsed`, mutually exclusive)
- `outline` — file structure
- `usages` — find all references to a symbol; `filter` / `--strict` / `--why`
- `grep` — regex content search
- `pattern` — AST pattern matching with metavar back-references; diff-scope; `--why`
- `implementations` — find types extending a base class/trait/interface (incl. generics); diff-scope
- `callers` / `callees` — direct callgraph navigation (fast path via persistent index); diff-scope
- `paths` — enumerate caller chains between two functions
- `reachable` — transitive callers of a target
- `diff` — symbol-level diff between a git revision and the working tree
- `check` — fast symbol existence check
- `bundle` — unified multi-source bundle (`mode: symbol | pr-impact | project`), Phase 13 envelope
- `eval` — ranking-evaluation harness (`bench` / `min_ndcg`), MCP defaults `json: true` so agents get a structured `EvalReport`
- `capabilities` — machine-readable capability matrix (`protocol_version`, `signals`, `bundle_modes`, etc.)
- `index` / `update` — build/rebuild index
- `status` — index statistics

**MCP ↔ CLI parity (v1.10):** the schemas now mirror the CLI surface for every path-aware tool. Glob filters (`include` / `exclude`), substring `filter`, `kind` boost, `context_path` proximity hint, `no_bm25`, Phase 13.3 truncation, diff-scope, and `no_stale_check` are exposed everywhere the CLI accepts them — agents no longer need to drop to bash for "Rust files under `crates/api/` since `main`"-style scoping.

The schemas follow a canonical vocabulary (`query` / `symbol` / `symbols` / `path` / `pattern` / `filter` / `include` / `exclude`); pre-v1.7 aliases (`name`, `file`, `names`, etc.) still work and emit `_meta.deprecated_args: [...]` in the JSON-RPC response. Malformed JSON-RPC input now returns the spec-compliant `-32700 Parse error` response (v1.9.2 fix) with a 512-codepoint echo of the offending line in the `data` field; broken-pipe / EOF on stdin cleanly shuts down the server instead of dropping in-flight tool calls. See [`docs/MCP-SCHEMA.md`](docs/MCP-SCHEMA.md).

For other MCP-compatible clients (Cursor, Codex CLI, Windsurf, Cline, Continue.dev, Zed), see [Other MCP Clients](#other-mcp-clients) below — same `vex-mcp` binary, different config files.

### Other MCP Clients

The same `vex-mcp` binary works with any MCP-compatible client. The binary install is identical to the Claude Code section above; only the per-client config file location and format differ.

**One-line setup (v1.15.0+)**:

```bash
vex mcp install --agent cursor       # or any of: claude-code, codex-cli, windsurf, cline, continue, zed
vex mcp install --agent all          # fan out across every supported agent
vex mcp install --agent cursor --dry-run   # preview the post-merge config without writing
```

`vex mcp install` reads your existing agent config, merges a single `vex` server entry without disturbing siblings, and writes back atomically. Idempotent — re-running on a matching entry is a no-op skip (`--force` overrides). `vex mcp uninstall --agent <X>` removes the entry; `vex mcp list` enumerates current entries per agent. The same seven config files documented below are exactly what `vex mcp install` writes — keep [`integrations/`](integrations/) handy for manual edits, agents the auto-installer doesn't know yet, or anything more exotic than the default shape.

Copy-pasteable snippets for the most common ones live under [`integrations/`](integrations/):

| Agent              | Snippet                                                                            | Target file on disk                                                  |
| ------------------ | ---------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| Claude Code        | [`integrations/claude-code/claude_desktop_config.json`](integrations/claude-code/claude_desktop_config.json) | `~/.claude/claude_desktop_config.json`                               |
| Cursor             | [`integrations/cursor/mcp.json`](integrations/cursor/mcp.json)                     | `~/.cursor/mcp.json` *or* `<project>/.cursor/mcp.json`               |
| Codex CLI (OpenAI) | [`integrations/codex-cli/config.toml`](integrations/codex-cli/config.toml)         | `~/.codex/config.toml` *or* `<project>/.codex/config.toml`           |
| Windsurf (Codeium) | [`integrations/windsurf/mcp_config.json`](integrations/windsurf/mcp_config.json)   | `~/.codeium/windsurf/mcp_config.json`                                |
| Cline (VS Code)    | [`integrations/cline/mcp.json`](integrations/cline/mcp.json)                       | Cline panel → MCP Servers → Configure tab                            |
| Continue.dev       | [`integrations/continue/vex.yaml`](integrations/continue/vex.yaml)                 | `<project>/.continue/mcpServers/vex.yaml`                            |
| Zed                | [`integrations/zed/settings.json`](integrations/zed/settings.json)                 | `~/.config/zed/settings.json`                                        |

Per-agent caveats (auto-approve flags, timeout overrides, agent-mode requirements) are documented in [`integrations/README.md`](integrations/README.md).

### Agent Recipes & Workflows

Once vex-mcp is wired into your agent, the next question is *what to ask the agent so it picks the right tools in the right order*. [`docs/COOKBOOK.md`](docs/COOKBOOK.md) is a recipe collection for the common chains — code archaeology, cross-file refactor with `usages --strict` verification, PR-impact analysis via `bundle(mode="pr-impact")`, dead-code & duplicate cleanup, and multi-repo orchestration. Each recipe shows the tool sequence, the *why* of the ordering, and a phrase that reliably triggers the chain in agent prompts.

### Shell Integration

```bash
# Shell completions (tab-completion for commands and flags)
vex completions bash > ~/.bash_completion.d/vex   # Bash
vex completions zsh > ~/.zfunc/_vex               # Zsh (add ~/.zfunc to fpath)
vex completions fish > ~/.config/fish/completions/vex.fish  # Fish

# Aliases — add to .zshrc / .bashrc
alias vx="vex search"
alias vxu="vex usages"
alias vxi="vex index --path ."
alias vxs="vex index --path . --semantic"
alias vxw="vex watch"
```

### CLAUDE.md Integration

Add this to your project's `CLAUDE.md` to make Claude Code use vex instead of grep:

```markdown
## Code Search

Before first use in a project, run `vex init` to generate `.vex.toml`, then `vex index` to build the index.
Set `auto_update = true` in `.vex.toml` so the index stays fresh automatically.

Use vex for code search instead of grep or manual file reading:

- `vex search "SymbolName"` — find symbol definitions (~4ms)
- `vex show "SymbolName"` — extract symbol body (use INSTEAD of Read for specific symbols)
- `vex show "A" "B" "C"` — extract multiple symbols at once
- `vex usages "SymbolName"` — find all references
- `vex grep "pattern"` — regex content search (when you need text, not symbols)
- `vex search "description" --semantic` — search by meaning
- `vex search "rare_term"` — BM25 channel finds rare terms in symbol bodies (auto-on when index has BM25 data)
- `vex pattern 'class $NAME(BaseModel):' --lang python` — AST pattern matching with metavariables
- `vex pattern 'fn $N($$ARGS) -> Result<$T, $E> { $$$BODY }' --lang rust` — multi-line `$$$BODY` / `$$ARGS` capture
- `vex pattern 'struct $S && impl $S' --lang rust` — AND composition (back-ref `$S` must agree across both shapes)
- `vex pattern 'interface $N || class $N' --lang typescript` — OR composition (union, deduped by `(path, line)`)
- `vex pattern '<pat>' --lang <lang> --why` — emit ScanTrace on stderr (mode / candidate vs total / fallback reason)
- `vex outline path/to/file.py` — file structure overview
- `vex implementations "BaseService"` — find types extending a class/interface
- `vex callers "function_name"` — find all callers (~4ms via persistent call graph)
- `vex callees "function_name"` — find all callees (~4ms via persistent call graph)
- `vex similar "SymbolName"` — semantically close symbols (requires --semantic index)
- `vex duplicates --threshold 0.95` — near-duplicate symbol pairs
- `vex check "A" "B" "C"` — fast symbol existence check
- `vex paths "from" "to"` — enumerate caller chains between two functions (multi-hop)
- `vex reachable "Target"` — transitive callers of a target (blast-radius analysis)
- `vex diff --base main` — symbol-level diff against a branch (added / removed / moved / body-changed)
- `vex bundle --mode symbol --symbol Foo` — single-call body + callers + callees + similar (replaces 4 round-trips)
- `vex bundle --mode pr-impact --base origin/main` — changed symbols + transitive callers + tests on the current branch

All commands support `--filter "path/"` to narrow results to a directory. Most search-shaped commands also accept `--since <rev>` / `--since-branched` / `--changed-only` for diff-scoping.

### Rules
- **Always prefer `vex show` over `Read`** when you need a specific function or class
- **Always prefer `vex search` over `Grep`** when looking for symbol definitions
- **Use `vex grep` instead of `Grep`** for searching inside string literals, comments, or config values
- **Use `--format compact`** for token-efficient output in automated workflows
- **Use `--kind fn`** to boost results matching a specific symbol kind (fn, struct, trait, class, etc.)
- **Use `--context-path`** with the path of the file you are currently editing to boost nearby results
- **Run `vex update` after modifying source files** if `auto_update` is not enabled in `.vex.toml`
- **Use `vex pattern ... --why`** to debug match counts — the trace tells you whether the indexed prefilter ran or fell back to live-scan, and why
- **Indexed pattern prefilter requires a full `vex index`** — after `vex update` the section is partial and `vex pattern` automatically degrades to live-scan (reason `partial-section` in `--why`)

### Indexing
- `vex index` — full structural index + pattern skeleton section (v6)
- `vex index --semantic` — with embeddings (slower, enables semantic search)
- `vex update` — incremental update (only changed files)
- `vex index --no-pattern-index` — skip the v6 pattern skeleton section if you don't use `vex pattern` (sticky across `vex update`)
```

## Testing

### Unit & Integration Tests

```bash
cargo test                    # 1973 tests — unit, integration, property-based, adversarial
cargo clippy -- -D warnings   # zero warnings policy
```

Test coverage includes:
- **Per-language grammar regression** (NEW): `tests/<lang>_query_test.rs` for all 19 supported languages — catches ABI mismatches and AST node renames when a tree-sitter grammar crate is upgraded
- **Binary format**: roundtrip, corrupted/truncated/wrong-version rejection, out-of-bounds access, string pool dedup, empty index
- **Adversarial format**: 20 crafted index tests — overflow offsets, bad magic/version, alignment attacks, truncated records
- **Vectors**: write/read roundtrip for 384-dim f32 embeddings
- **FST**: refs FST roundtrip, prefix search, symbol FST exact/prefix/fuzzy search
- **Search**: structural, fuzzy (Levenshtein), RRF fusion, reranking with kind/path/proximity boosts
- **Reranking stress**: NaN/Infinity/zero scores, 10K results, edge context paths
- **Property-based** (proptest): rerank preserves length, sorted output, no NaN/negative scores, fusion commutativity
- **Incremental update**: unchanged reuse, deleted removal, file rename, symbol move between files, empty file
- **Concurrency**: parallel index/update (lock serialization), concurrent readers, read during reindex
- **Multi-language**: Rust, Python, Go, Kotlin, TypeScript, C++, cross-language same-name, wrong extension, 1K-symbol file, deep nesting, error recovery
- **Unicode**: BOM, mixed CRLF, unicode identifiers, null bytes, empty/whitespace files
- **Path edges**: spaces in paths, deep nesting (20 levels), symlinks, absolute vs relative, Windows backslashes
- **Callgraph**: callers/callees for Rust, Python, Go, TypeScript, Java
- **Persistent call graph (v1.5)**: format v4 roundtrip, callers/callees FST lookup, dedup, same-name-across-files isolation, same-name-within-file disambiguation, incremental update preserves edges, fallback to live scan for v3
- **Similar/duplicates (v1.5)**: self-exclusion, threshold filtering, canonical pair dedup, body-length filter, empty-index handling
- **Pluggable embedder (v1.5)**: registry lookup, mismatch detection (incl. back-compat for pre-9.1 manifests), config + CLI priority, writer variable `vector_dim`
- **BM25 channel (v1.5)**: writer/reader roundtrip, pipeline emission, IDF discrimination, short-doc preference, 3-way RRF with Hybrid labeling, MatchType tagging, unicode tokens
- **Staleness**: git HEAD comparison, dirty file detection, mtime fallback

### Fuzz Testing

Fuzz tests exercise every parser that consumes untrusted input — the
binary index format, sidecar files, the user-facing pattern grammar,
and the JSON manifest — using [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
(libFuzzer + AddressSanitizer):

```bash
# Install (once)
cargo install cargo-fuzz

# Generate seed corpus for every target
bash fuzz/generate_seeds.sh

# Run (requires nightly)
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_index_reader        -- -max_total_time=120
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_refs_fst            -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_symbol_fst          -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_bloom_load          -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_pattern_parser      -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_manifest_load       -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_marker_load         -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_tokenize_document   -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_hash_index_load     -- -max_total_time=60
```

Nine fuzz targets cover the reader's `unsafe` paths plus every text /
sidecar parser that takes adversarial input:

| Target | What it fuzzes | Surface |
|--------|---------------|---------|
| `fuzz_index_reader` | Arbitrary bytes as `.vex` file | `header()`, `symbol()`, `vector()`, `read_string()`, `file_paths()` |
| `fuzz_refs_fst` | Arbitrary FST + posting bytes | `RefReader::find()`, `find_by_prefix()` |
| `fuzz_symbol_fst` | Arbitrary FST + posting bytes | `SymbolFstReader::find()`, `find_fuzzy()`, `search_with_fallback()` |
| `fuzz_bloom_load` (v1.12.0) | Arbitrary `index.bloom` sidecar | `SymbolBloom::load`, then `may_contain` probes |
| `fuzz_pattern_parser` (v1.12.0) | Arbitrary UTF-8 as a pattern string | `parse_composite_pattern` (metavars, `&&` / `||`, quoted segments) |
| `fuzz_manifest_load` (v1.12.0) | Arbitrary JSON as `manifest.json` | `Manifest::load` |
| `fuzz_marker_load` (v1.13.0) | Arbitrary text as `<onnx>.sha256.marker` | `verify_with_marker` parser + decision tree |
| `fuzz_tokenize_document` (v1.13.0) | Arbitrary UTF-8 as BM25 input | `tokenize_document` (post share-owning-String refactor) |
| `fuzz_hash_index_load` (v1.14.1) | Arbitrary bytes as `index.hashes` sidecar | `hash_index::load` (`VEXH` magic, MAX_COUNT guard, truncation) |

Most recent system-wide audit (v1.14.1, 2026-06-05): **5,792,231 total
iterations across all 9 targets in ~9 min wall-clock, zero crashes /
panics / AddressSanitizer hits / leaks.** Plus a focused 3,000,000-
iter run on `fuzz_hash_index_load` alone — clean. Coverage saturated
for the small binary-header parsers (bloom, marker, hash_index);
JSON / grammar parsers still surfacing new features at saturation
(`fuzz_manifest_load` reached `cov:1355 ft:4191 / 1311 corpus`,
`fuzz_pattern_parser` `cov:551 ft:3693 / 1210 corpus`).

Fuzzing has found and fixed five real defects across the project life:

- v1.x: out-of-bounds read on crafted `symbol_count`, misaligned
  pointer dereference on odd `symbols_offset`, unchecked section
  offsets exceeding file size (binary reader hardening).
- v1.12.0: `SymbolBloom::load` accepted a sidecar with `n_bits = 0`
  + `k_num = 0` whose consistency guard passed but later panicked
  inside `bloomfilter::Bloom::check` on `hash % 0`. Fix: reject
  degenerate sizes during load.
- v1.12.0: `SymbolBloom::load` accepted `k_num` up to ~2.1B, which
  made every `may_contain` call loop for 110+ seconds (DoS, not a
  panic). Fix: cap `k_num <= MAX_K_NUM = 64` at load time.

The v1.13.0 / v1.14.1 additions found no defects in fresh code — the
review-driven `MAX_COUNT` guards on `hash_index::save` / `load` were
added as defence-in-depth before the fuzzer ran (rust-reviewer +
code-reviewer flagged the truncating `as u32` cast on save), and the
sustained 3M / 5.8M iteration runs confirmed they hold.

## Architecture

```
CLI (clap) → Pipeline (rayon) → Tree-sitter → Binary Format v4 (mmap)
                                      ↓
                               Embedder trait (fastembed/MiniLM)
                                      ↓
                               HNSW Index (usearch)
                                      ↓
Search:    Symbol FST (structural) + BM25 (body) + HNSW (semantic) → 3-way RRF
Callers/Callees: Callers FST + Callees FST (persistent edges) → ~4ms
Usages:    Refs FST + Posting Lists → zero-copy refs lookup
Show:      Tree-sitter node boundaries → symbol body extraction
Similar:   HNSW nearest neighbors over stored embeddings
```

- **No SQLite** — custom binary format v4 with zero-copy mmap reads (v3 still readable)
- **Symbol FST** — persistent inverted index, O(query_len) lookup
- **Refs FST** — symbol references in Finite State Transducer, prefix search
- **Persistent call graph** — `CallEdge` records + callers/callees FSTs built at index time, ~4ms lookup vs seconds of live tree-sitter scan
- **BM25 channel** — Okapi BM25 over body identifiers, auto-on when section present
- **HNSW** — approximate nearest neighbor via usearch, O(log N) semantic search
- **Pluggable embedder** — `Embedder` trait + registry, identity recorded in manifest with mismatch detection at search
- **Parallel parsing** — rayon with 500-file chunks
- **Incremental updates** — content hashing via xxh3, only re-parse changed files (unchanged symbols + call edges reconstructed from existing index)
- **Watch mode** — notify crate with 500ms debouncing
- **3-way RRF fusion** — merges structural + BM25 + semantic ranked lists, marks cross-channel hits as `Hybrid`

## License

MIT
