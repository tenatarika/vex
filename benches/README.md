# Benchmarks

Performance benchmarks comparing vex against ast-index.

## Quick Start

```bash
# Build release binary first
cargo build --release

# Run all benchmarks
./benches/bench.sh

# Quick mode (skip large projects)
./benches/bench.sh --quick

# Benchmark a specific project
./benches/bench.sh --project /path/to/project
```

## What It Measures

| Benchmark | Description |
|-----------|-------------|
| **Indexing** | Time to build full index from scratch |
| **Index size** | Size of index file on disk |
| **Search** | Avg latency over 10 runs for structural search |
| **Usages** | FST ref lookup vs ast-index SQLite query |
| **Pattern** | AST pattern matching (vex only) |
| **Grep** | vex symbol search vs ripgrep raw text scan |

## Results

Results are saved to `benches/results/` (gitignored, machine-specific).

Each run creates a timestamped file: `bench_20260507_143022.txt`

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `VEX` | `target/release/vex` | Path to vex binary |
| `AST` | `ast-index` | Path to ast-index binary |
| `RG` | `rg` | Path to ripgrep binary |
| `VEX_BENCH_LARGE_PROJECTS` | (none) | Space-separated paths to large projects for benchmarking |

## Example Output

```
============================================
  Vex Benchmark — Wed May  7 14:30:22 CEST 2026
  vex: vex 0.1.0
  ast-index: ast-index 3.31.0
============================================

=== Small (vex itself) ===
  vex:       16ms  269 symbols  43K
  ast-index: 48ms  531 symbols  0.49 MB
  speedup:   3.0x (indexing)

=== Search: Medium (avg 10 runs) ===
  "search"      vex: 3.7ms  ast: 8.8ms  2.4x
  "SymbolKind"  vex: 3.6ms  ast: 8.1ms  2.2x

=== Grep: Medium (avg 10 runs) ===
  vex search (symbol index) vs rg (raw text scan)
  "search"
    vex search: 3.7ms  (12 symbol results)
    rg -w:      5.2ms  (847 text matches)
    rg -t:      4.8ms  (filtered by lang)
    ratio:      vex 1.4x faster than rg

=== Pattern: Medium ===
  "fn $NAME($$$) -> Result" --lang rust  31ms  50 matches
  "pub struct $NAME" --lang rust  32ms  45 matches
```

## Grep Comparison Notes

The grep benchmark shows fundamentally different result sets:
- **vex search**: finds **symbol definitions** (functions, structs, classes) — precise, few results
- **rg -w**: finds **all text occurrences** — noisy, many results (includes comments, strings, usage sites)

vex is faster because it reads a pre-built FST index (~3.7ms constant). rg scans every file on disk.
On small projects the difference is small; on 100K+ LOC codebases vex pulls ahead significantly.

## Adding New Benchmarks

Edit `bench.sh` to add new projects or queries. The script auto-discovers:
- `../Claude-ast-index-search` as medium project
- Large projects can be added to the loop at the bottom

## Notes

- Always build release (`cargo build --release`) before benchmarking
- Close other heavy apps for consistent results
- First run after reboot may be slower (cold disk cache)
- ast-index comparison is optional — runs without it if not installed
