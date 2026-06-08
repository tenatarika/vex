# Security Policy

## Supported Versions

Only the latest minor release receives security fixes. Older minor versions
are not patched — please upgrade to the latest release before reporting.

| Version | Supported |
|---------|-----------|
| 1.15.x  | yes       |
| 1.14.x  | yes       |
| < 1.14  | no — upgrade first |

`vex self-update` will fetch the latest GitHub release on Linux, macOS,
and Windows.

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.** Use
the private channel instead:

**GitHub Security Advisories**:
<https://github.com/tenatarika/vex/security/advisories/new>

This creates a private advisory thread visible only to maintainers.
Please include enough detail to reproduce: vex version, OS, minimal
repro, and the impact you observed.

You can expect:

- Acknowledgement within **72 hours**.
- A first triage assessment (severity + likely fix window) within **7 days**.
- A coordinated-disclosure window of **up to 90 days** before public
  disclosure. We will request an extension only if the fix is non-trivial
  and we keep you informed about progress.
- Credit in the release notes and the CHANGELOG, unless you prefer to
  remain anonymous.

## In-Scope Issues

These are the surfaces we treat as security-relevant:

- **Binary format reader** (`src/store/`): out-of-bounds reads,
  misaligned pointer dereferences, integer overflow on offsets,
  type-confusion across sections. The reader is exercised by three
  libFuzzer targets (`fuzz_index_reader`, `fuzz_refs_fst`,
  `fuzz_symbol_fst`); past fuzz-found bugs are listed in the README.
- **Sidecar parsers**: every file the cache directory loads alongside
  `index.vex` is fuzzed as untrusted input. Covered surfaces:
  - `index.bloom` (`src/search/bloom.rs`, `fuzz_bloom_load`, v1.12.0)
    — two real defects caught and fixed (`hash % 0` panic on degenerate
    `n_bits/k_num`, and DoS-via-huge-`k_num` looping for minutes).
  - `<onnx>.sha256.marker` (`src/embed/integrity.rs`, `fuzz_marker_load`,
    v1.13.0) — P2 fast-path marker for the ONNX integrity check.
  - `embed_cache_<id>.bin` (`src/embed/cache.rs`) — content-addressed
    embedding cache from v1.13 E2b. Validated at load (magic + dim
    bound) but not yet fuzzed; defence-in-depth via existing roundtrip
    unit tests.
  - `index.hashes` (`src/search/hash_index.rs`, `fuzz_hash_index_load`,
    v1.14.1) — `VEXH` sidecar pairing HNSW hash keys with sym_idx
    positions; `MAX_COUNT` guard on both save + load paths after a
    parallel-reviewer audit. 3M-iter focused + 5.8M-iter system-wide
    runs (2026-06-05) clean.
  - `index.hnsw` incremental update (`build_hnsw_incremental_at`,
    `fuzz_incremental_hnsw`, v1.15.0) — drives the B1.2 update path
    with adversarial `new_hashes` slices (duplicate-heavy batches,
    tombstone-threshold boundary inputs, usearch `add`/`remove`
    corner cases, sidecar-rewrite error paths). Pre-v1.15.1 the
    `usearch::Index::add` collision was a hard abort; v1.15.1
    dedup-and-skip is exercised here in addition to integration
    tests.

  See README §Fuzz Testing for the full target list and historical
  defects.
- **Mmap / unsafe paths**: any code path in `src/store/reader.rs` or
  the FST readers that can be tripped by a hand-crafted `.vex` file.
- **User-facing parsers**: the pattern grammar behind `vex pattern '...'`
  (`parse_composite_pattern`) and the JSON manifest at `manifest.json`
  (`Manifest::load`) — both covered by libFuzzer targets
  (`fuzz_pattern_parser`, `fuzz_manifest_load`) and expected to surface
  `Err` rather than panic on adversarial input. The BM25 hot-path
  tokenizer (`tokenize_document`) is also fuzzed (`fuzz_tokenize_document`,
  v1.13.0) since it walks attacker-supplied UTF-8 byte-by-byte during
  every `vex index`.
- **MCP server (`crates/vex-mcp`)**: JSON-RPC parser, stdio handling,
  path-traversal in tool arguments (`VEX_ROOT` containment), or
  resource exhaustion via malformed `tools/call` payloads.
- **`vex self-update`**: signature verification, archive extraction
  (path traversal in tar/zip), or downgrade attacks via the release
  manifest.
- **Tree-sitter grammar / pattern engine**: stack overflow or quadratic
  blowup on adversarial source files, exploitable via `vex index` of
  attacker-supplied code.

## Out of Scope

- Issues that require an attacker to already have write access to your
  source tree or shell. Indexing untrusted code is a supported use case,
  but executing untrusted code is the language toolchain's concern.
- Bugs in upstream tree-sitter grammar crates — please report those
  upstream; we'll bump the grammar version once a fix lands.
- Denial-of-service via legitimately large repositories. `vex index` is
  bounded by your filesystem and `--exclude` is the supported mitigation.
- Findings against pre-release builds, forks, or unreleased branches.

## Hardening Notes

If you're embedding vex into a multi-tenant environment:

- Treat `.vex` index files **and every sidecar in the cache directory**
  (`index.hnsw`, `index.bloom`, `index.hashes`, `index.git_history`,
  `manifest.json`, `embed_cache_<id>.bin`, `<onnx>.sha256.marker`) as
  **untrusted input** even when you wrote them yourself — they're
  consumed via mmap or parsed without prior validation. The fuzz
  harness covers each binary-input parser, but defence in depth
  helps. The most recent v1.15.1 release-gate audit (2026-06-08) ran
  ~853k executions across the four highest-signal targets
  (`fuzz_incremental_hnsw`, `fuzz_hash_index_load`, `fuzz_bloom_load`,
  `fuzz_index_reader`) with zero crashes; the v1.14.1 system-wide
  audit (2026-06-05) ran ~5.8M iterations across all then-9 targets
  with zero crashes — historical baseline retained for comparison.
- The MCP server reads `VEX_ROOT` from the environment and rejects paths
  that escape it. Don't pass user-controlled values into `VEX_ROOT`.
- `vex self-update` verifies release archives via zipsign signatures —
  do not pipe arbitrary URLs into the updater.
