# Changelog

All notable changes to vex are documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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

[Unreleased]: https://github.com/tenatarika/vex/compare/v1.4.1...HEAD
[1.4.1]: https://github.com/tenatarika/vex/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/tenatarika/vex/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/tenatarika/vex/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/tenatarika/vex/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/tenatarika/vex/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/tenatarika/vex/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/tenatarika/vex/compare/v0.2.0...v1.0.0
[0.2.0]: https://github.com/tenatarika/vex/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tenatarika/vex/releases/tag/v0.1.0
