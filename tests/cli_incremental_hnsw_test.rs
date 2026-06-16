//! v1.15.0 B1.2 — end-to-end CLI integration coverage for the
//! incremental HNSW update path.
//!
//! Pins the orchestrator's `match build_hnsw_incremental(...)?` wiring
//! at `src/index/pipeline/mod.rs::update`: when the pre-baked index has
//! both `index.hashes` + `index.bodytokens` sidecars and the diff is
//! small enough, `vex update --semantic` must take the incremental
//! path (`Ok(true)`) and skip the full `build_hnsw` rebuild. We assert
//! the path via tracing output captured from stderr — the
//! `tracing::info!("HNSW incremental update applied")` from
//! `build_hnsw_incremental_at` is the contract.
//!
//! Bypasses ONNX entirely via the pre-baked-vectors pattern: every
//! symbol's `context_hash` is pre-computed against
//! `embed::build_context` and `embed::cache::context_hash`, seeded
//! into `embed_cache_minilm-l6-v2.bin`. The in-memory `EmbedCache::get`
//! hit during `generate_embeddings` short-circuits the model load
//! (same path `all_hit_returns_cached_vectors_without_model_load` in
//! `pipeline::output::tests` pins at the unit level).

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;
use vex::embed::cache::{context_hash, EmbedCache};
use vex::embed::{build_context, MINILM_CHAR_BUDGET, MINILM_DIM, MINILM_ID};
use vex::index::hasher;
use vex::index::manifest::Manifest;
use vex::index::pipeline::build_hnsw_at;
use vex::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
use vex::search::hash_index;
use vex::store::body_tokens;
use vex::store::writer::write_index_full;

/// Unit vector of dimension `dim` with `1.0` at slot `slot` — orthogonal
/// across different `slot` values so HNSW similarity stays unambiguous
/// (cosine = 1 for self-query, ~0 for any other slot).
fn one_hot(slot: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; MINILM_DIM as usize];
    v[slot] = 1.0;
    v
}

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env_remove("VEX_CACHE_DIR");
    // Hermetic w.r.t. the host: a developer machine may pin a global default
    // embedder/device (documented user-level env vars). The fixtures here are
    // seeded at MINILM_DIM — an ambient `VEX_EMBEDDER=jina-code` (768-d) would
    // make the spawned `vex update --semantic` fail with a dim mismatch.
    cmd.env_remove("VEX_EMBEDDER");
    cmd.env_remove("VEX_DEVICE");
    // Capture the incremental-update tracing event by enabling info-
    // level emit. `build_hnsw_incremental_at` calls
    // `tracing::info!("HNSW incremental update applied")` on the
    // Ok(true) branch — the integration probe.
    cmd.env("RUST_LOG", "info");
    cmd
}

/// Compute the `context_hash` a v1.15+ pipeline would assign to the
/// described symbol. Mirrors `pipeline::output::compute_hashes_for`
/// exactly: same `build_context` call shape, same `context_hash` key.
fn hash_for(symbol_kind: &str, name: &str, path: &str, body: Option<&str>) -> u64 {
    let sig = format!("pub fn {name}() {{}}");
    let ctx = build_context(
        symbol_kind,
        name,
        path,
        Some(&sig),
        None,
        body,
        MINILM_CHAR_BUDGET,
    );
    context_hash(MINILM_ID, &ctx)
}

fn mk_sym(name: &str) -> ParsedSymbol {
    // `pub fn alpha() {}` → body_tokens=Some(name) because the function
    // identifier is itself a node walked by `extract_body_tokens`. Matches
    // what `parse::parse_file` produces for a minimal Rust function;
    // verified via a one-shot probe before authoring this test.
    ParsedSymbol {
        name: name.to_string(),
        kind: SymbolKind::Function,
        line: 1,
        signature: Some(format!("pub fn {name}() {{}}")),
        doc: None,
        body_tokens: Some(name.to_string()),
    }
}

fn mk_parsed_file(path: &str, name: &str) -> ParsedFile {
    ParsedFile {
        path: path.to_string(),
        symbols: vec![mk_sym(name)],
        refs: vec![],
        call_edges: vec![],
        bound_refs: vec![],
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
    }
}

