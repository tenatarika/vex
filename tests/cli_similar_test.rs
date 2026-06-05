//! T3 — CLI integration coverage for `vex similar`.
//!
//! Before this suite, `cli/cmd_similar.rs` was at 15.70% line coverage
//! per the T1 baseline (the lowest of any cli/* handler with non-trivial
//! logic). The only prior tests touched the no-vectors bail path
//! vacuously (envelope-only) and a `VEX_TEST_SEMANTIC=1`-gated `--why`
//! trace — neither exercised the filter / scope / `--explain` / `--why`
//! / `signal_no_results` / `print_similar` branches.
//!
//! We sidestep ONNX semantic indexing (60+ sec per test) by dropping a
//! pre-built v6 vector-bearing index at the `local_cache = true` cache
//! path and running `vex similar --no-stale-check` against it. The
//! brute-force fallback in `find_similar` kicks in when the HNSW file
//! is absent, which is exactly what fixture tests want.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;
use vex::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
use vex::store::format::VECTOR_DIM;
use vex::store::writer::write_index_full;

mod common;
use common::assert_ran;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    // Clear any inherited VEX_CACHE_DIR so `.vex.toml`'s `local_cache = true`
    // wins and the index file lands at a predictable `<dir>/.vex_cache/index.vex`.
    cmd.env_remove("VEX_CACHE_DIR");
    cmd
}

fn ones() -> Vec<f32> {
    vec![1.0_f32; VECTOR_DIM as usize]
}

/// Alternating ±1 vector — genuinely orthogonal to `ones()`:
/// `dot([1,1,…], [1,-1,1,-1,…]) == 0`, and both norms are well-defined.
/// Prefer this over an all-zero vector for "low similarity" fixtures:
/// `cosine_similarity` short-circuits to 0.0 on a zero-norm input, so a
/// future change to that guard (e.g. returning NaN) would silently make
/// zero-based tests pass for the wrong reason.
fn orthogonal_to_ones() -> Vec<f32> {
    (0..VECTOR_DIM as usize)
        .map(|i| if i % 2 == 0 { 1.0_f32 } else { -1.0_f32 })
        .collect()
}

fn near_ones() -> Vec<f32> {
    let mut v = ones();
    v[0] = 0.999;
    v
}

fn mk_sym(name: &str, line: usize) -> ParsedSymbol {
    ParsedSymbol {
        name: name.to_string(),
        kind: SymbolKind::Function,
        line,
        signature: Some(format!("fn {name}()")),
        doc: None,
        body_tokens: None,
    }
}

struct Entry {
    name: &'static str,
    path: &'static str,
    line: usize,
    vector: Vec<f32>,
}

/// Drop a v6 index with vectors into `<dir>/.vex_cache/index.vex` so
/// `vex similar` opens it without going through `vex index --semantic`.
/// Symbol order in `entries` is preserved, and vector index N is bound
/// to the Nth symbol — the writer assigns `vector_index = symbol_idx`.
///
/// **Precondition**: entries sharing the same `path` must be grouped
/// (contiguous) in the slice. The writer iterates `parsed` files in
/// order and emits `symbol_idx` linearly, so an interleaved layout like
/// `[a, b, a]` would bind the second `a`-entry's vector to whatever
/// symbol falls at writer-index 2 — likely the first `b`-symbol.
/// The helper does not enforce this; rely on the call site.
fn prebuild_index(dir: &Path, entries: &[Entry]) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    let cache_root = dir.join(".vex_cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let mut parsed: Vec<ParsedFile> = Vec::new();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut path_idx: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for e in entries {
        let i = *path_idx.entry(e.path).or_insert_with(|| {
            parsed.push(ParsedFile {
                path: e.path.to_string(),
                symbols: vec![],
                refs: vec![],
                call_edges: vec![],
                bound_refs: vec![],
                skeletons: Vec::new(),
                cpp_includes: Vec::new(),
            });
            parsed.len() - 1
        });
        parsed[i].symbols.push(mk_sym(e.name, e.line));
        vectors.push(e.vector.clone());
    }

    write_index_full(&parsed, &vectors, 384, &cache_root.join("index.vex"))
        .expect("write_index_full");
}

