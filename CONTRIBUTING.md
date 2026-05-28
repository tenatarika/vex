# Contributing to vex

Thanks for your interest in vex. This doc covers the local development loop: building, testing, and the quality gates CI enforces. For release mechanics, see [`docs/RELEASING.md`](docs/RELEASING.md); for architecture, for honest coverage caveats, [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md).

## Prerequisites

- **Rust ≥ 1.85** (MSRV pinned in [`Cargo.toml`](Cargo.toml) `rust-version = "1.85"`). Bumped from 1.80 in v1.10.0 because the `fastembed → image → moxcms → pxfm` dep chain moved to `edition2024` (stabilized in Cargo 1.85) and pre-1.85 versions of those crates are no longer maintained. Install via [rustup](https://rustup.rs):

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
cargo test --workspace            # ~67 binaries, ~1990 tests, < 2 minutes on a recent laptop

# 4. Benches compile (optional but cheap — catches benchmark drift).
cargo bench --no-run
```

If any check fails, fix the code, not the gate. Clippy / fmt drift in particular is non-negotiable — every commit must land them clean.

For language-specific grammar regression, the per-language `tests/<lang>_query_test.rs` files exercise each tree-sitter grammar's pinned query patterns. They catch ABI mismatches and AST node renames when a grammar crate is upgraded; never disable one to "make CI green" without rooting out the underlying ABI break.

### Lockfile / MSRV policy

`Cargo.lock` is checked in (since v1.10.0) so `cargo check --workspace --all-targets --locked` works on the CI runner's fresh clone. The MSRV gate is **Rust 1.85**, enforced by the `msrv` job in `.github/workflows/ci.yml`.

**Re-verify the MSRV gate** when running `cargo update` locally: `cargo +1.85 check --workspace --all-targets --locked`. If a transitive dep starts requiring Rust > 1.85 (e.g. by declaring `edition2024+future` once that lands), the check fires immediately rather than at the next CI run. Prefer `cargo update --precise <version> <crate>` over bare `cargo update` so a one-off bump doesn't cascade.

## Adding a new language

Vex supports new languages with three files and a registration:

1. **Vendor the tree-sitter grammar** as a Cargo dependency in `[dependencies]` (`tree-sitter-<lang> = "X.Y"`).
2. **Write the AST queries**: `queries/<lang>.scm` (symbol extraction), optionally `queries/<lang>-refs.scm` and `queries/<lang>-callgraph.scm`.
3. **Register the language** in `src/parse/language.rs::Language` enum + `from_extension` mapping.
4. **Add per-language tests** in `tests/<lang>_query_test.rs` — copy the shape of `tests/rust_query_test.rs`. Cover at minimum: simple symbol extraction, refs, callgraph for the function-shaped node kinds.
5. **Update [`docs/SUPPORTED_LANGUAGES.md`](docs/SUPPORTED_LANGUAGES.md)** with the tier (T1 / T2 / T3) and any coverage caveats.

Before writing the `.scm` files, dump the grammar's `node-types.json` and parse a sample file with an AST printout — this saves multiple `Query::new` compile-fail iterations.

## Adding an MCP tool

Two files, one snapshot:

1. **`crates/vex-mcp/src/main.rs`** — add a dispatch arm in `build_command(...)` translating MCP args into CLI argv, and add a schema entry in `tool_descriptors()`.
2. **Tests** — add inline `#[test]` cases mirroring the existing `<tool>_<flag>_pushes_flag` / `<tool>_<flag>_default_omits_flag` pattern; the `tool_descriptors_snapshot` regression guard locks the schema.
3. **Regenerate the snapshot**: `INSTA_UPDATE=always cargo test -p vex-mcp tool_descriptors_snapshot`.

The shared helpers (`push_scope`, `push_metadata`, `push_diff_scope`, `push_show_truncate`, `push_kind`, `push_no_stale_check`, `push_auto_update`) handle the standard flag families — reuse them rather than inlining.

## Fuzzing the binary format

Three fuzz targets cover all `unsafe` code paths in the reader. Requires nightly Rust:

```bash
cargo install cargo-fuzz
bash fuzz/generate_seeds.sh                                              # seed corpus from local vex cache
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_index_reader -- -max_total_time=120
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_refs_fst    -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly cargo fuzz run fuzz_symbol_fst  -- -max_total_time=60
```

Any new `unsafe` block in the reader path SHOULD be exercised by an existing or new fuzz target before merge.

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
