/// Integration tests for Phase 9.4 — BM25 channel in hybrid search.
///
/// Tests exercise: format sizing, writer/reader roundtrip with and without
/// BM25 sections, pipeline end-to-end, quality/ranking properties, fusion,
/// CLI adapter, and tokenization edge cases.
use std::fs;

use tempfile::TempDir;
use vex::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
use vex::search::MatchType;
use vex::store::bm25::{tokenize_document, tokenize_query, Bm25IndexBuilder, Bm25Reader, B, K1};
use vex::store::format::{CallGraphHeader, Header};
use vex::store::reader::IndexReader;
use vex::store::writer::write_index_with_call_graph;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn make_symbol(name: &str, line: usize, body_tokens: Option<&str>) -> ParsedSymbol {
    ParsedSymbol {
        name: name.to_string(),
        kind: SymbolKind::Function,
        line,
        signature: None,
        doc: None,
        body_tokens: body_tokens.map(|s| s.to_string()),
    }
}

fn make_file(path: &str, symbols: Vec<ParsedSymbol>) -> ParsedFile {
    ParsedFile {
        path: path.to_string(),
        symbols,
        refs: Vec::new(),
        call_edges: Vec::new(),
    }
}

fn build_small_bm25(doc_count: usize, docs: &[(u32, &[&str])]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut b = Bm25IndexBuilder::new(doc_count);
    for (idx, terms) in docs {
        let owned: Vec<String> = terms.iter().map(|s| s.to_string()).collect();
        b.add_document(*idx, &owned);
    }
    b.build().unwrap()
}

fn write_no_bm25(tmp: &TempDir, parsed: &[ParsedFile]) -> IndexReader {
    let path = tmp.path().join("index.vex");
    write_index_with_call_graph(parsed, &[], 384, &[], None, &path).unwrap();
    IndexReader::open(&path).unwrap()
}

fn write_with_bm25(
    tmp: &TempDir,
    parsed: &[ParsedFile],
    fst: &[u8],
    posts: &[u8],
    stats: &[u8],
) -> IndexReader {
    let path = tmp.path().join("index.vex");
    write_index_with_call_graph(parsed, &[], 384, &[], Some((fst, posts, stats)), &path).unwrap();
    IndexReader::open(&path).unwrap()
}

fn make_result(
    name: &str,
    path: &str,
    line: usize,
    score: f64,
    match_type: MatchType,
) -> vex::search::SearchResult {
    vex::search::SearchResult {
        name: name.to_string(),
        kind: "function".to_string(),
        path: path.to_string(),
        line,
        signature: None,
        score,
        match_type,
    }
}

// ---------------------------------------------------------------------------
// 1. Format & header
// ---------------------------------------------------------------------------

#[test]
fn call_graph_header_total_size_includes_bm25() {
    // 10 call-graph u64 fields (9.3) + 6 BM25 u64 fields (9.4) = 16 u64 = 128 bytes
    assert_eq!(CallGraphHeader::SIZE, 128);
}

#[test]
fn header_has_six_bm25_fields() {
    // Instantiate with zero values; all BM25 offset/len fields default to 0.
    let h = CallGraphHeader {
        call_edges_offset: 0,
        call_edges_len: 0,
        callers_fst_offset: 0,
        callers_fst_len: 0,
        callers_postings_offset: 0,
        callers_postings_len: 0,
        callees_fst_offset: 0,
        callees_fst_len: 0,
        callees_postings_offset: 0,
        callees_postings_len: 0,
        bm25_fst_offset: 0,
        bm25_fst_len: 0,
        bm25_postings_offset: 0,
        bm25_postings_len: 0,
        bm25_stats_offset: 0,
        bm25_stats_len: 0,
    };
    assert_eq!(h.bm25_fst_offset, 0);
    assert_eq!(h.bm25_fst_len, 0);
    assert_eq!(h.bm25_postings_offset, 0);
    assert_eq!(h.bm25_postings_len, 0);
    assert_eq!(h.bm25_stats_offset, 0);
    assert_eq!(h.bm25_stats_len, 0);
}

