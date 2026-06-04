//! CLI integration tests for v1.7/11.8 `--explain` on
//! `vex similar` and `vex duplicates`. The reasoning artefacts
//! (identifier Jaccard + unified diff) are produced by
//! `src/search/explain.rs`; these tests confirm the wiring all the way
//! through the binary plus the alias `--min-score` on the `--threshold`
//! flag.
//!
//! v1.12.0: switched from `vex index --semantic` (which downloads the
//! MiniLM ONNX model and gets rate-limited on CI with HTTP 429) to the
//! pre-baked-vectors pattern from `cli_similar_test.rs`:
//! `write_index_full` drops a v6 vector-bearing index at the
//! `local_cache = true` cache path, `--no-stale-check` skips the
//! manifest probe. Tests run in <2s and have no network dependency.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;
use vex::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
use vex::store::format::VECTOR_DIM;
use vex::store::writer::write_index_full;

mod common;
use common::assert_ran;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    // Force `.vex.toml`'s `local_cache = true` to win so the index
    // lands at a predictable `<dir>/.vex_cache/index.vex` and we can
    // pre-place a vector-bearing index there.
    cmd.env_remove("VEX_CACHE_DIR");
    cmd
}

fn ones() -> Vec<f32> {
    vec![1.0_f32; VECTOR_DIM as usize]
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

/// Lay out two near-identical functions in separate files and drop a
/// pre-baked v6 index with vectors that guarantee a duplicate pair
/// above any reasonable threshold. The bodies differ in one operator
/// (`+` vs `*`) so `--explain` produces a one-line `+`/`-` diff —
/// mirrors the shape of the previous `vex index --semantic` fixture
/// without the network dependency.
fn write_duplicate_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    let cache_root = dir.join(".vex_cache");
    std::fs::create_dir_all(&cache_root).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("alpha.rs"),
        "pub fn payment_processor() {\n    \
             let amount = 100;\n    \
             let fee = 5;\n    \
             let total = amount + fee;\n    \
             total\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("beta.rs"),
        "pub fn payment_processor() {\n    \
             let amount = 100;\n    \
             let fee = 5;\n    \
             let total = amount * fee;\n    \
             total\n\
         }\n",
    )
    .unwrap();

    // Two symbols with near-identical vectors → cosine ≈ 1.0, well
    // above any --threshold the tests set. Same name across two
    // files is exactly the canonical "duplicate" shape.
    let parsed = vec![
        ParsedFile {
            path: "src/alpha.rs".to_string(),
            symbols: vec![mk_sym("payment_processor", 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
        },
        ParsedFile {
            path: "src/beta.rs".to_string(),
            symbols: vec![mk_sym("payment_processor", 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
        },
    ];
    let vectors = vec![ones(), near_ones()];
    write_index_full(&parsed, &vectors, 384, &cache_root.join("index.vex"))
        .expect("write_index_full");
}

#[test]
fn duplicates_explain_emits_jaccard_and_diff() {
    let tmp = TempDir::new().unwrap();
    write_duplicate_project(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args([
        "duplicates",
        "--threshold",
        "0.5",
        "--min-body-lines",
        "1",
        "--explain",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let envelope: serde_json::Value =
        serde_json::from_str(&stdout).expect("duplicates --explain emits envelope");
    let json = envelope
        .get("results")
        .expect("envelope has results")
        .clone();
    let pairs = json.as_array().expect("expected array of pairs");
    assert!(!pairs.is_empty(), "expected at least one duplicate pair");

    let first = &pairs[0];
    let explanation = &first["explanation"];
    assert!(
        explanation.is_object(),
        "expected explanation object on first pair: {first}"
    );
    let jac = explanation["identifier_jaccard"]
        .as_f64()
        .expect("explanation.identifier_jaccard is a number");
    assert!(
        jac > 0.5,
        "near-identical bodies should share most identifiers, got jaccard={jac}"
    );
    let added = explanation["diff_added"].as_u64().unwrap_or(99);
    let removed = explanation["diff_removed"].as_u64().unwrap_or(99);
    assert!(
        added >= 1 && removed >= 1,
        "expected at least one +/- change line, got +{added} -{removed}"
    );
    assert!(
        explanation["diff"].as_str().unwrap_or("").contains('+'),
        "diff string should contain insert markers: {}",
        explanation["diff"]
    );
}

#[test]
fn duplicates_without_explain_omits_explanation_field() {
    let tmp = TempDir::new().unwrap();
    write_duplicate_project(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args([
        "duplicates",
        "--threshold",
        "0.5",
        "--min-body-lines",
        "1",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let json = envelope
        .get("results")
        .expect("envelope has results")
        .clone();
    let pairs = json.as_array().unwrap();
    if let Some(first) = pairs.first() {
        assert!(
            first.get("explanation").is_none(),
            "explanation must not appear without --explain, got: {first}"
        );
    }
}

#[test]
fn min_score_alias_matches_threshold_behavior() {
    let tmp = TempDir::new().unwrap();
    write_duplicate_project(tmp.path());

    // `--threshold 0.99` should match `--min-score 0.99`: both produce
    // the same JSON because they are clap aliases. Diff in either
    // direction would indicate the alias isn't wired.
    let with_threshold = assert_ran(vex_in(tmp.path()).args([
        "duplicates",
        "--threshold",
        "0.99",
        "--min-body-lines",
        "1",
        "--no-stale-check",
        "--format",
        "json",
    ]));
    let with_alias = assert_ran(vex_in(tmp.path()).args([
        "duplicates",
        "--min-score",
        "0.99",
        "--min-body-lines",
        "1",
        "--no-stale-check",
        "--format",
        "json",
    ]));

    let a = String::from_utf8_lossy(&with_threshold.get_output().stdout).into_owned();
    let b = String::from_utf8_lossy(&with_alias.get_output().stdout).into_owned();
    let mut va: serde_json::Value = serde_json::from_str(&a).expect("threshold stdout JSON");
    let mut vb: serde_json::Value = serde_json::from_str(&b).expect("min-score stdout JSON");
    // `_meta.vex.dev/index_age_ms` is wall-clock-dependent (rounded to
    // seconds), so two sequential calls flake at second boundaries.
    // The aliases are about ranking equivalence; strip the timing
    // metadata before comparing.
    if let Some(meta) = va.get_mut("_meta").and_then(|m| m.as_object_mut()) {
        meta.remove("vex.dev/index_age_ms");
    }
    if let Some(meta) = vb.get_mut("_meta").and_then(|m| m.as_object_mut()) {
        meta.remove("vex.dev/index_age_ms");
    }
    assert_eq!(
        va, vb,
        "--min-score should behave identically to --threshold"
    );
}
