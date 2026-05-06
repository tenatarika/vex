# Vex

Fast hybrid structural + semantic code search. **V**ector + ind**ex**.

Parses source code with tree-sitter, stores symbols in a custom mmap'd binary format, and supports both exact name lookup and semantic search via ONNX embeddings.

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
- `"payment processing"` finds `process_payment`, `ChargeUseCase`, `InvoiceService`
- `"database connection"` finds `ConnectionPool`, `DbSession`, `open_db`

### Hybrid Search (both flags)
When `--semantic` is used with an embedding-enabled index, results from both methods are merged using **Reciprocal Rank Fusion (RRF)**. Symbols found by both methods rank highest.

## Supported Languages

| Language | Extensions |
|----------|------------|
| Rust | `.rs` |
| Python | `.py` |
| Go | `.go` |
| Kotlin | `.kt`, `.kts` |
| TypeScript/JavaScript | `.ts`, `.tsx`, `.js`, `.jsx` |

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
- `index` — build/rebuild index
- `status` — index statistics

### Shell Integration

```bash
# Add to .zshrc / .bashrc
alias vx="vex search"
alias vxi="vex index --path ."
alias vxs="vex index --path . --semantic"
alias vxw="vex watch"
```

### CI / Pre-commit

```bash
# Rebuild index before commit
vex index --path .
```

## Architecture

```
CLI (clap) → Pipeline (rayon) → Tree-sitter → Binary Format (mmap)
                                      ↓
                               ONNX Embeddings (fastembed)
                                      ↓
Search: Inverted Index + Cosine Similarity → RRF Fusion
```

- **No SQLite** — custom binary format with zero-copy mmap reads
- **Parallel parsing** — rayon with 500-file chunks
- **Incremental updates** — content hashing via xxh3, only re-parse changed files
- **Watch mode** — notify crate with 500ms debouncing

## License

MIT
