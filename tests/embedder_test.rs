//! Integration tests for Phase 9.1 — pluggable embedder backend.
//!
//! These tests pin the public contract of `vex::embed` (registry, dim lookup,
//! mismatch detection), `Manifest::embedder_id`, `VexConfig::embedder`, and
//! the updated `write_index_full` writer signature.
//!
//! **No model is loaded.** `MiniLMEmbedder::new()` is never called — doing so
//! would trigger an 86 MB download. All semantics-level assertions operate
//! through the registry metadata functions.

use tempfile::TempDir;
use vex::embed::minilm::MINILM_CHAR_BUDGET;
use vex::embed::{
    build_context, check_embedder_match, embedder_dim, known_embedders, make_embedder,
    resolve_embedder, DEFAULT_EMBEDDER, MINILM_DIM, MINILM_ID,
};
use vex::index::manifest::Manifest;
use vex::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
use vex::store::reader::IndexReader;
use vex::store::writer::write_index_full;
use vex::util::config::DEFAULT_CONFIG;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_sym(name: &str) -> ParsedSymbol {
    ParsedSymbol {
        name: name.to_string(),
        kind: SymbolKind::Function,
        line: 1,
        signature: Some(format!("fn {name}()")),
        doc: None,
        body_tokens: None,
    }
}

fn one_symbol_parsed() -> Vec<ParsedFile> {
    vec![ParsedFile {
        path: "src/lib.rs".to_string(),
        symbols: vec![make_sym("example_fn")],
        refs: vec![],
        call_edges: vec![],
        bound_refs: vec![],
        skeletons: Vec::new(),
    }]
}

// ---------------------------------------------------------------------------
// Registry — no model load
// ---------------------------------------------------------------------------

/// `embedder_dim("minilm-l6-v2")` returns `Some(384)`.
#[test]
fn embedder_dim_minilm() {
    assert_eq!(embedder_dim(MINILM_ID), Some(MINILM_DIM));
    assert_eq!(embedder_dim("minilm-l6-v2"), Some(384));
}

/// `embedder_dim` for an unknown id returns `None`.
#[test]
fn embedder_dim_unknown() {
    assert_eq!(embedder_dim("bge-large"), None);
    assert_eq!(embedder_dim("unknown-model"), None);
}

/// `known_embedders()` includes `"minilm-l6-v2"`.
#[test]
fn known_embedders_contains_minilm() {
    let list = known_embedders();
    assert!(
        list.contains(&"minilm-l6-v2"),
        "known_embedders() should contain \"minilm-l6-v2\", got: {list:?}"
    );
}

/// `make_embedder("bge-large")` returns `Err`; the error message must mention
/// both the unknown id and at least one known id ("minilm-l6-v2").
#[test]
fn make_embedder_unknown_errors() {
    let result = make_embedder("bge-large");
    assert!(
        result.is_err(),
        "make_embedder(\"bge-large\") should return Err"
    );
    // Extract the error without requiring T: Debug on Box<dyn Embedder>.
    let msg = result.err().unwrap().to_string();
    assert!(
        msg.contains("bge-large"),
        "error should mention the unknown id \"bge-large\", got: {msg}"
    );
    assert!(
        msg.contains("minilm-l6-v2"),
        "error should list a known embedder \"minilm-l6-v2\", got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Resolve priority
// ---------------------------------------------------------------------------

/// CLI flag wins over config and default.
#[test]
fn resolve_embedder_cli_wins() {
    let result = resolve_embedder(Some("x"), Some("y"));
    assert_eq!(result, "x");
}

/// Config wins when CLI is absent.
#[test]
fn resolve_embedder_config_when_no_cli() {
    let result = resolve_embedder(None, Some("y"));
    assert_eq!(result, "y");
}

/// Falls back to `DEFAULT_EMBEDDER` when neither CLI nor config is set.
#[test]
fn resolve_embedder_default_when_neither() {
    let result = resolve_embedder(None, None);
    assert_eq!(result, DEFAULT_EMBEDDER);
    assert_eq!(result, "minilm-l6-v2");
}

// ---------------------------------------------------------------------------
// Mismatch detection
// ---------------------------------------------------------------------------

/// Stored embedder differs from requested → `Err` with both IDs and `--embedder` hint.
#[test]
fn mismatch_when_stored_differs() {
    let result = check_embedder_match(Some("bge-small"), "minilm-l6-v2");
    assert!(result.is_err(), "should return Err on embedder mismatch");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("bge-small"),
        "error should mention stored id \"bge-small\", got: {msg}"
    );
    assert!(
        msg.contains("minilm-l6-v2"),
        "error should mention requested id \"minilm-l6-v2\", got: {msg}"
    );
    assert!(
        msg.contains("--embedder") || msg.contains("embedder"),
        "error should contain a rebuild hint mentioning embedder, got: {msg}"
    );
}

/// Same stored and requested id → `Ok(())`.
#[test]
fn mismatch_passes_when_equal() {
    let result = check_embedder_match(Some("minilm-l6-v2"), "minilm-l6-v2");
    assert!(
        result.is_ok(),
        "should return Ok when ids match, got: {result:?}"
    );
}

/// `None` stored (pre-9.1 manifest) with the default embedder requested → `Ok(())`.
#[test]
fn mismatch_back_compat_none_is_default() {
    let result = check_embedder_match(None, "minilm-l6-v2");
    assert!(
        result.is_ok(),
        "None stored should be treated as default (minilm-l6-v2), got: {result:?}"
    );
}

/// `None` stored but a non-default embedder requested → `Err`.
#[test]
fn mismatch_back_compat_none_vs_non_default_errors() {
    let result = check_embedder_match(None, "bge-small");
    assert!(
        result.is_err(),
        "None stored == default, but \"bge-small\" != default → should return Err"
    );
}

// ---------------------------------------------------------------------------
// Manifest roundtrip
// ---------------------------------------------------------------------------

/// `embedder_id` is preserved across save/load.
#[test]
fn manifest_persists_embedder_id() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("manifest.json");

    let manifest = Manifest {
        embedder_id: Some("minilm-l6-v2".to_string()),
        ..Default::default()
    };
    manifest.save(&path).expect("save manifest");

    let loaded = Manifest::load(&path).expect("load manifest");
    assert_eq!(
        loaded.embedder_id,
        Some("minilm-l6-v2".to_string()),
        "embedder_id should round-trip through save/load"
    );
}

