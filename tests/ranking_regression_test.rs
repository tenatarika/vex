//! Phase 13.12 — ranking regression guard.
//!
//! Runs `vex eval` against the bundled golden set (`benches/ranking_golden/queries.toml`)
//! and asserts the mean nDCG@10 stays above a recorded baseline. CI
//! catches silent ranking degradations this way: a change to the
//! rerank weights, BM25 scoring, or fusion math that drops nDCG below
//! the floor fails the build.
//!
//! Mechanics:
//!   * Run against the vex source tree itself — `CARGO_MANIFEST_DIR`
//!     is the repo root, which is exactly the index target the golden
//!     set was authored against.
//!   * Use a per-test cache dir (`$VEX_CACHE_DIR`) so we never touch
//!     the user's real cache and the test cannot collide with other
//!     concurrent runs.
//!   * Bootstrap the index inline (`vex index`) so the test is
//!     self-contained and CI doesn't need a pre-built index.
//!
//! When intentionally changing ranking math, run `vex eval` locally,
//! observe the new score, and update `BASELINE_NDCG` here. Document
//! the change in CHANGELOG.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

/// Floor for the mean nDCG@10 over the bundled golden set, captured
/// against vex v1.8.2 (Phase 13.12 commit). The measured value at
/// commit time was 0.892 — we leave ~5% headroom for run-to-run
/// variation (tie-breaking in fusion's RRF, the deterministic sort
/// in structural search, etc.).
///
/// Bump this number ONLY when an intentional ranking change has
/// produced a documented improvement. NEVER lower it to silence a
/// failing test.
const BASELINE_NDCG: f64 = 0.85;

/// Per-query-type floors (Phase 13.12.1). A mean-only floor lets a
/// silent regression in one channel hide behind perfect scores in the
/// others — `semantic` falling from 0.73 to 0.50 still leaves the mean
/// above 0.85 with the current 5/5/3/3 query mix. Pin each bucket so
/// per-channel regressions surface.
///
/// Measured at 13.12.1 commit time:
///   exact_symbol  1.000
///   bm25_rare     1.000
///   fuzzy         0.833
///   semantic      0.729
///
/// Headroom is roughly proportional to the channel's tie-breaking
/// sensitivity — exact / bm25 are deterministic identifier lookups so
/// floors hug the measured value; fuzzy / semantic have more RRF jitter
/// so floors sit further below.
const FLOOR_EXACT_SYMBOL: f64 = 0.95;
const FLOOR_BM25_RARE: f64 = 0.95;
const FLOOR_FUZZY: f64 = 0.75;
const FLOOR_SEMANTIC: f64 = 0.65;

fn per_type_floor(query_type: &str) -> Option<f64> {
    match query_type {
        "exact_symbol" => Some(FLOOR_EXACT_SYMBOL),
        "bm25_rare" => Some(FLOOR_BM25_RARE),
        "fuzzy" => Some(FLOOR_FUZZY),
        "semantic" => Some(FLOOR_SEMANTIC),
        _ => None,
    }
}

fn vex(repo_root: &Path, cache_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(repo_root);
    cmd.env("VEX_CACHE_DIR", cache_dir);
    cmd
}