// ---------------------------------------------------------------------------
// 2. Writer + reader roundtrip — no BM25
// ---------------------------------------------------------------------------

#[test]
fn write_index_without_bm25_produces_empty_section() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("foo", 1, None)])];
    let reader = write_no_bm25(&tmp, &parsed);

    assert!(
        !reader.has_bm25(),
        "has_bm25 should be false when None passed"
    );
    assert!(reader.bm25_fst_bytes().is_empty());
    assert!(reader.bm25_posting_bytes().is_empty());
    assert!(reader.bm25_stats_bytes().is_empty());
}

// ---------------------------------------------------------------------------
// 3. Writer + reader roundtrip — with BM25
// ---------------------------------------------------------------------------

#[test]
fn write_index_with_bm25_persists_sections() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![
        make_file(
            "src/a.rs",
            vec![make_symbol("alpha", 1, Some("timeout retry"))],
        ),
        make_file(
            "src/b.rs",
            vec![make_symbol("beta", 1, Some("retry config"))],
        ),
        make_file("src/c.rs", vec![make_symbol("gamma", 1, Some("config"))]),
    ];

    let (fst, posts, stats) = build_small_bm25(
        3,
        &[
            (0, &["timeout", "retry"]),
            (1, &["retry", "config"]),
            (2, &["config"]),
        ],
    );

    let reader = write_with_bm25(&tmp, &parsed, &fst, &posts, &stats);

    assert!(
        reader.has_bm25(),
        "has_bm25 should be true when sections written"
    );
    assert!(
        !reader.bm25_fst_bytes().is_empty(),
        "FST bytes should be non-empty"
    );
    assert!(
        !reader.bm25_posting_bytes().is_empty(),
        "postings bytes should be non-empty"
    );
    assert!(
        !reader.bm25_stats_bytes().is_empty(),
        "stats bytes should be non-empty"
    );

    // Verify search returns results via Bm25Reader
    let bm25_reader = Bm25Reader::new(
        reader.bm25_fst_bytes(),
        reader.bm25_posting_bytes(),
        reader.bm25_stats_bytes(),
    )
    .unwrap();
    let hits = bm25_reader.search("timeout", 10);
    assert!(!hits.is_empty(), "timeout should be found in BM25 index");
    assert_eq!(hits[0].0, 0, "sym_idx 0 should be the timeout doc");
}

// ---------------------------------------------------------------------------
// 4. Pipeline end-to-end — pipeline::run emits BM25
// ---------------------------------------------------------------------------