/// A manifest JSON without `embedder_id` field loads successfully with `None`.
/// Validates `#[serde(default)]` back-compat.
#[test]
fn manifest_backward_compat_missing_field() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("manifest.json");

    // Write by hand without the embedder_id field.
    std::fs::write(&path, r#"{"files": {}}"#).unwrap();

    let loaded = Manifest::load(&path).expect("load manifest without embedder_id");
    assert_eq!(
        loaded.embedder_id, None,
        "missing embedder_id field in JSON must deserialize as None"
    );
}

/// When `embedder_id` is `None`, the serialized JSON must NOT contain the key
/// (validates `skip_serializing_if = "Option::is_none"`).
#[test]
fn manifest_skips_serializing_when_none() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("manifest.json");

    let manifest = Manifest {
        embedder_id: None,
        ..Default::default()
    };
    manifest.save(&path).expect("save manifest");

    let json = std::fs::read_to_string(&path).expect("read manifest json");
    assert!(
        !json.contains("embedder_id"),
        "serialized JSON must not contain \"embedder_id\" when the field is None, got: {json}"
    );
}

// ---------------------------------------------------------------------------
// Config roundtrip
// ---------------------------------------------------------------------------

/// `.vex.toml` with `embedder = "minilm-l6-v2"` loads correctly.
#[test]
fn config_parses_embedder_field() {
    let tmp = TempDir::new().unwrap();
    let toml_path = tmp.path().join(".vex.toml");
    std::fs::write(&toml_path, "embedder = \"minilm-l6-v2\"\n").unwrap();

    let cfg = vex::util::config::load_config(tmp.path()).expect("load config");
    assert_eq!(
        cfg.embedder,
        Some("minilm-l6-v2".to_string()),
        "config should parse embedder field"
    );
}

/// `.vex.toml` without an `embedder` line → `cfg.embedder == None`.
#[test]
fn config_omitted_embedder_is_none() {
    let tmp = TempDir::new().unwrap();
    let toml_path = tmp.path().join(".vex.toml");
    std::fs::write(&toml_path, "# no embedder line\n").unwrap();

    let cfg = vex::util::config::load_config(tmp.path()).expect("load config");
    assert_eq!(
        cfg.embedder, None,
        "omitted embedder in .vex.toml should parse as None"
    );
}

/// `DEFAULT_CONFIG` contains a commented `embedder` line as a help hint.
#[test]
fn config_default_template_contains_embedder_hint() {
    assert!(
        DEFAULT_CONFIG.contains("embedder"),
        "DEFAULT_CONFIG should contain an embedder hint, got:\n{DEFAULT_CONFIG}"
    );
}

// ---------------------------------------------------------------------------
// build_context budget
// ---------------------------------------------------------------------------

/// A budget smaller than the context prefix suppresses the body entirely.
#[test]
fn build_context_respects_budget() {
    let body = "alpha beta gamma delta epsilon zeta eta theta iota";
    let ctx = build_context("function", "foo", "src/lib.rs", None, None, Some(body), 10);
    assert!(
        !ctx.contains("body:"),
        "body section must be suppressed when budget=10, got: {ctx}"
    );
}

/// With a full budget the body tokens must appear in the output.
#[test]
fn build_context_default_budget_includes_body() {
    let body = "alpha beta gamma delta epsilon zeta eta theta iota";
    let ctx = build_context(
        "function",
        "foo",
        "src/lib.rs",
        None,
        None,
        Some(body),
        MINILM_CHAR_BUDGET,
    );
    assert!(
        ctx.contains("body:"),
        "body section must appear when using full budget, got: {ctx}"
    );
    // At least one token from the body must be present.
    assert!(
        ctx.contains("alpha") || ctx.contains("beta"),
        "body tokens must appear in output, got: {ctx}"
    );
}

// ---------------------------------------------------------------------------
// Writer vector_dim
// ---------------------------------------------------------------------------

/// `write_index_full` with a custom `vector_dim` persists that value in the
/// Header. Proves the writer no longer hardcodes 384.
#[test]
fn writer_records_custom_vector_dim() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");
    let parsed = one_symbol_parsed();

    write_index_full(&parsed, &[], 768, &index_path).expect("write_index_full");

    let reader = IndexReader::open(&index_path).expect("open index");
    assert_eq!(
        reader.header().vector_dim,
        768,
        "Header.vector_dim should reflect the custom 768 dim passed to write_index_full"
    );
}

/// `write_index_full` with a vector whose length mismatches `vector_dim` returns `Err`.
#[test]
fn writer_validates_vector_length() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");
    let parsed = one_symbol_parsed();

    // Vector of 100 floats but vector_dim=384 → mismatch.
    let bad_vector = vec![0.0_f32; 100];
    let result = write_index_full(&parsed, &[bad_vector], 384, &index_path);
    assert!(
        result.is_err(),
        "write_index_full should return Err when vector length != vector_dim"
    );
}
