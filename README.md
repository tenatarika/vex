# Vex

Fast hybrid structural + semantic code search. **V**ector + ind**ex**.

Parses source code with tree-sitter, stores symbols in a custom mmap'd binary format, and supports both exact name lookup and semantic search via ONNX embeddings. References stored in an FST (Finite State Transducer) for zero-copy lookup.

## Installation

```bash
# From source
git clone <repo-url>
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

# Incremental update (only changed files)
vex update

# Watch mode (re-indexes on file changes)
vex watch

# Show index stats
vex status
```

## Commands

| Command | Description |
|---------|-------------|
| `vex index [--path .] [--semantic]` | Build full index. `--semantic` generates embeddings. |
| `vex search <query> [--semantic] [--limit N]` | Search symbols. `--semantic` enables hybrid search. |
| `vex usages <name> [--limit N]` | Find all references/usages of a symbol (FST lookup). |
| `vex outline <file>` | Show file structure: symbols, kinds, lines. |
| `vex update [--path .] [--semantic]` | Incremental update — only re-index changed files. |
| `vex watch [--path .] [--semantic]` | Watch filesystem, auto re-index on changes. |
| `vex status [--path .]` | Show index stats: symbol count, size, embeddings. |

## Output Formats

```bash
# Human-readable (default)
vex search "Foo"

# JSON (for MCP/tool integration)
vex search "Foo" --format json
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

Compared against [ast-index](https://github.com/defendend/Claude-ast-index-search) v3.31.0 (SQLite + FTS5).

### Indexing

| Project | vex | ast-index | Speedup | vex size | ast-index size |
|---------|-----|-----------|---------|----------|----------------|
| Small (2K lines Rust) | **16 ms** | 48 ms | **3.0x** | 43 KB | 490 KB |
| Medium (31K lines Rust) | **37 ms** | 112 ms | **3.0x** | 314 KB | 3.4 MB |
| Large (1247 Python files) | **183 ms** | 633 ms | **3.5x** | 1.8 MB | 15.9 MB |

Index size: **10-11x smaller** than ast-index (mmap binary + FST vs SQLite + FTS5).

### Search (avg 10 runs, medium project)

| Query | ast-index | vex | Speedup |
|-------|-----------|-----|---------|
| "search" | 8.8 ms | **3.7 ms** | **2.4x** |
| "SymbolKind" | 8.1 ms | **3.6 ms** | **2.2x** |
| "parse_file" | 7.6 ms | **3.7 ms** | **2.0x** |
| "IndexReader" | 9.8 ms | **3.7 ms** | **2.7x** |

Search latency is constant (~3.7 ms) regardless of index size thanks to FST O(query_len) lookup.

### Usages (FST vs SQLite)

| Query | ast-index | vex | Speedup |
|-------|-----------|-----|---------|
| "SymbolKind" | 4.8 ms | **3.7 ms** | **1.3x** |

### Pattern Matching (vex only)

| Pattern | Time | Matches |
|---------|------|---------|
| `fn $NAME($$$) -> Result` | 31 ms | 50 |
| `pub struct $NAME` | 32 ms | 45 |
| `fn $NAME($$$)` | 31 ms | 50 |

ast-index does not support AST pattern matching.

### Semantic Search

Queries where structural search returns 0 results but semantic finds relevant symbols:

| Query | Structural | Semantic | Top results |
|-------|-----------|----------|-------------|
| "parse source code files" | 0 | **19** | parse_file, extract_refs, parse_file_symbols |
| "database storage" | 0 | **20** | populate_db, create_10k_db, setup_db |
| "find implementations of an interface" | 0 | **20** | find_implementations, test_interface_extends |
| "file system directory walker" | 0 | **20** | index_directory, walk_for_kind, find_project_root |
| "handle errors and exceptions" | 0 | **20** | try_recover_from_error, extract_parents_from_error_text |

| Mode | Latency |
|------|---------|
| Structural only | ~3.7 ms |
| Hybrid (structural + semantic) | ~55 ms |

## Supported Languages

| Language | Extensions | Parser |
|----------|------------|--------|
| Rust | `.rs` | tree-sitter |
| Python | `.py` | tree-sitter |
| Go | `.go` | tree-sitter |
| Java | `.java` | tree-sitter |
| C# | `.cs` | tree-sitter |
| Ruby | `.rb` | tree-sitter |
| Swift | `.swift` | tree-sitter |
| Kotlin | `.kt`, `.kts` | planned |
| TypeScript/JS | `.ts`, `.tsx`, `.js`, `.jsx` | planned |

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

**MCP Tools:**
- `search` — hybrid structural + semantic search
- `find_symbol` — exact name lookup
- `find_similar` — semantic search by description
- `outline` — file structure
- `index` / `update` — build/rebuild index
- `status` — index statistics

### Shell Integration

```bash
# Add to .zshrc / .bashrc
alias vx="vex search"
alias vxu="vex usages"
alias vxi="vex index --path ."
alias vxs="vex index --path . --semantic"
alias vxw="vex watch"
```

## Architecture

```
CLI (clap) → Pipeline (rayon) → Tree-sitter → Binary Format (mmap)
                                      ↓
                               ONNX Embeddings (fastembed)
                                      ↓
Search: Inverted Index + Cosine Similarity → RRF Fusion
Usages: FST (fst crate) + Posting Lists → zero-copy refs lookup
```

- **No SQLite** — custom binary format with zero-copy mmap reads
- **FST refs** — symbol references stored in Finite State Transducer, O(query_len) lookup
- **Parallel parsing** — rayon with 500-file chunks
- **Incremental updates** — content hashing via xxh3, only re-parse changed files
- **Watch mode** — notify crate with 500ms debouncing
- **Semantic search** — MiniLM-L6-v2 embeddings (384-dim), brute-force cosine similarity
- **RRF fusion** — merges structural + semantic ranked lists

## License

MIT