/// Lay out a project with one pre-existing symbol (`alpha` in
/// `src/alpha.rs`), pre-bake every artefact a v1.15.0 `vex index --semantic`
/// would produce, and pre-seed the embed cache so a subsequent
/// `vex update --semantic` adds a symbol without triggering ONNX.
/// Returns the cache root for sidecar inspection.
fn bake_v115_index(dir: &Path) -> PathBuf {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let alpha_content = "pub fn alpha() {}\n";
    let alpha_path = dir.join("src").join("alpha.rs");
    std::fs::write(&alpha_path, alpha_content).unwrap();

    let cache_root = dir.join(".vex_cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    // Pre-baked v6 index + sidecars + manifest. Matches exactly what
    // `pipeline::run` would have produced — caller's `vex update`
    // reads these and proceeds as if a real `vex index --semantic`
    // had run.
    let parsed = vec![mk_parsed_file("src/alpha.rs", "alpha")];
    let vec_alpha = one_hot(0);
    write_index_full(
        &parsed,
        std::slice::from_ref(&vec_alpha),
        MINILM_DIM,
        &cache_root.join("index.vex"),
    )
    .expect("write_index_full");

    // HNSW + paired hash-index sidecar in one call (build_hnsw_at
    // writes both). Hash matches what compute_hashes_for will recompute
    // on the next update — that's the prerequisite for the incremental
    // diff to see "0 removes, 1 add" instead of "1 remove, 1 add".
    let h_alpha = hash_for("function", "alpha", "src/alpha.rs", Some("alpha"));
    let hnsw_path = cache_root.join("index.hnsw");
    let hash_path = cache_root.join("index.hashes");
    // The HNSW + hash-index sidecar pair: drive the same code path
    // production uses via the `#[doc(hidden)]` re-export from
    // `vex::index::pipeline`. Previously this test inlined a copy of
    // the usearch options + add loop; v1.15.0 D-cleanup removed the
    // duplication so a future `IndexOptions` tweak in `output.rs`
    // applies here transparently.
    build_hnsw_at(
        &hnsw_path,
        &hash_path,
        std::slice::from_ref(&vec_alpha),
        &[h_alpha],
    )
    .expect("build_hnsw_at");

    // body_tokens sidecar — `Some("alpha")` matches what
    // `extract_body_tokens` produces for a `pub fn alpha() {}` AST.
    body_tokens::save(
        &cache_root.join("index.bodytokens"),
        &[Some("alpha".to_string())],
    )
    .expect("body_tokens sidecar write");

    // Manifest: every v1.15+ marker `Some(true)`, file_hashes match
    // on-disk content. `diff_files` against this manifest must report
    // zero changed/deleted for src/alpha.rs (we wrote the exact same
    // bytes) so the only churn the update sees is the new file we add
    // below.
    let alpha_disk_hash = hasher::content_hash(alpha_content.as_bytes());
    let manifest = Manifest {
        files: [("src/alpha.rs".to_string(), alpha_disk_hash)]
            .into_iter()
            .collect(),
        git_head: None,
        indexed_at: Some(0),
        embedder_id: Some(MINILM_ID.to_string()),
        call_graph: Some(true),
        bm25: Some(true),
        pattern_index: Some(true),
        pattern_index_full: Some(true),
        vectors_normalized: Some(true),
        cpp_includes_processed: Some(true),
        body_tokens_persisted: Some(true),
        history_indexed_at: None,
        history_tip_sha: None,
        history_depth: None,
        history: None,
        rename_chains_built: None,
        rename_chains_minilm_tiebreak_hits: None,
        imported_by: Default::default(),
        imported_by_built: None,
    };
    manifest
        .save(&cache_root.join("manifest.json"))
        .expect("manifest write");

    // Embed cache: pre-seed with the hashes the update will look up.
    // alpha is reconstructed from the index so its hash isn't probed
    // against the cache during update (`generate_embeddings` runs only
    // over changed files). The cache still needs to be on disk in the
    // right shape so `EmbedCache::load` succeeds and the lookup for
    // the freshly-parsed beta hits.
    let h_beta = hash_for("function", "beta", "src/beta.rs", Some("beta"));
    let vec_beta = one_hot(1);
    let mut cache = EmbedCache::empty(MINILM_ID, MINILM_DIM);
    cache.insert(h_beta, vec_beta);
    // alpha's cache entry isn't strictly required, but seeding it
    // matches what a real `vex index --semantic` would have left
    // behind and exercises the partial-hit path.
    cache.insert(h_alpha, vec_alpha);
    cache
        .save(&cache_root.join(format!("embed_cache_{MINILM_ID}.bin")))
        .expect("embed cache save");

    cache_root
}

#[test]
fn vex_update_semantic_with_one_added_file_takes_incremental_hnsw_path() {
    let tmp = TempDir::new().unwrap();
    let cache_root = bake_v115_index(tmp.path());

    // Add src/beta.rs — the only churn the update sees. With diff
    // reporting 1 add + 0 removes against an old_size of 1, tombstone
    // arithmetic gives 0/1 < 25% so the incremental path applies.
    std::fs::write(tmp.path().join("src").join("beta.rs"), "pub fn beta() {}\n").unwrap();

    let assert = vex_in(tmp.path())
        .args(["update", "--semantic"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert!(
        stderr.contains("HNSW incremental update applied"),
        "expected incremental HNSW path to fire on a 1-add update, but stderr was:\n{stderr}"
    );

    // Negative guard: the fallback log line should NOT appear — if it
    // does, build_hnsw_incremental returned Ok(false) and the pipeline
    // dropped to the full rebuild path silently. That's the regression
    // the test is here to catch.
    assert!(
        !stderr.contains("tombstone threshold exceeded"),
        "small-add update tripped tombstone fallback: {stderr}"
    );

    // The hash-index sidecar must now carry both hashes in sym_idx
    // order (alpha at 0, beta at 1) — proves the post-incremental
    // rewrite in `build_hnsw_incremental_at` worked.
    let h_alpha = hash_for("function", "alpha", "src/alpha.rs", Some("alpha"));
    let h_beta = hash_for("function", "beta", "src/beta.rs", Some("beta"));
    let loaded = hash_index::load(&cache_root.join("index.hashes")).expect("hash-index load");
    assert_eq!(
        loaded,
        vec![h_alpha, h_beta],
        "post-update hash-index sidecar must reflect new sym_idx layout"
    );

    // Manifest still carries `body_tokens_persisted: Some(true)` —
    // the update wrote a fresh sidecar successfully.
    let manifest =
        Manifest::load(&cache_root.join("manifest.json")).expect("manifest load post-update");
    assert_eq!(manifest.body_tokens_persisted, Some(true));
}