/// Extract the JSON envelope from stdout.
fn parse_envelope(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}):\n---\n{stdout}\n---"))
}

/// Names of every result in the envelope, in order.
fn names_in(envelope: &serde_json::Value) -> Vec<String> {
    envelope["results"]
        .as_array()
        .expect("envelope must have results array")
        .iter()
        .map(|r| r["name"].as_str().expect("name").to_string())
        .collect()
}

/// Extract the `VEX_WHY: { ... }` trace line from stderr.
fn parse_why_trace(stderr: &str) -> serde_json::Value {
    const PREFIX: &str = "VEX_WHY:";
    let rest = stderr
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(PREFIX))
        .unwrap_or_else(|| panic!("VEX_WHY trace missing from stderr:\n{stderr}"));
    serde_json::from_str(rest.trim())
        .unwrap_or_else(|e| panic!("VEX_WHY trace did not parse as JSON ({e}):\n{stderr}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn similar_bails_when_index_has_no_vectors() {
    // The handler's `if !reader.has_vectors() { bail!(...) }` branch —
    // covered vacuously before, asserted explicitly here.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    let cache_root = tmp.path().join(".vex_cache");
    std::fs::create_dir_all(&cache_root).unwrap();
    let parsed = vec![ParsedFile {
        path: "a.rs".to_string(),
        symbols: vec![mk_sym("Foo", 1)],
        refs: vec![],
        call_edges: vec![],
        bound_refs: vec![],
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
    }];
    write_index_full(&parsed, &[], 384, &cache_root.join("index.vex")).unwrap();

    let assert = vex_in(tmp.path())
        .args(["similar", "Foo", "--no-stale-check"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("semantic"),
        "error should mention --semantic, got stderr: {stderr}"
    );
}

#[test]
fn similar_signals_no_results_when_threshold_drops_all() {
    // Foo's vector is orthogonal to Bar's (alternating ±1) → cosine 0
    // → threshold 0.9 post-filters everything → `signal_no_results()`
    // → exit 1 per S8.2. Using `orthogonal_to_ones()` (not zeros) so
    // the test doesn't rely on `cosine_similarity`'s zero-norm guard.
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "a.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "Bar",
                path: "a.rs",
                line: 10,
                vector: orthogonal_to_ones(),
            },
        ],
    );
    let assert = vex_in(tmp.path())
        .args([
            "similar",
            "Foo",
            "--threshold",
            "0.9",
            "--no-stale-check",
            "--format",
            "text",
        ])
        .assert();
    let code = assert.get_output().status.code();
    assert_eq!(code, Some(1), "empty post-filter must exit 1 per S8.2");
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("No similar symbols found"),
        "text mode should emit empty-state banner, got: {stdout}"
    );
}

#[test]
fn similar_text_format_renders_header_and_row() {
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "a.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "Bar",
                path: "a.rs",
                line: 10,
                vector: near_ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "Foo",
        "--threshold",
        "0.0",
        "--no-stale-check",
        "--format",
        "text",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Similar to \"Foo\""),
        "text header missing: {stdout}"
    );
    assert!(stdout.contains("Bar"), "match row missing: {stdout}");
}

#[test]
fn similar_filter_path_drops_other_paths() {
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "src/a.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "Bar",
                path: "src/a.rs",
                line: 10,
                vector: ones(),
            },
            Entry {
                name: "Baz",
                path: "other/b.rs",
                line: 1,
                vector: ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "Foo",
        "--threshold",
        "0.0",
        "--filter",
        "other/",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env = parse_envelope(&stdout);
    let names = names_in(&env);
    assert!(
        names.contains(&"Baz".to_string()),
        "Baz under other/ should pass --filter other/: {names:?}"
    );
    assert!(
        !names.contains(&"Bar".to_string()),
        "Bar under src/ should be filtered out: {names:?}"
    );
}

