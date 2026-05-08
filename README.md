# Vex

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org/)
[![Tests](https://img.shields.io/badge/tests-115_passing-brightgreen.svg)]()
[![Commands](https://img.shields.io/badge/commands-16-blue.svg)]()
[![Languages](https://img.shields.io/badge/languages-10-blueviolet.svg)]()

Fast hybrid structural + semantic code search. **V**ector + ind**ex**.

```
$ vex search "TelemetryProcessor"          # 4ms — find symbol definitions
$ vex show "TelemetryProcessor"            # extract just the class body (not the whole file)
$ vex search "handle alert" --semantic     # find by meaning, not just name
$ vex pattern 'fn $NAME($$$) -> Result'    # AST pattern matching (like ast-grep)
$ vex usages "Config"                      # who references this symbol?
$ vex implementations "BaseService"        # who extends/implements this?
$ vex callers "process_event"              # who calls this function?
$ vex check "Foo" "Bar" "Baz"             # fast existence check
```

## Why Vex?

- **4ms search** on any size codebase — FST-based O(query_len) lookup, not O(symbols)
- **14-21x faster than ripgrep** for symbol search on large projects
- **Semantic search** — "find payment processing" returns `ProcessPayment`, `ChargeCard`, `RefundOrder`
- **Token-efficient** — compact output uses 6-88x fewer tokens than grep, `vex show` extracts just the symbol body instead of the whole file
- **10 languages** out of the box — Rust, Python, Go, Java, C#, Ruby, Swift, Kotlin, TypeScript, SQL
- **Single binary, zero config** — no LSP servers, no databases, no Docker. Just `vex index && vex search`

## How It Compares

|  | **vex** | **ripgrep** | **ast-index** | **ast-grep** | **Serena** |
|---|---|---|---|---|---|
| **What it searches** | Symbol definitions | All text | Symbol definitions | AST patterns | Symbols (via LSP) |
| **Search speed** | **~4ms** (FST) | 75-120ms (disk scan) | 22-60ms (SQLite) | ~30ms (scan) | LSP-dependent |
| **Semantic search** | HNSW + embeddings | -- | -- | -- | -- |
| **Pattern matching** | `fn $NAME($$$)` | regex only | -- | `fn $NAME($$$)` | regex only |
| **Index size** | **5 MB** / 20K syms | no index | 190 MB / 20K syms | no index | no index |
| **Token efficiency** | **6-88x** fewer than rg | baseline | ~3x fewer than rg | N/A | N/A |
| **Symbol body extraction** | `vex show` | -- | -- | -- | -- |
| **Languages** | 10 | any | 10+ | 10+ | 40+ (LSP) |
| **Refactoring** | -- | -- | -- | -- | rename, move, inline |
| **Runtime deps** | none | none | none | none | Python + LSP |

**Best for**: fast symbol search in AI agent workflows where token efficiency matters. Not a replacement for LSP-based tools (no refactoring, no go-to-definition in dependencies).

## Installation

```bash
# Homebrew (macOS/Linux)
brew tap tenatarika/tap
brew install vex

# From source
git clone https://github.com/tenatarika/vex.git
cd vex
cargo build --release
cp target/release/vex ~/.local/bin/
```

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

# Callgraph: who calls / is called by a function
vex callers "process_event"
vex callees "process_event"

# Fast existence check
vex check "Foo" "Bar" "Baz"

# Incremental update (only changed files)
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
| `vex index [--path .] [--semantic]` | Build full index. `--semantic` generates embeddings + HNSW. |
| `vex search <query> [--semantic] [--limit N]` | Search symbols. `--semantic` enables hybrid search. |
| `vex show <symbol> [--limit N] [--context N]` | Extract symbol body from source (saves tokens vs full file read). |
| `vex usages <name> [--limit N]` | Find all references/usages of a symbol (FST lookup). |
| `vex pattern '<pat>' --lang <lang>` | AST pattern matching with metavariables ($NAME, $$$). |
| `vex outline <file> [--kind fn]` | Show file structure, optionally filter by symbol kind. |
| `vex implementations <name>` | Find types that extend/implement a base class, trait, or interface. |
| `vex callers <name>` | Find all functions that call a given function. |
| `vex callees <name>` | Find all functions called by a given function. |
| `vex check <name> [name...]` | Fast existence check — which symbols exist in the index? |
| `vex grep <pattern> [--filter path/]` | Regex content search (no index needed). |
| `vex update [--path .] [--semantic]` | Incremental update — only re-index changed files. |
| `vex watch [--path .] [--semantic]` | Watch filesystem, auto re-index on changes. |
| `vex status [--path .]` | Show index stats: symbol count, size, embeddings. |
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
```

CLI flags always override config values. Use `--no-semantic` to explicitly disable semantic mode when the config enables it.

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

### Hybrid Search (both flags)
When `--semantic` is used with an embedding-enabled index, results from both methods are merged using **Reciprocal Rank Fusion (RRF)**. Symbols found by both methods rank highest.

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

**Key takeaway**: vex search is constant ~4 ms (FST O(query_len)), regardless of project size. On large projects vex is **14-21x faster than ripgrep** and **6-16x faster than ast-index**. vex returns only symbol definitions (precise), while rg returns all text occurrences (noisy).

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
| Python | `.py` | classes, functions | `import`, `from..import` |
| Go | `.go` | functions, methods, structs, interfaces | `import` |
| Java | `.java` | classes, interfaces, enums, methods, constructors | `import` |
| C# | `.cs` | classes, interfaces, structs, enums, methods, properties | — |
| Ruby | `.rb` | classes, modules, methods | — |
| Swift | `.swift` | classes, protocols, enums, functions | `import` |
| Kotlin | `.kt`, `.kts` | classes, interfaces, objects, functions, properties | `import` |
| TypeScript/JS | `.ts`, `.tsx`, `.js`, `.jsx` | classes, interfaces, enums, functions, arrows, type aliases | `import` |
| SQL | `.sql` | tables, views, functions, triggers, indexes, schemas, types, sequences | `ALTER TABLE` refs |

## Index Location

```
macOS:   ~/Library/Caches/vex/<hash>/index.vex
Linux:   $XDG_CACHE_HOME/vex/<hash>/index.vex
```

Each project gets its own index based on a hash of the project root path.

## Integration

### Claude Code (MCP Server)

Vex includes an MCP server (`vex-mcp`) for integration with Claude Code and other AI agents:

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

**MCP Tools (15):**
- `search` — hybrid structural + semantic search
- `find_symbol` — exact name lookup
- `find_similar` — semantic search by description
- `show` — extract symbol body from source
- `outline` — file structure
- `usages` — find all references to a symbol
- `grep` — regex content search
- `implementations` — find types extending a base class/trait
- `callers` / `callees` — callgraph navigation
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

Use vex for code search instead of grep or manual file reading:

- `vex search "SymbolName"` — find symbol definitions (~4ms)
- `vex show "SymbolName"` — extract symbol body (use INSTEAD of Read for specific symbols)
- `vex show "A" "B" "C"` — extract multiple symbols at once
- `vex usages "SymbolName"` — find all references
- `vex grep "pattern"` — regex content search (when you need text, not symbols)
- `vex search "description" --semantic` — search by meaning
- `vex pattern 'class $NAME(BaseModel):' --lang python` — AST pattern matching
- `vex outline path/to/file.py` — file structure overview
- `vex implementations "BaseService"` — find types extending a base class/trait
- `vex callers "process_event"` — find functions that call this
- `vex callees "process_event"` — find functions called by this
- `vex check "Foo" "Bar"` — fast symbol existence check

All commands support `--filter "path/"` to narrow results to a directory.

### Rules
- **Always prefer `vex show` over `Read`** when you need a specific function or class
- **Always prefer `vex search` over `Grep`** when looking for symbol definitions
- **Use `vex grep` instead of `Grep`** for searching inside string literals, comments, or config values
- **Use `--format compact`** for token-efficient output in automated workflows
```

## Architecture

```
CLI (clap) → Pipeline (rayon) → Tree-sitter → Binary Format (mmap)
                                      ↓
                               ONNX Embeddings (fastembed)
                                      ↓
                               HNSW Index (usearch)
                                      ↓
Search: Symbol FST (structural) + HNSW (semantic) → RRF Fusion
Usages: Refs FST + Posting Lists → zero-copy refs lookup
Show:   Tree-sitter node boundaries → symbol body extraction
```

- **No SQLite** — custom binary format with zero-copy mmap reads
- **Symbol FST** — persistent inverted index, O(query_len) lookup
- **Refs FST** — symbol references in Finite State Transducer, prefix search
- **HNSW** — approximate nearest neighbor via usearch, O(log N) semantic search
- **Parallel parsing** — rayon with 500-file chunks
- **Incremental updates** — content hashing via xxh3, only re-parse changed files
- **Watch mode** — notify crate with 500ms debouncing
- **Semantic search** — MiniLM-L6-v2 embeddings (384-dim), HNSW with brute-force fallback
- **RRF fusion** — merges structural + semantic ranked lists

## License

MIT