#[test]
fn pipeline_run_emits_bm25() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    // Write a minimal Rust source file with a rare identifier in the body
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Handles payment with a configurable timeout.
pub fn handle_payment() {
    let timeout = 30;
    let retry_count = 3;
    let _ = timeout + retry_count;
}
"#,
    )
    .unwrap();

    // Write a minimal Cargo.toml so vex treats this as a project
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    vex::index::pipeline::run(
        &root,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let index_path = vex::util::config::index_path(&root);
    let reader = IndexReader::open(&index_path).unwrap();

    assert!(
        reader.has_bm25(),
        "pipeline::run should produce BM25 section"
    );

    let results = vex::search::bm25::search(&reader, "timeout", 10);
    assert!(
        !results.is_empty(),
        "BM25 search for 'timeout' should find handle_payment"
    );
    assert!(
        results.iter().any(|r| r.name == "handle_payment"),
        "handle_payment should be in BM25 results, got: {results:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Pipeline update preserves BM25
// ---------------------------------------------------------------------------

#[test]
fn pipeline_update_preserves_bm25() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "pub fn process_order() { let idempotency_key = 42; let _ = idempotency_key; }\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    vex::index::pipeline::run(
        &root,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
    // No file changes — update should be a no-op but keep BM25 intact
    vex::index::pipeline::update(
        &root,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let index_path = vex::util::config::index_path(&root);
    let reader = IndexReader::open(&index_path).unwrap();
    assert!(
        reader.has_bm25(),
        "has_bm25 should remain true after update with no changes"
    );
}

// ---------------------------------------------------------------------------
// 6. BM25 quality — finds rare body term when structural misses
// ---------------------------------------------------------------------------

#[test]
fn bm25_finds_rare_body_term_when_structural_misses() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    // Function name has no "singlestore" — only the body does (as a standalone identifier).
    // Note: comments are NOT extracted by the Rust parser into body_tokens; only
    // identifiers from the AST are captured. We use `singlestore` as a bare
    // let-binding identifier so tree-sitter captures it as an "identifier" node.
    fs::write(
        src_dir.join("lib.rs"),
        r#"
pub fn process_order() {
    let singlestore = 42u64;
    let _ = singlestore;
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    vex::index::pipeline::run(
        &root,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let index_path = vex::util::config::index_path(&root);
    let reader = IndexReader::open(&index_path).unwrap();

    // Structural search should NOT find "singlestore"
    let structural_hits = vex::search::structural::search_with_fuzzy(&reader, "singlestore", 10);
    assert!(
        structural_hits.is_empty(),
        "structural search should not find 'singlestore' — it's not in any symbol name"
    );

    // BM25 search SHOULD find it via body tokens (singlestore is a standalone identifier)
    assert!(reader.has_bm25(), "index must have BM25 section");
    let bm25_hits = vex::search::bm25::search(&reader, "singlestore", 10);
    assert!(
        !bm25_hits.is_empty(),
        "BM25 should find 'singlestore' in process_order body"
    );
    assert!(
        bm25_hits.iter().any(|r| r.name == "process_order"),
        "process_order should be in BM25 results, got: {bm25_hits:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. BM25 IDF — rare term ranks over common term
// ---------------------------------------------------------------------------

#[test]
fn bm25_idf_ranks_rare_over_common() {
    // 5 docs: all mention "config", only doc 0 mentions "timeout"
    let docs: &[(u32, &[&str])] = &[
        (0, &["config", "timeout"]),
        (1, &["config"]),
        (2, &["config"]),
        (3, &["config"]),
        (4, &["config"]),
    ];
    let (fst, posts, stats) = build_small_bm25(5, docs);
    let r = Bm25Reader::new(&fst, &posts, &stats).unwrap();

    // "timeout" hits only doc 0 (rare)
    let timeout_hits = r.search("timeout", 10);
    assert_eq!(timeout_hits.len(), 1);
    assert_eq!(timeout_hits[0].0, 0, "only doc 0 has timeout");

    // "config" hits all 5 docs; doc 0 has len=2, docs 1-4 have len=1
    // BM25 normalization should rank the shorter docs (len=1) higher
    let config_hits = r.search("config", 10);
    assert_eq!(config_hits.len(), 5, "all 5 docs contain config");
    // Docs 1-4 (len=1) should rank higher than doc 0 (len=2)
    let top_idx = config_hits[0].0;
    assert_ne!(
        top_idx, 0,
        "doc 0 (longer doc) should not be ranked first for 'config'"
    );
}

// ---------------------------------------------------------------------------
// 8. fuse3 — hybrid label when 2+ lists agree
// ---------------------------------------------------------------------------

#[test]
fn fuse3_hybrid_label_when_two_or_more_lists_agree() {
    // Doc X appears in structural + bm25, doc Y only in semantic
    let structural = vec![make_result("DocX", "x.rs", 1, 1.0, MatchType::Structural)];
    let bm25_list = vec![make_result("DocX", "x.rs", 1, 0.9, MatchType::Bm25)];
    let semantic = vec![make_result("DocY", "y.rs", 1, 0.8, MatchType::Semantic)];

    let fused = vex::search::fusion::fuse3(structural, bm25_list, semantic, 10);

    let doc_x = fused
        .iter()
        .find(|r| r.name == "DocX")
        .expect("DocX should be in fused results");
    let doc_y = fused
        .iter()
        .find(|r| r.name == "DocY")
        .expect("DocY should be in fused results");

    assert!(
        matches!(doc_x.match_type, MatchType::Hybrid),
        "DocX (in 2 lists) should be Hybrid, got {:?}",
        doc_x.match_type
    );
    assert!(
        matches!(doc_y.match_type, MatchType::Semantic),
        "DocY (only in 1 list) should keep Semantic, got {:?}",
        doc_y.match_type
    );
}

// ---------------------------------------------------------------------------
// 9. fuse3 — scores are descending
// ---------------------------------------------------------------------------

#[test]
fn fuse3_score_descending() {
    let structural = vec![
        make_result("A", "a.rs", 1, 1.0, MatchType::Structural),
        make_result("B", "b.rs", 1, 0.8, MatchType::Structural),
        make_result("C", "c.rs", 1, 0.6, MatchType::Structural),
    ];
    let bm25_list = vec![make_result("B", "b.rs", 1, 0.9, MatchType::Bm25)];
    let semantic = vec![make_result("C", "c.rs", 1, 0.7, MatchType::Semantic)];

    let fused = vex::search::fusion::fuse3(structural, bm25_list, semantic, 10);

    for w in fused.windows(2) {
        assert!(
            w[0].score >= w[1].score,
            "results should be sorted descending by score: {} (score={}) > {} (score={})",
            w[0].name,
            w[0].score,
            w[1].name,
            w[1].score
        );
    }
}

// ---------------------------------------------------------------------------
// 10. fuse3 — respects limit
// ---------------------------------------------------------------------------

#[test]
fn fuse3_respects_limit() {
    let structural: Vec<_> = (0..10)
        .map(|i| {
            make_result(
                &format!("S{i}"),
                "a.rs",
                i,
                1.0 - i as f64 * 0.05,
                MatchType::Structural,
            )
        })
        .collect();
    let bm25_list: Vec<_> = (0..10)
        .map(|i| {
            make_result(
                &format!("B{i}"),
                "b.rs",
                i,
                0.9 - i as f64 * 0.05,
                MatchType::Bm25,
            )
        })
        .collect();
    let semantic: Vec<_> = (0..10)
        .map(|i| {
            make_result(
                &format!("E{i}"),
                "c.rs",
                i,
                0.8 - i as f64 * 0.05,
                MatchType::Semantic,
            )
        })
        .collect();

    let fused = vex::search::fusion::fuse3(structural, bm25_list, semantic, 3);
    assert_eq!(
        fused.len(),
        3,
        "fuse3 with limit=3 must return at most 3 results"
    );
}

// ---------------------------------------------------------------------------
// 11. fuse_many — empty lists returns empty
// ---------------------------------------------------------------------------

#[test]
fn fuse_many_empty_lists_returns_empty() {
    let result = vex::search::fusion::fuse_many(vec![vec![], vec![], vec![]], 10);
    assert!(
        result.is_empty(),
        "fuse_many of empty lists should return empty"
    );
}

// ---------------------------------------------------------------------------
// 12. BM25 search adapter — empty when no section
// ---------------------------------------------------------------------------

#[test]
fn search_bm25_adapter_empty_when_no_section() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("foo", 1, None)])];
    let reader = write_no_bm25(&tmp, &parsed);

    assert!(!reader.has_bm25());
    let results = vex::search::bm25::search(&reader, "anything", 10);
    assert!(
        results.is_empty(),
        "bm25::search on index without BM25 section should return empty"
    );
}

// ---------------------------------------------------------------------------
// 13. BM25 search adapter — tags MatchType::Bm25
// ---------------------------------------------------------------------------

#[test]
fn search_bm25_adapter_tags_match_type() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "pub fn compute_timeout() { let singlestore = 1; let _ = singlestore; }\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    vex::index::pipeline::run(
        &root,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let index_path = vex::util::config::index_path(&root);
    let reader = IndexReader::open(&index_path).unwrap();
    assert!(reader.has_bm25());

    let results = vex::search::bm25::search(&reader, "singlestore", 10);
    assert!(!results.is_empty(), "should find singlestore in BM25");
    for r in &results {
        assert!(
            matches!(r.match_type, MatchType::Bm25),
            "all BM25 adapter results must have MatchType::Bm25, got {:?}",
            r.match_type
        );
    }
}

// ---------------------------------------------------------------------------
// 14. Tokenization — tokenize_document does NOT drop stopwords
// ---------------------------------------------------------------------------

#[test]
fn tokenize_document_does_not_drop_stopwords() {
    // Documents keep stopwords at index time; only query time filters them
    let tokens = tokenize_document("the timeout");
    assert!(
        tokens.contains(&"the".to_string()),
        "tokenize_document should keep 'the' (no stopword filtering at index time): {tokens:?}"
    );
    assert!(
        tokens.contains(&"timeout".to_string()),
        "tokenize_document should keep 'timeout': {tokens:?}"
    );
}

// ---------------------------------------------------------------------------
// 15. Tokenization — tokenize_query drops short tokens and stopwords
// ---------------------------------------------------------------------------

#[test]
fn tokenize_query_drops_short_and_stopwords() {
    // All stopwords → empty
    assert_eq!(tokenize_query("the a is"), Vec::<String>::new());
    // Short single-char tokens dropped
    assert_eq!(tokenize_query("x y z"), Vec::<String>::new());
    // Normal tokens preserved, casing lowered
    let result = tokenize_query("Handle Timeout Retry");
    assert!(result.contains(&"handle".to_string()), "got: {result:?}");
    assert!(result.contains(&"timeout".to_string()), "got: {result:?}");
    assert!(result.contains(&"retry".to_string()), "got: {result:?}");
    // Unicode multi-byte: should not panic
    let _ = tokenize_query("naïve résumé");
}

// ---------------------------------------------------------------------------
// 16. v3 index has no BM25 — has_bm25() == false without panic
// ---------------------------------------------------------------------------

#[test]
fn v3_index_has_no_bm25() {
    // write_index calls write_index_full which calls write_index_with_call_graph(None)
    // and writes VERSION=4 with zero BM25 lens — has_bm25 returns false.
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("bar", 1, None)])];
    let path = tmp.path().join("index.vex");
    vex::store::writer::write_index(&parsed, &path).unwrap();
    let reader = IndexReader::open(&path).unwrap();
    // Should not panic and should return false
    assert!(
        !reader.has_bm25(),
        "write_index (no BM25) should have has_bm25 == false"
    );
}