#[test]
fn similar_scope_include_keeps_only_matching_paths() {
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "src/a.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "Bar",
                path: "src/a.rs",
                line: 10,
                vector: ones(),
            },
            Entry {
                name: "Baz",
                path: "tests/b.rs",
                line: 1,
                vector: ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "Foo",
        "--threshold",
        "0.0",
        "--include",
        "tests/**",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env = parse_envelope(&stdout);
    let names = names_in(&env);
    assert_eq!(
        names,
        vec!["Baz"],
        "only tests/** path should remain after --include"
    );
}

#[test]
fn similar_scope_exclude_wins_over_include() {
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "src/a.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "Bar",
                path: "src/a.rs",
                line: 10,
                vector: ones(),
            },
            Entry {
                name: "Baz",
                path: "src/gen/b.rs",
                line: 1,
                vector: ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "Foo",
        "--threshold",
        "0.0",
        "--include",
        "src/**",
        "--exclude",
        "src/gen/**",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env = parse_envelope(&stdout);
    let names = names_in(&env);
    assert_eq!(
        names,
        vec!["Bar"],
        "exclude must override include for src/gen/**"
    );
}

#[test]
fn similar_limit_truncates_results() {
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "a.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "A",
                path: "a.rs",
                line: 10,
                vector: ones(),
            },
            Entry {
                name: "B",
                path: "a.rs",
                line: 20,
                vector: ones(),
            },
            Entry {
                name: "C",
                path: "a.rs",
                line: 30,
                vector: ones(),
            },
            Entry {
                name: "D",
                path: "a.rs",
                line: 40,
                vector: ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "Foo",
        "--threshold",
        "0.0",
        "--limit",
        "2",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env = parse_envelope(&stdout);
    let n = env["results"].as_array().expect("results array").len();
    // 5 total - 1 (self, Foo) = 4 candidates, all with identical vectors
    // and similarity == 1.0. `--limit 2` must saturate to exactly 2 —
    // a looser `<= 2` would silently pass if a regression dropped to 0/1.
    assert_eq!(n, 2, "--limit 2 must saturate to exactly 2 results");
}

#[test]
fn similar_json_envelope_includes_protocol_version_and_results() {
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "a.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "Bar",
                path: "a.rs",
                line: 10,
                vector: near_ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "Foo",
        "--threshold",
        "0.0",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env = parse_envelope(&stdout);
    assert_eq!(
        env.get("protocol_version").and_then(|v| v.as_str()),
        Some("v1"),
        "envelope must carry protocol_version=v1: {env}"
    );
    let bar = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Bar")
        .expect("Bar should be in results");
    let sim = bar["similarity"].as_f64().unwrap();
    assert!(
        sim > 0.99,
        "near-identical vectors should yield similarity > 0.99, got {sim}"
    );
}

#[test]
fn similar_why_emits_trace_with_seed_resolved() {
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "a.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "Bar",
                path: "a.rs",
                line: 10,
                vector: near_ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "Foo",
        "--threshold",
        "0.0",
        "--no-stale-check",
        "--why",
        "--format",
        "json",
    ]));
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace = parse_why_trace(&stderr);
    assert_eq!(
        trace["seed_resolved"].as_bool(),
        Some(true),
        "seed `Foo` should resolve, got trace: {trace}"
    );
    assert!(trace["threshold_applied"].is_number(), "trace: {trace}");
    assert!(
        trace["candidates_before_filter"].as_u64().is_some(),
        "trace: {trace}"
    );
    assert!(
        trace["candidates_after_filter"].as_u64().is_some(),
        "trace: {trace}"
    );
    // `filter_applied` carries the FilterSnapshot — pin its shape so a
    // serde rename / structural change is caught here, not at MCP time.
    assert!(
        trace["filter_applied"].is_object(),
        "trace.filter_applied must be an object: {trace}"
    );
}

