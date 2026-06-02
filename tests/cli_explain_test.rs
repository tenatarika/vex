//! CLI integration tests for v1.7/11.8 `--explain` on
//! `vex similar` and `vex duplicates`. The reasoning artefacts
//! (identifier Jaccard + unified diff) are produced by
//! `src/search/explain.rs`; these tests confirm the wiring all the way
//! through the binary plus the alias `--min-score` on the `--threshold`
//! flag.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

mod common;
use common::assert_ran;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// Create a tempdir with two near-identical functions in different files
/// so `vex duplicates` has something to surface.
///
/// The bodies differ by one line so the diff has exactly one `+` and
/// one `-` change, which lets the test pin the explain output precisely.
fn write_duplicate_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
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
    // Semantic index required — duplicates needs vectors.
    vex_in(dir).args(["index", "--semantic"]).assert().success();
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
        "--format",
        "json",
    ]));
    let with_alias = assert_ran(vex_in(tmp.path()).args([
        "duplicates",
        "--min-score",
        "0.99",
        "--min-body-lines",
        "1",
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
