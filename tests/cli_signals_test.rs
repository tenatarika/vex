//! CLI integration tests for Phase 13.11 — JSON envelope shape.
//!
//! These tests build a tiny index, run `vex search --format json`, and assert
//! that the envelope fields described in Phase 13 are present. They will fail
//! at Stage 2 because `src/cli/output.rs` has not yet been updated to emit the
//! `ResponseEnvelope` wrapper. They become GREEN in Stage 3.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// Write a minimal two-file project and build an index so search works.
fn seed_corpus(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("alpha.rs"),
        "pub fn alpha_handler() {}\npub fn beta_processor() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("beta.rs"),
        "pub fn gamma_service() {}\npub fn delta_worker() {}\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

/// Run `vex search <query> --format json` and return the parsed JSON value.
fn search_json(dir: &Path, query: &str) -> serde_json::Value {
    let assert = vex_in(dir)
        .args(["search", query, "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("search --format json output is not valid JSON: {e}\n---\n{stdout}")
    })
}

#[test]
fn search_json_envelope_carries_protocol_version() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let out = search_json(tmp.path(), "alpha_handler");
    assert_eq!(
        out["protocol_version"].as_str(),
        Some("v1"),
        "expected top-level protocol_version == \"v1\", got: {}",
        out
    );
}

#[test]
fn search_json_envelope_has_capabilities_block() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let out = search_json(tmp.path(), "alpha_handler");
    assert_eq!(
        out["capabilities"]["signals"].as_bool(),
        Some(true),
        "expected capabilities.signals == true in search envelope, got: {}",
        out
    );
}

#[test]
fn search_json_envelope_has_meta_block() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let out = search_json(tmp.path(), "alpha_handler");
    assert!(
        out["_meta"].is_object(),
        "expected _meta key to be present in envelope, got: {}",
        out
    );
}

#[test]
fn search_json_envelope_meta_index_age_ms_present() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let out = search_json(tmp.path(), "alpha_handler");
    let age = out["_meta"]["vex.dev/index_age_ms"]
        .as_u64()
        .unwrap_or_else(|| {
            panic!(
                "expected _meta[\"vex.dev/index_age_ms\"] to be a non-negative integer, got: {}",
                out["_meta"]
            )
        });
    // index was just created so age should be near zero, but we only assert ≥ 0
    let _ = age;
}

#[test]
fn search_json_results_each_have_signals() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let out = search_json(tmp.path(), "alpha");
    let results = out["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected results array in envelope, got: {}", out));
    assert!(
        !results.is_empty(),
        "expected at least one result for query 'alpha', got empty array"
    );
    for (i, r) in results.iter().enumerate() {
        assert!(
            r["signals"].is_object(),
            "results[{i}] missing signals object, got: {r}"
        );
    }
}

#[test]
fn search_signals_fst_hit_is_bool() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let out = search_json(tmp.path(), "alpha_handler");
    let results = out["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected results array, got: {}", out));
    assert!(!results.is_empty(), "expected at least one result");
    for (i, r) in results.iter().enumerate() {
        let fst_hit = &r["signals"]["fst_hit"];
        assert!(
            fst_hit.is_boolean(),
            "results[{i}].signals.fst_hit must be bool (true or false), got: {fst_hit}"
        );
    }
}

#[test]
fn search_envelope_rank_percentile_present_and_in_range() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let out = search_json(tmp.path(), "alpha");
    let results = out["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected results array, got: {}", out));
    assert!(!results.is_empty(), "expected at least one result");
    for (i, r) in results.iter().enumerate() {
        let rp = r["rank_percentile"]
            .as_f64()
            .unwrap_or_else(|| panic!("results[{i}] missing rank_percentile, got: {r}"));
        assert!(
            (0.0..=1.0).contains(&rp),
            "results[{i}].rank_percentile {rp} out of [0.0, 1.0]"
        );
    }
}

#[test]
fn search_envelope_rank_percentile_monotonic_descending() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let out = search_json(tmp.path(), "alpha");
    let results = out["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected results array, got: {}", out));
    if results.len() < 2 {
        // Can't assert ordering with fewer than 2 results — pass trivially.
        return;
    }
    let percentiles: Vec<f64> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            r["rank_percentile"]
                .as_f64()
                .unwrap_or_else(|| panic!("results[{i}] missing rank_percentile"))
        })
        .collect();
    for i in 1..percentiles.len() {
        assert!(
            percentiles[i - 1] >= percentiles[i],
            "rank_percentile not monotonically descending at index {i}: {} < {}",
            percentiles[i - 1],
            percentiles[i]
        );
    }
    // Boundary lock: with N >= 2 the top result must be exactly 1.0 and
    // the bottom exactly 0.0. The earlier `1.0 - i/total` formula gave
    // `(N-1)/N` for the bottom (e.g. 0.25 for N=4), which this assertion
    // would have caught.
    let top = *percentiles.first().expect("non-empty after len>=2 check");
    let bottom = *percentiles.last().expect("non-empty after len>=2 check");
    assert!(
        (top - 1.0).abs() < 1e-6,
        "top result rank_percentile must be 1.0, got {top}"
    );
    assert!(
        bottom.abs() < 1e-6,
        "bottom result rank_percentile must be 0.0, got {bottom}"
    );
}

/// Backwards-compat opt-out: setting `VEX_JSON_ENVELOPE=0` restores the
/// pre-Phase-13 bare-array shape so existing scripts piping
/// `vex search --format json | jq '.[0].name'` keep working.
#[test]
fn search_envelope_legacy_mode_emits_bare_array_when_env_var_zero() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let assert = vex_in(tmp.path())
        .args(["search", "alpha_handler", "--format", "json"])
        .env("VEX_JSON_ENVELOPE", "0")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("VEX_JSON_ENVELOPE=0 output is not valid JSON: {e}\n---\n{stdout}")
    });
    assert!(
        value.is_array(),
        "VEX_JSON_ENVELOPE=0 must emit a bare JSON array (pre-1.9 shape), got: {value}"
    );
    // Also assert that the bare-array elements look like SearchResult, not
    // the envelope wrapper. Each element should have `name` directly, not
    // be wrapped under `results`.
    if let Some(arr) = value.as_array() {
        for (i, el) in arr.iter().enumerate() {
            assert!(
                el.get("name").is_some(),
                "VEX_JSON_ENVELOPE=0 element [{i}] must look like a bare SearchResult (have a `name` field), got: {el}"
            );
            assert!(
                el.get("protocol_version").is_none(),
                "VEX_JSON_ENVELOPE=0 element [{i}] must not carry envelope fields like protocol_version, got: {el}"
            );
        }
    }
}

/// Regression lock: the `confidence` field was considered during design but
/// rejected (T1 research). It must never appear in the output.
#[test]
fn search_envelope_does_not_include_confidence_field() {
    let tmp = TempDir::new().unwrap();
    seed_corpus(tmp.path());
    let out = search_json(tmp.path(), "alpha");
    let results = out["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected results array, got: {}", out));
    assert!(!results.is_empty(), "expected at least one result");
    for (i, r) in results.iter().enumerate() {
        assert!(
            r.get("confidence").is_none(),
            "results[{i}] must NOT contain 'confidence' key (research-rejected T1 field), got: {r}"
        );
    }
}