#[test]
fn similar_explain_emits_jaccard_and_diff_for_similar_match() {
    // Bodies share most identifiers but differ in one operator — mirrors
    // `cli_explain_test.rs::write_duplicate_project` so we exercise the
    // `--explain` branch in `cmd_similar` specifically (the existing
    // explain suite only covers `vex duplicates`).
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("alpha.rs"),
        "pub fn payment_processor() {\n    \
             let amount = 100;\n    \
             let fee = 5;\n    \
             let total = amount + fee;\n    \
             total\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("beta.rs"),
        "pub fn payment_processor_v2() {\n    \
             let amount = 100;\n    \
             let fee = 5;\n    \
             let total = amount * fee;\n    \
             total\n\
         }\n",
    )
    .unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "payment_processor",
                path: "src/alpha.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "payment_processor_v2",
                path: "src/beta.rs",
                line: 1,
                vector: near_ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "payment_processor",
        "--threshold",
        "0.0",
        "--no-stale-check",
        "--explain",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env = parse_envelope(&stdout);
    let results = env["results"].as_array().unwrap();
    assert!(!results.is_empty(), "expected at least one match: {env}");
    let first = &results[0];
    let explanation = &first["explanation"];
    assert!(
        explanation.is_object(),
        "expected explanation object: {first}"
    );
    let jac = explanation["identifier_jaccard"]
        .as_f64()
        .expect("identifier_jaccard");
    // Two-function bodies share `let amount fee total = ;` plus the
    // function name stem — jaccard well above 0.5. A looser bound
    // would silently pass if `fetch_symbol_body` returned empty
    // strings (resulting in 0/0 → undefined; the impl reports 0).
    // Aligned with `cli_explain_test.rs::duplicates_explain_emits_*`.
    assert!(
        jac > 0.5,
        "near-identical bodies should share most identifiers, got jaccard={jac}"
    );
    // Body must have been actually read off disk, else diff is empty
    // and the test passes vacuously. Pin the diff content explicitly.
    let diff = explanation["diff"].as_str().unwrap_or("");
    assert!(!diff.is_empty(), "diff must be non-empty: {explanation}");
    assert!(
        diff.contains('+') && diff.contains('-'),
        "diff should contain both insert and delete markers (bodies \
         differ in one operator): {explanation}"
    );
}

#[test]
fn similar_without_why_leaves_stderr_quiet() {
    // Negative contract: when `--why` is absent, no `VEX_WHY:` trace
    // line should appear on stderr. Mirrors `cli_why_11_10_test.rs::
    // usages_without_why_leaves_stderr_quiet` for `vex similar`.
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "a.rs",
                line: 1,
                vector: ones(),
            },
            Entry {
                name: "Bar",
                path: "a.rs",
                line: 10,
                vector: near_ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "Foo",
        "--threshold",
        "0.0",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        !stderr.contains("VEX_WHY:"),
        "no `--why` flag must mean no VEX_WHY: trace, got stderr: {stderr}"
    );
}

#[test]
fn similar_filter_and_include_compose_with_and_semantics() {
    // `cmd_similar` ANDs `filter_path.contains` with
    // `path_scope.accept` — a result must satisfy BOTH. Covered
    // separately by `similar_filter_path_drops_other_paths` and
    // `similar_scope_include_keeps_only_matching_paths`; here we
    // exercise the combined predicate explicitly.
    let tmp = TempDir::new().unwrap();
    prebuild_index(
        tmp.path(),
        &[
            Entry {
                name: "Foo",
                path: "src/api/a.rs",
                line: 1,
                vector: ones(),
            },
            // Passes --include but not --filter:
            Entry {
                name: "Bar",
                path: "src/util/b.rs",
                line: 1,
                vector: ones(),
            },
            // Passes --filter but not --include:
            Entry {
                name: "Baz",
                path: "tests/api/c.rs",
                line: 1,
                vector: ones(),
            },
            // Passes BOTH:
            Entry {
                name: "Qux",
                path: "src/api/d.rs",
                line: 1,
                vector: ones(),
            },
        ],
    );
    let assert = assert_ran(vex_in(tmp.path()).args([
        "similar",
        "Foo",
        "--threshold",
        "0.0",
        "--filter",
        "api/",
        "--include",
        "src/**",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let env = parse_envelope(&stdout);
    let names = names_in(&env);
    assert_eq!(
        names,
        vec!["Qux"],
        "only paths matching BOTH --filter api/ AND --include src/** \
         should remain (Bar fails filter, Baz fails include): got {names:?}"
    );
}
