//! RED integration tests for Phase 13.10 — `vex tests-for <Symbol>`.
//!
//! The `tests-for` subcommand does NOT exist on `main` at the time
//! these tests are written. Every test will fail with clap exit code 2
//! ("unrecognised subcommand") until the GREEN phase wires the command.
//!
//! Design contract being pinned:
//!   - `vex tests-for <TARGET> --format json` emits the standard
//!     envelope (`results[]`) where each entry has:
//!     `name`, `path`, `line`, `depth`, `framework`
//!   - Default filter: only symbols whose path matches the built-in
//!     test-glob pattern set AND whose name satisfies Signal-B
//!     (starts with `test_`, ends with `_test`, starts with `Test`, etc.)
//!   - `--test-pattern <glob>` REPLACES the default set (not appends).
//!   - `--include-fixtures` weakens Signal-B so fixture helpers in
//!     test directories are included.
//!   - `--max-hops 0` returns an empty set (exit 1).
//!   - Empty result → exit 1 (mirrors `vex reachable`).
//!
//! Framework labels are derived from path pattern:
//!   `rust-integration` for `tests/**/*.rs`,
//!   `pytest`           for `test_*.py` / `tests/**/*.py`,
//!   `jest`             for `*.test.ts` / `*.test.tsx` / `*.test.js` / `*.test.jsx`

use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

mod common;
use common::assert_ran;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// Write a `.vex.toml` that keeps the cache inside the tempdir so tests
/// don't touch the user's real cache directory.
fn init_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
}

/// Parse the JSON envelope and return the `results` array.
/// Panics with a descriptive message when parsing fails.
fn results_from(stdout: &str) -> Vec<Value> {
    let envelope: Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("non-JSON stdout: {e}\n---\n{stdout}"));
    envelope
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("envelope missing `results` array; got: {envelope}"))
}

// ---------------------------------------------------------------------------
// Test 1: Rust integration-test chain → framework label "rust-integration"
// ---------------------------------------------------------------------------

