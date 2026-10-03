# Contributing to vex

Thanks for your interest in vex. This doc covers the local development loop: building, testing, and the quality gates CI enforces. For release mechanics, see [`docs/RELEASING.md`](docs/RELEASING.md); for architecture and honest coverage caveats, see [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) and [`docs/V9-FORMAT.md`](docs/V9-FORMAT.md) (index format v9 architecture).

## Prerequisites

- **Rust ≥ 1.88** (MSRV pinned in [`Cargo.toml`](Cargo.toml) `rust-version = "1.88"`). Bumped from 1.80 in v1.10.0 because the `fastembed → image / ort` dep chain requires Rust 1.88 (image 0.25.10, ort 2.0.0-rc.12, built 0.8.1). Earlier versions of these crates either don't exist or break vex's semantic-search feature, so pinning back is not an option. Install via [rustup](https://rustup.rs):

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup default stable
  ```

- **Git ≥ 2.30** — vex shells out to `git ls-files` / `git diff-files` for the Phase 14.7 blob cache and `--since` / `--changed-only` filters.
- **No other system deps.** Tree-sitter grammars vendor their C parsers; ONNX Runtime is downloaded by `fastembed` on first `--semantic` index. macOS / Linux / Windows are all first-class.

Optional but useful:

- [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) for coverage reports.
- [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) for the binary-format fuzz targets (nightly only).
- [`git-cliff`](https://github.com/orhun/git-cliff) to preview release-body generation locally.

## Clone & build

```bash
git clone https://github.com/tenatarika/vex.git
cd vex

# Debug build — fast compile, slow runtime; default for development.
cargo build

# Release build — slow compile, fast runtime; required for any perf measurement.
cargo build --release

# MCP server (separate crate in the workspace).
cargo build --release -p vex-mcp
```

Binaries land at `target/debug/vex` and `target/release/vex` (+ `target/release/vex-mcp`).

`target/` can grow to 30+ GiB across a long session with multiple agents. `cargo clean` periodically when disk pressure becomes a concern — incremental rebuild from clean takes ~1 minute.

## Run

```bash
./target/release/vex --version              # vex v<X>.<Y>.<Z>-<n>-g<sha>
./target/release/vex index --path /some/repo
./target/release/vex search "BlobCache"
./target/release/vex search "BlobCache" --format json | jq .
```

Versions reported by the binary come from `build.rs`'s `git describe --tags --always` — touch `build.rs` if you need to force the embedded version string to refresh after creating a new tag locally.

To install into your `PATH` during development:

```bash
cp target/release/vex ~/.local/bin/vex
# or, after a release tag is on GitHub:
vex self-update
```

## Quality gates (CI mirrors these)

Run before opening a PR. CI fails on the same checks.

```bash
# 1. Format
cargo fmt --check                 # autofix: cargo fmt

# 2. Lint — treat warnings as errors.
cargo clippy --workspace --all-targets -- -D warnings

# 3. Tests across the workspace.
cargo test --workspace            # ~4,000 tests across ~110 test files, < 2 minutes on a recent laptop

# 4. Benches compile (optional but cheap — catches benchmark drift).
cargo bench --no-run
```

If any check fails, fix the code, not the gate. Clippy / fmt drift in particular is non-negotiable — every commit must land them clean.

### Faster local runs with cargo-nextest (optional)

`cargo test` parallelises tests within each test binary but runs the
integration-test binaries themselves sequentially. With 20+ files in `tests/`,
[`cargo-nextest`](https://nexte.st/) is meaningfully faster — it runs every
test in its own process and parallelises across binaries. Use **cargo-nextest ≥ 0.9.145**,
which fixes spurious `LEAK` reports on macOS that plagued earlier versions.

```bash
cargo install cargo-nextest --locked

cargo nextest run --workspace           # 2-5x faster than `cargo test`
cargo nextest run --profile ci          # CI-shaped (more retries, louder output)
cargo test --doc --workspace            # Still required: nextest cannot run doctests
```

Profile defaults live in `.config/nextest.toml`. CI keeps using `cargo test`
to avoid an extra tool install per runner; nextest is purely a local-dev
convenience.

For language-specific grammar regression, the per-language `tests/<lang>_query_test.rs` files exercise each tree-sitter grammar's pinned query patterns. They catch ABI mismatches and AST node renames when a grammar crate is upgraded; never disable one to "make CI green" without rooting out the underlying ABI break.

### Lockfile / MSRV policy

`Cargo.lock` is checked in (since v1.10.0) so `cargo check --workspace --all-targets --locked` works on the CI runner's fresh clone. The MSRV gate is **Rust 1.88**, enforced by the `msrv` job in `.github/workflows/ci.yml`.

**Re-verify the MSRV gate** when running `cargo update` locally: `cargo +1.88 check --workspace --all-targets --locked`. The MSRV-aware resolver (default since Rust 1.84) will surface dep-tree floor breaches at `cargo update` time, but local stable-channel builds skip that gate — so a CI failure shows up only if you didn't reproduce locally. Prefer `cargo update --precise <version> <crate>` over bare `cargo update` so a one-off bump doesn't cascade.

## Adding a new language

Vex supports new languages — see the comprehensive guide in [`docs/SUPPORTED_LANGUAGES.md`](docs/SUPPORTED_LANGUAGES.md) § "How to add a new language". Briefly:

1. Add the grammar crate to `Cargo.toml`.
2. Add a `Language::<Name>` variant in `src/parse/language.rs`.
3. Write the symbol-extraction query `queries/<lang>.scm` (the only per-language `.scm` file) and register it in `src/parse/queries.rs`; optionally add a call-graph query arm in `src/callgraph/queries.rs`.
4. Register in `src/hierarchy/queries.rs` (inheritance) and `src/cli/cmd_pattern.rs` (pattern `--lang` spelling).
5. Add a scope binder in `src/parse/scope/` if your language has imports (enables `vex usages --strict`), and usually add it to `src/parse/language.rs::has_ast_ref_filter` (AST-aware ref filter for non-strict `usages`).
6. For indexed `vex pattern` prefiltering, add pattern-targetable node kinds in `src/pattern/skeleton/kinds.rs`.
7. Add per-language test in `tests/<lang>_query_test.rs`.
8. Update [`docs/SUPPORTED_LANGUAGES.md`](docs/SUPPORTED_LANGUAGES.md).

Before writing the `.scm` files, dump the grammar's `node-types.json` and parse a sample file with an AST printout — this saves multiple `Query::new` compile-fail iterations.

## Adding an MCP tool

Three files and one snapshot update:

1. **Dispatch**: `crates/vex-mcp/src/tools/mod.rs` → `build_command(...)` translating MCP args into CLI argv.
2. **Schema**: `crates/vex-mcp/src/descriptors.rs` → add a tool entry in `tool_descriptors()` JSON.
3. **Helpers**: `crates/vex-mcp/src/args.rs` → optional shared parameter helpers (reduce boilerplate).
4. **Tests** — add inline `#[test]` cases mirroring the existing `<tool>_<flag>_pushes_flag` / `<tool>_<flag>_default_omits_flag` pattern; the `tool_descriptors_snapshot` regression guard locks the schema.
5. **Regenerate the snapshot**: `INSTA_UPDATE=always cargo test -p vex-mcp tool_descriptors_snapshot`.

The shared helpers (`push_scope`, `push_metadata`, `push_diff_scope`, `push_show_truncate`, `push_kind`, `push_no_stale_check`, `push_auto_update`) handle the standard flag families — reuse them rather than inlining.

## Fuzzing the binary format

Multiple fuzz targets cover all `unsafe` code paths in the reader and format parsers. Requires nightly Rust. See `fuzz/Cargo.toml [[bin]]` for the full count:

```bash
cargo install cargo-fuzz
bash fuzz/generate_seeds.sh                                              # seed corpus from local vex cache
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_index_reader -- -max_total_time=120
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_refs_fst    -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_symbol_fst  -- -max_total_time=60
```

Any new `unsafe` block in the reader path SHOULD be exercised by an existing or new fuzz target before merge. See the README's Fuzzing section for the full fuzz target list and historical defects, and [`SECURITY.md`](SECURITY.md) § "In-Scope Issues" for the fuzzed security surfaces.

## Commit & PR conventions

- **Conventional commits** with prefixes from the set `feat / fix / perf / refactor / docs / test / chore / ci / build`. `git-cliff` reads these to build release bodies — see [`cliff.toml`](cliff.toml).
- **One topic per commit.** When in doubt, split — a focused diff is easier to review and to revert.
- **Test coverage** — every fix lands with at least one regression test that fails without the fix. Refactors land with the existing tests untouched (or with a test added if the refactor exposed an uncovered path).
- **No co-author lines** in commits. The repo convention.

## Release

Cutting a release is documented in [`docs/RELEASING.md`](docs/RELEASING.md). TL;DR: edit `CHANGELOG.md`, bump `Cargo.toml`'s `version`, tag `vX.Y.Z`, push the tag. CI signs the prebuilt binaries with the zipsign keypair and updates the Homebrew tap automatically.

## Where to start

- Browse open issues on GitHub.
- Read [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) — the items there are deliberate gaps that may be addressable. Anything tagged "roadmap" is fair game.
- Walk through [`.claude/Task/`](.claude/Task) for in-flight feature sketches if you want context on what's currently being shaped.
- Try indexing a few real projects in different languages with `vex index --semantic` and report any panics, parse failures, or surprising results — robustness reports are always welcome.

## Questions

File an issue or open a discussion. For security-sensitive findings, prefer a private channel (see the security policy in the repo if present, otherwise contact the maintainers via the email on their profile).