#[test]
fn mean_ndcg_stays_above_baseline() {
    // CARGO_MANIFEST_DIR points at the vex repo root at compile time.
    // No env-var lookup at runtime — robust under nextest, parallel
    // cargo invocations, and `cargo test --workspace`.
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tmp_cache = TempDir::new().expect("create tempdir");

    // Build the index from scratch in the temp cache. We pass
    // `--no-bm25` would defeat the purpose — keep all channels
    // active. No --semantic because CI shouldn't need to download a
    // model just to run a regression guard; the harness gracefully
    // skips the semantic channel when vectors aren't present.
    vex(repo_root, tmp_cache.path())
        .arg("index")
        .assert()
        .success();

    // Run the eval with the threshold pinned to the baseline. If
    // mean nDCG drops below, the subcommand exits non-zero and
    // assert_cmd::success() asserts on it.
    let output = vex(repo_root, tmp_cache.path())
        .args([
            "eval",
            "--min-ndcg",
            &format!("{BASELINE_NDCG:.4}"),
            "--json",
        ])
        .assert()
        .success();

    // Cross-check: parse the JSON report and verify the mean was
    // actually computed (not zeroed out by an empty golden set). This
    // catches "we shipped without bundling queries.toml" regressions.
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    let envelope: serde_json::Value =
        serde_json::from_str(&stdout).expect("eval --json should emit valid JSON envelope");
    let report = envelope
        .get("results")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let mean_ndcg = report["mean_ndcg"].as_f64().expect("mean_ndcg field");
    let total = report["total_queries"]
        .as_u64()
        .expect("total_queries field");
    assert!(
        total >= 10,
        "golden set unexpectedly small: {total} queries"
    );
    assert!(
        mean_ndcg >= BASELINE_NDCG,
        "mean nDCG@10 {mean_ndcg:.4} below baseline {BASELINE_NDCG:.4} — \
         a ranking regression has likely landed. Either fix it or, if \
         intentional, update BASELINE_NDCG in this file and document \
         the change."
    );

    // Per-type floors — Phase 13.12.1. Mean nDCG can stay above its
    // floor while one channel quietly collapses; the per-type asserts
    // catch that. If a query_type appears in the golden set but isn't
    // in `per_type_floor`, we skip it (forward-compat for new types).
    let by_type = report["by_type"].as_array().expect("by_type array");
    let mut checked = 0;
    for bucket in by_type {
        let qt = bucket["query_type"].as_str().expect("query_type field");
        let bucket_ndcg = bucket["mean_ndcg"].as_f64().expect("mean_ndcg field");
        if let Some(floor) = per_type_floor(qt) {
            checked += 1;
            assert!(
                bucket_ndcg >= floor,
                "per-type nDCG@10 for {qt} = {bucket_ndcg:.4} below floor {floor:.4} — \
                 the {qt} channel regressed even though the global mean stayed up. \
                 Either fix the regression or update the FLOOR_* constant with a CHANGELOG note."
            );
        }
    }
    // Pin to the exact number of FLOOR_* constants. `>= 3` would
    // silently tolerate one floor going dark if a query_type were
    // renamed in queries.toml or a constant deleted from
    // per_type_floor. The fix is to assert the exact count.
    assert_eq!(
        checked, 4,
        "expected 4 query-type floors to be exercised, got {checked}; \
         either the golden set lost coverage or a query_type was \
         renamed without updating per_type_floor()"
    );
}

/// Smoke test that runs against a tiny synthetic fixture so CI can
/// exercise the eval pipeline cheaply (no full-repo index). Catches
/// breakage in the harness itself — TOML parsing, metric wiring,
/// JSON envelope — without paying for the full regression run.
#[test]
fn eval_smoke_runs_on_minimal_fixture() {
    let tmp_proj = TempDir::new().expect("create tempdir");
    let tmp_cache = TempDir::new().expect("create tempdir");

    // Minimal source tree with one matchable symbol.
    std::fs::write(
        tmp_proj.path().join("a.rs"),
        "pub fn smoke_target() {}\npub struct Other;\n",
    )
    .unwrap();
    std::fs::write(tmp_proj.path().join(".vex.toml"), "local_cache = false\n").unwrap();

    // Custom golden set scoped to the fixture.
    let golden = r#"
[[queries]]
query = "smoke_target"
query_type = "exact_symbol"
expected_top_path = "a.rs"
acceptable_paths = ["a.rs"]
"#;
    let golden_path = tmp_proj.path().join("golden.toml");
    std::fs::write(&golden_path, golden).unwrap();

    Command::cargo_bin("vex")
        .unwrap()
        .current_dir(tmp_proj.path())
        .env("VEX_CACHE_DIR", tmp_cache.path())
        .args(["index"])
        .assert()
        .success();

    let assert = Command::cargo_bin("vex")
        .unwrap()
        .current_dir(tmp_proj.path())
        .env("VEX_CACHE_DIR", tmp_cache.path())
        .args([
            "eval",
            "--bench",
            golden_path.to_str().unwrap(),
            "--min-ndcg",
            "0.99",
            "--json",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let report = envelope
        .get("results")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    assert_eq!(report["total_queries"].as_u64().unwrap(), 1);
    assert!((report["mean_ndcg"].as_f64().unwrap() - 1.0).abs() < 1e-9);
    assert!(report["per_query"][0]["top1_hit"].as_bool().unwrap());

    // Negative path: bumping --min-ndcg above 1.0 must trigger non-zero exit.
    Command::cargo_bin("vex")
        .unwrap()
        .current_dir(tmp_proj.path())
        .env("VEX_CACHE_DIR", tmp_cache.path())
        .args([
            "eval",
            "--bench",
            golden_path.to_str().unwrap(),
            "--min-ndcg",
            "1.5",
        ])
        .assert()
        .failure();
}
