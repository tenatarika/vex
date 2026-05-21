# Vex

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/tenatarika/vex/actions/workflows/ci.yml/badge.svg)](https://github.com/tenatarika/vex/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org/)
[![Commands](https://img.shields.io/badge/commands-19-blue.svg)]()
[![Languages](https://img.shields.io/badge/languages-19-blueviolet.svg)]()
[![Tests](https://img.shields.io/badge/tests-649-green.svg)]()

Fast hybrid structural + semantic code search. **V**ector + ind**ex**.

[Why Vex?](#why-vex) · [How It Compares](#how-it-compares) · [Installation](#installation) · [Quick Start](#quick-start) · [Commands](#commands) · [Configuration](#configuration) · [How Search Works](#how-search-works) · [Benchmarks](#benchmarks) · [Supported Languages](#supported-languages) · [Integration](#integration) · [Testing](#testing) · [Architecture](#architecture)

```
$ vex search "TelemetryProcessor"          # 4ms — find symbol definitions
$ vex search "timeout retry"               # NEW: BM25 finds rare body terms
$ vex show "TelemetryProcessor"            # extract just the class body (not the whole file)
$ vex search "handle alert" --semantic     # find by meaning, not just name
$ vex pattern 'fn $NAME($$$) -> Result'    # AST pattern matching (like ast-grep)
$ vex usages "Config"                      # who references this symbol?
$ vex implementations "BaseService"        # who extends/implements this?
$ vex callers "process_event"              # who calls this function? (~4ms — FST lookup)
$ vex similar "PaymentService"             # NEW: semantically close symbols
$ vex duplicates --threshold 0.95          # NEW: near-duplicate pairs
$ vex check "Foo" "Bar" "Baz"              # fast existence check
```

## Why Vex?

- **~4ms search** after indexing — FST-based O(query_len) lookup, not O(symbols). Requires a pre-built index (indexing takes 20ms-600ms+ depending on project size)
- **3-channel hybrid search** — structural FST (names) + BM25 (rare body terms) + semantic HNSW (meaning), fused via Reciprocal Rank Fusion. Find symbols when you don't know the exact name AND when generic semantic-only search would be too noisy
- **Persistent call graph** — `vex callers`/`vex callees` reads from an FST built at index time (~4ms), not a live tree-sitter scan (seconds)
- **Pluggable embedder** — `Embedder` trait + registry; swap MiniLM-L6-v2 for future code-specific models (BGE, CodeBERT) without touching call sites
- **Token-efficient** — compact output uses 6-88x fewer tokens than grep, `vex show` extracts just the symbol body instead of the whole file
- **19 languages** out of the box — Rust, Python, Go, Java, C/C++, C#, Ruby, Swift, Kotlin, TypeScript, SQL, Markdown, PHP, Bash, Lua, CSS, HTML, YAML, TOML
- **Single binary, zero config** — no LSP servers, no databases, no Docker. Just `vex index && vex search`

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

### Windows

Pre-built `vex.exe` ships in every GitHub Release.

1. Download `vex-x86_64-pc-windows-msvc.zip` from the [latest release](https://github.com/tenatarika/vex/releases/latest)
2. Extract `vex.exe` somewhere stable (e.g. `C:\Users\<you>\bin\`)
3. Add that folder to `PATH` (System Properties → Environment Variables → edit `Path` → add the folder)
4. Open a fresh terminal and run `vex --version`

To update, run `vex self-update` — it fetches the latest release, picks the right archive for your platform, and replaces the binary in-place. Same command works on macOS and Linux too.

## Quick Start

```bash
# Index a project (structural only — fast)
vex index --path /path/to/project

# Index with semantic embeddings (slower first time, downloads 86 MB model)
vex index --path /path/to/project --semantic

# Search by symbol name
vex search "PaymentService"

# Search by meaning (requires --semantic index)
vex search "payment processing" --semantic

# Find all usages of a symbol
vex usages "IndexReader"

# File structure outline
vex outline src/main.rs

# Find implementations of a trait/interface
vex implementations "Iterator"

# Callgraph: who calls / is called by a function (fast path via persistent index)
vex callers "process_event"
vex callees "process_event"

# Semantic similarity by existing symbol (requires --semantic index)
vex similar "PaymentService" --limit 5 --threshold 0.7

# Near-duplicate pairs (refactor / dedup helper)
vex duplicates --threshold 0.95 --min-body-lines 5

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
| `vex index [--path .] [--semantic] [--embedder ID]` | Build full index. `--semantic` generates embeddings + HNSW + BM25. `--embedder` selects embedding model (default `minilm-l6-v2`). |
| `vex search <query> [--semantic] [--no-bm25] [--limit N] [--kind fn] [--context-path p]` | Hybrid search: structural + BM25 + semantic (when `--semantic`). 3-way RRF fusion. |
| `vex show <symbol> [--limit N] [--context N] [--kind fn]` | Extract symbol body from source (saves tokens vs full file read). |
| `vex similar <name> [--limit N] [--threshold T]` | Find symbols semantically close to an existing one (HNSW nearest neighbors). |
| `vex duplicates [--threshold T] [--min-body-lines N]` | List near-duplicate symbol pairs by embedding similarity. |
| `vex usages <name> [--limit N]` | Find all references/usages of a symbol (FST lookup). |
| `vex pattern '<pat>' --lang <lang>` | AST pattern matching with metavariables ($NAME, $$$). |
| `vex outline <file> [--kind fn]` | Show file structure, optionally filter by symbol kind. |
| `vex implementations <name>` | Find types that extend/implement a base class, trait, or interface. |
| `vex callers <name>` | Find all functions that call a given function (fast path via persistent call graph). |
| `vex callees <name>` | Find all functions called by a given function (fast path via persistent call graph). |
| `vex check <name> [name...]` | Fast existence check — which symbols exist in the index? |
| `vex grep <pattern> [--filter path/]` | Regex content search (no index needed). |
| `vex update [--path .] [--semantic] [--embedder ID]` | Incremental update — re-parse only changed files, reuse unchanged symbols from existing index. |
| `vex watch [--path .] [--semantic] [--embedder ID]` | Watch filesystem, auto re-index on changes. |
| `vex status [--path .]` | Show index stats: symbol count, size, embeddings, call graph, BM25. |
| `vex completions <shell>` | Generate shell completions (bash, zsh, fish). |
| `vex init` | Create a default `.vex.toml` config file in the project root. |

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

# Default output format: "text", "json", or "compact"
format = "compact"

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

**How it works**: on every search, vex compares the git HEAD stored at index time with the current HEAD (~0.1ms, single `git rev-parse`). If HEAD changed → stale. For non-git repos, falls back to mtime comparison.

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
# Human-readable (default)
vex search "Foo"

# JSON (for MCP/tool integration)
vex search "Foo" --format json

# Compact (token-efficient, optimized for LLM context)
vex search "Foo" --format compact
```

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
A classic Okapi BM25 (`K1=1.2`, `B=0.75`) over symbol body tokens — identifiers, signatures, docstrings. Closes the gap between "exact name" (structural) and "general meaning" (semantic): finds **rare body terms** like `timeout`, `retry`, `singlestore`, `idempotency_key` that aren't part of any symbol name. Pass `--no-bm25` to disable per-call.

### Hybrid Search (3-way RRF)
When the index has all three channels (built with `--semantic`), `vex search` fuses structural + BM25 + semantic using **Reciprocal Rank Fusion**. Symbols hit by ≥2 channels rank as `Hybrid`; symbols unique to one keep their original match type. Cuts both structural-noise and semantic-blur in the same query.

### Usages (FST)
References stored in an FST (Finite State Transducer) — zero-copy lookup from mmap with prefix search support.

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

| Language | Extensions | Symbols | Imports |
|----------|------------|---------|---------|
| Rust | `.rs` | functions, structs, enums, traits, impls, types, constants | `use` declarations |
| Python | `.py` | classes, functions (incl. async, decorated) | `import`, `from..import` |
| Go | `.go` | functions, methods, structs, interfaces | `import` |
| Java | `.java` | classes, interfaces, enums, methods, constructors | `import` |
| C# | `.cs` | classes, interfaces, structs, enums, methods, properties | — |
| Ruby | `.rb` | classes, modules, methods | — |
| Swift | `.swift` | classes, structs, enums, actors, protocols, functions | `import` |
| Kotlin | `.kt`, `.kts` | classes, interfaces, objects, functions, properties | `import` |
| TypeScript/JS | `.ts`, `.tsx`, `.js`, `.jsx` | classes, interfaces, enums, functions, arrows, type aliases | `import` |
| SQL | `.sql` | tables, views, functions, triggers, indexes, schemas, types, sequences | `ALTER TABLE` refs |
| C/C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx`, `.h` | classes, structs, functions, methods, templates, enums | `#include` |
| Markdown | `.md`, `.markdown` | headings (section structure) | — |

See [docs/SUPPORTED_LANGUAGES.md](docs/SUPPORTED_LANGUAGES.md) for grammar
versions, ABI level, and the runbook for adding a language or upgrading a
grammar.

## Index Location

```
macOS:   ~/Library/Caches/vex/<hash>/index.vex
Linux:   $XDG_CACHE_HOME/vex/<hash>/index.vex
```

Each project gets its own index based on a hash of the project root path.

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
format = "compact"
```

### Claude Code (MCP Server)

Alternatively, vex includes an MCP server (`vex-mcp`) that exposes all commands as MCP tools:

```bash
# Build MCP server
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

**MCP Tools (17):**
- `search` — 3-way hybrid (structural + BM25 + semantic)
- `find_symbol` — exact name lookup
- `find_similar` — semantic search by free-form description
- `similar` — nearest neighbors of an existing symbol
- `duplicates` — near-duplicate symbol pairs
- `show` — extract symbol body from source
- `outline` — file structure
- `usages` — find all references to a symbol
- `grep` — regex content search
- `implementations` — find types extending a base class/trait
- `callers` / `callees` — callgraph navigation (fast path via persistent index)
- `check` — fast symbol existence check
- `index` / `update` — build/rebuild index
- `status` — index statistics

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
- `vex pattern 'class $NAME(BaseModel):' --lang python` — AST pattern matching
- `vex outline path/to/file.py` — file structure overview
- `vex implementations "BaseService"` — find types extending a class/interface
- `vex callers "function_name"` — find all callers (~4ms via persistent call graph)
- `vex callees "function_name"` — find all callees (~4ms via persistent call graph)
- `vex similar "SymbolName"` — semantically close symbols (requires --semantic index)
- `vex duplicates --threshold 0.95` — near-duplicate symbol pairs
- `vex check "A" "B" "C"` — fast symbol existence check

All commands support `--filter "path/"` to narrow results to a directory.

### Rules
- **Always prefer `vex show` over `Read`** when you need a specific function or class
- **Always prefer `vex search` over `Grep`** when looking for symbol definitions
- **Use `vex grep` instead of `Grep`** for searching inside string literals, comments, or config values
- **Use `--format compact`** for token-efficient output in automated workflows
- **Use `--kind fn`** to boost results matching a specific symbol kind (fn, struct, trait, class, etc.)
- **Use `--context-path`** with the path of the file you are currently editing to boost nearby results
- **Run `vex update` after modifying source files** if `auto_update` is not enabled in `.vex.toml`

### Indexing
- `vex index` — full structural index (fast)
- `vex index --semantic` — with embeddings (slower, enables semantic search)
- `vex update` — incremental update (only changed files)
```

## Testing

### Unit & Integration Tests

```bash
cargo test                    # 649 tests — unit, integration, property-based, adversarial
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

Fuzz tests exercise the binary format reader with arbitrary/corrupted data using [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer + AddressSanitizer):

```bash
# Install (once)
cargo install cargo-fuzz

# Generate seed corpus from local vex cache
bash fuzz/generate_seeds.sh

# Run (requires nightly)
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_index_reader -- -max_total_time=120
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_refs_fst -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_symbol_fst -- -max_total_time=60
```

Three fuzz targets cover all `unsafe` code paths in the reader:

| Target | What it fuzzes | Unsafe paths exercised |
|--------|---------------|----------------------|
| `fuzz_index_reader` | Arbitrary bytes as `.vex` file | `header()`, `symbol()`, `vector()`, `read_string()`, `file_paths()` |
| `fuzz_refs_fst` | Arbitrary FST + posting bytes | `RefReader::find()`, `find_by_prefix()` |
| `fuzz_symbol_fst` | Arbitrary FST + posting bytes | `SymbolFstReader::find()`, `find_fuzzy()`, `search_with_fallback()` |

Fuzzing found and fixed 3 bugs: out-of-bounds read on crafted `symbol_count`, misaligned pointer dereference on odd `symbols_offset`, and unchecked section offsets exceeding file size.

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