// ---------------------------------------------------------------------------
// 17. BM25 constants match spec (K1=1.2, B=0.75)
// ---------------------------------------------------------------------------

#[test]
fn bm25_constants_match_spec() {
    assert!(
        (K1 - 1.2_f32).abs() < f32::EPSILON * 4.0,
        "K1 should be 1.2, got {K1}"
    );
    assert!(
        (B - 0.75_f32).abs() < f32::EPSILON * 4.0,
        "B should be 0.75, got {B}"
    );
}

// ---------------------------------------------------------------------------
// 18. Bm25IndexBuilder — empty index produces valid (parseable) sections
// ---------------------------------------------------------------------------

#[test]
fn bm25_empty_builder_produces_parseable_sections() {
    let b = Bm25IndexBuilder::new(0);
    let (fst, posts, stats) = b.build().unwrap();
    // An empty index should still produce a valid FST (empty map) and stats section
    let r = Bm25Reader::new(&fst, &posts, &stats);
    assert!(r.is_ok(), "Bm25Reader::new on empty index should succeed");
    let r = r.unwrap();
    assert!(r.search("anything", 10).is_empty());
}

// ---------------------------------------------------------------------------
// 19. Header::SIZE unchanged at 144 bytes (regression guard)
// ---------------------------------------------------------------------------

#[test]
fn header_size_is_144() {
    // Guard: format extension must not modify the base Header struct.
    assert_eq!(Header::SIZE, 144);
}