/// A two-hop Rust chain: `tests/integration.rs::test_target_works` →
/// `src/foo.rs::caller_of_target` → `src/lib.rs::target`.
/// `vex tests-for target` must surface `test_target_works` and label it
/// `rust-integration` because it lives in `tests/`.
#[test]
fn rust_chain_surfaces_integration_test_with_framework_label() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_project(dir);

    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();

    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"testpkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn target() -> u8 { 0 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("foo.rs"),
        "pub fn caller_of_target() -> u8 { crate::target() }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tests").join("integration.rs"),
        "#[test]\nfn test_target_works() { testpkg::caller_of_target(); }\n",
    )
    .unwrap();

    vex_in(dir).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(dir).args(["tests-for", "target", "--format", "json"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let rows = results_from(&stdout);

    let hit = rows
        .iter()
        .find(|r| r["name"].as_str() == Some("test_target_works"))
        .unwrap_or_else(|| panic!("expected test_target_works in results; got: {stdout}"));

    assert!(
        hit["path"]
            .as_str()
            .unwrap_or("")
            .ends_with("tests/integration.rs"),
        "path should end with tests/integration.rs; got: {}",
        hit["path"]
    );
    assert_eq!(
        hit["framework"].as_str(),
        Some("rust-integration"),
        "wrong framework label; got: {hit}"
    );
    let depth = hit["depth"].as_u64().unwrap_or(0);
    assert!(
        depth >= 1,
        "depth should be at least 1 (2-hop chain); got: {depth}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Python chain → framework label "pytest"
// ---------------------------------------------------------------------------

/// Two-hop Python chain: `tests/test_mod.py::test_target` →
/// `src/wrap.py::wrap` → `src/mod.py::target`.
/// `vex tests-for target` must surface `test_target` labeled `pytest`.
#[test]
fn pytest_chain_surfaces_test_with_pytest_framework() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_project(dir);

    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();

    std::fs::write(dir.join("src").join("mod.py"), "def target():\n    pass\n").unwrap();
    std::fs::write(
        dir.join("src").join("wrap.py"),
        "from src.mod import target\ndef wrap():\n    target()\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tests").join("test_mod.py"),
        "from src.wrap import wrap\ndef test_target():\n    wrap()\n",
    )
    .unwrap();

    vex_in(dir).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(dir).args(["tests-for", "target", "--format", "json"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let rows = results_from(&stdout);

    let hit = rows
        .iter()
        .find(|r| r["name"].as_str() == Some("test_target"))
        .unwrap_or_else(|| panic!("expected test_target in results; got: {stdout}"));

    assert_eq!(
        hit["framework"].as_str(),
        Some("pytest"),
        "wrong framework label; got: {hit}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: TypeScript chain → framework label "jest"
// ---------------------------------------------------------------------------

/// Two-hop TypeScript chain: `src/util.test.ts::test_target_returns_zero`
/// → `src/wrap.ts::wrap` → `src/util.ts::target`.
/// Top-level function (not `it()` callback) to ensure CallEdges resolve.
/// Framework bucket for `.test.ts` is "jest".
#[test]
fn typescript_chain_surfaces_dot_test_ts_with_jest_framework() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_project(dir);

    std::fs::create_dir_all(dir.join("src")).unwrap();

    std::fs::write(
        dir.join("tsconfig.json"),
        "{\"compilerOptions\":{\"target\":\"ES2020\",\"module\":\"commonjs\"}}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("util.ts"),
        "export function target(): number { return 0; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("wrap.ts"),
        "import { target } from './util';\nexport function wrap() { return target(); }\n",
    )
    .unwrap();
    // Top-level function rather than an `it()` callback so tree-sitter
    // extracts a named function symbol and CallEdges can resolve across files.
    std::fs::write(
        dir.join("src").join("util.test.ts"),
        "import { wrap } from './wrap';\nfunction test_target_returns_zero() { return wrap(); }\n",
    )
    .unwrap();

    vex_in(dir).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(dir).args(["tests-for", "target", "--format", "json"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let rows = results_from(&stdout);

    let hit = rows
        .iter()
        .find(|r| r["name"].as_str() == Some("test_target_returns_zero"))
        .unwrap_or_else(|| panic!("expected test_target_returns_zero in results; got: {stdout}"));

    assert_eq!(
        hit["framework"].as_str(),
        Some("jest"),
        "wrong framework label; got: {hit}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Unknown target → exit 1 with empty results
// ---------------------------------------------------------------------------

/// Querying a symbol that does not exist in any call edge must produce an
/// empty result set and exit 1 (matches the S8.2 contract from `reachable`).
#[test]
fn unknown_target_exits_one_with_empty_results() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_project(dir);

    std::fs::write(
        dir.join("lib.rs"),
        "fn unrelated() {}\nfn also_unrelated() { unrelated(); }\n",
    )
    .unwrap();

    vex_in(dir).args(["index"]).assert().success();

    let output = vex_in(dir)
        .args(["tests-for", "nonexistent_symbol_xyz", "--format", "json"])
        .output()
        .expect("vex spawned");

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 for empty result set"
    );

    // `--format json` is contractually obliged to emit the standard envelope
    // on every code path, including the empty / not-found one. Reject empty
    // stdout outright; then verify `results == []`.
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        !stdout.trim().is_empty(),
        "vex tests-for --format json must always emit an envelope, even on \
         empty result; got empty stdout"
    );
    let rows = results_from(&stdout);
    assert!(
        rows.is_empty(),
        "expected empty results for unknown symbol; got: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Fixture helpers excluded without --include-fixtures
// ---------------------------------------------------------------------------

/// `tests/fixtures/data.rs` contains `fixture_helper` (name does NOT
/// match Signal-B: no `test_` prefix, no `_test` suffix, no `Test` prefix).
/// `tests/integration.rs` contains `test_target` which calls both
/// `fixture_helper` and `target`.
///
/// Without `--include-fixtures`:  only `test_target` appears.
/// With    `--include-fixtures`:  `fixture_helper` also appears because the
///         Signal-B name filter is weakened for functions in test paths.
#[test]
fn fixture_excluded_without_include_fixtures_flag() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_project(dir);

    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests").join("fixtures")).unwrap();

    std::fs::write(dir.join("src").join("lib.rs"), "pub fn target() {}\n").unwrap();
    std::fs::write(
        dir.join("tests").join("fixtures").join("data.rs"),
        "pub fn fixture_helper() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tests").join("integration.rs"),
        "fn test_target() { fixture_helper(); target(); }\n",
    )
    .unwrap();

    vex_in(dir).args(["index"]).assert().success();

    // Without --include-fixtures: fixture_helper must be absent.
    let assert_no_fixture =
        assert_ran(vex_in(dir).args(["tests-for", "target", "--format", "json"]));
    let stdout_no = String::from_utf8_lossy(&assert_no_fixture.get_output().stdout).into_owned();
    let rows_no = results_from(&stdout_no);
    let names_no: Vec<&str> = rows_no.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names_no.contains(&"test_target"),
        "test_target must appear without --include-fixtures; got: {names_no:?}"
    );
    assert!(
        !names_no.contains(&"fixture_helper"),
        "fixture_helper must NOT appear without --include-fixtures; got: {names_no:?}"
    );

    // With --include-fixtures: fixture_helper must now appear.
    let assert_with_fixture = assert_ran(vex_in(dir).args([
        "tests-for",
        "target",
        "--include-fixtures",
        "--format",
        "json",
    ]));
    let stdout_with =
        String::from_utf8_lossy(&assert_with_fixture.get_output().stdout).into_owned();
    let rows_with = results_from(&stdout_with);
    let names_with: Vec<&str> = rows_with
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert!(
        names_with.contains(&"fixture_helper"),
        "fixture_helper must appear with --include-fixtures; got: {names_with:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 6: --test-pattern replaces defaults (not appends)
// ---------------------------------------------------------------------------

/// A project with two test locations:
///   `tests/integration.rs::test_target`  — matches the default `**/tests/**`
///   `spec/foo_spec.rs::spec_test`         — only matches `**/spec/**`
///
/// `--test-pattern '**/spec/**'` REPLACES the default set, so:
///   - `spec_test` IS included (its path matches `**/spec/**`)
///   - `test_target` is NOT included (default `**/tests/**` was replaced)
#[test]
fn test_pattern_override_replaces_defaults_not_appends() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_project(dir);

    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::create_dir_all(dir.join("spec")).unwrap();

    std::fs::write(dir.join("src").join("lib.rs"), "pub fn target() {}\n").unwrap();
    std::fs::write(
        dir.join("tests").join("integration.rs"),
        "fn test_target() { target(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("spec").join("foo_spec.rs"),
        "fn spec_test() { target(); }\n",
    )
    .unwrap();

    vex_in(dir).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(dir).args([
        "tests-for",
        "target",
        "--test-pattern",
        "**/spec/**",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let rows = results_from(&stdout);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();

    assert!(
        names.contains(&"spec_test"),
        "spec_test must appear when --test-pattern '**/spec/**' is given; got: {names:?}"
    );
    assert!(
        !names.contains(&"test_target"),
        "test_target must NOT appear — default pattern was replaced, not appended; got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 7: --max-hops 0 returns empty results
// ---------------------------------------------------------------------------

/// With `--max-hops 0` the BFS does zero traversal steps, so even a
/// direct caller is invisible. The result is an empty set → exit 1.
#[test]
fn max_hops_zero_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_project(dir);

    std::fs::create_dir_all(dir.join("tests")).unwrap();

    std::fs::write(dir.join("lib.rs"), "pub fn target() {}\n").unwrap();
    std::fs::write(
        dir.join("tests").join("test_it.rs"),
        "fn test_target() { target(); }\n",
    )
    .unwrap();

    vex_in(dir).args(["index"]).assert().success();

    let output = vex_in(dir)
        .args(["tests-for", "target", "--max-hops", "0", "--format", "json"])
        .output()
        .expect("vex spawned");

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 when --max-hops 0 yields no results"
    );

    // Envelope contract holds even when the result is empty.
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        !stdout.trim().is_empty(),
        "vex tests-for --format json must always emit an envelope on empty \
         result; got empty stdout"
    );
    let rows = results_from(&stdout);
    assert!(
        rows.is_empty(),
        "expected empty results with --max-hops 0; got: {rows:?}"
    );
}
