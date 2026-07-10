use std::path::PathBuf;
use tempfile::TempDir;

use vex::index::symbols::{ParsedFile, ParsedRef, ParsedSymbol, SymbolKind};
use vex::search::rerank::RerankContext;
use vex::search::{fusion, MatchType, SearchResult};
use vex::store::format::{Header, MAGIC};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn make_symbol(name: &str, kind: SymbolKind, line: usize) -> ParsedSymbol {
    ParsedSymbol {
        name: name.to_string(),
        kind,
        line,
        signature: Some(format!("fn {name}()")),
        doc: None,
        body_tokens: None,
    }
}

fn make_parsed_file(path: &str, symbols: Vec<ParsedSymbol>) -> ParsedFile {
    ParsedFile {
        path: path.to_string(),
        symbols,
        refs: Vec::new(),
        call_edges: Vec::new(),
        bound_refs: Vec::new(),
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
        trigram_bloom: None,
    }
}

// --- Binary format roundtrip tests ---

#[test]
fn binary_format_roundtrip_preserves_all_fields() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![
        make_parsed_file(
            "src/main.rs",
            vec![
                make_symbol("main", SymbolKind::Function, 1),
                make_symbol("Config", SymbolKind::Struct, 10),
                make_symbol("MAX_SIZE", SymbolKind::Constant, 20),
            ],
        ),
        make_parsed_file(
            "src/lib.rs",
            vec![
                make_symbol("add", SymbolKind::Function, 1),
                make_symbol("MyTrait", SymbolKind::Trait, 5),
            ],
        ),
    ];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    assert_eq!(reader.symbol_count(), 5);

    // Verify each symbol
    let rec = reader.symbol(0).unwrap();
    assert_eq!(reader.read_string(rec.name_offset), "main");
    assert_eq!(rec.kind, SymbolKind::Function as u8);
    assert_eq!(rec.line, 1);
    assert_eq!(reader.read_string(rec.file_offset), "src/main.rs");

    let rec = reader.symbol(1).unwrap();
    assert_eq!(reader.read_string(rec.name_offset), "Config");
    assert_eq!(rec.kind, SymbolKind::Struct as u8);
    assert_eq!(rec.line, 10);

    let rec = reader.symbol(4).unwrap();
    assert_eq!(reader.read_string(rec.name_offset), "MyTrait");
    assert_eq!(rec.kind, SymbolKind::Trait as u8);
    assert_eq!(reader.read_string(rec.file_offset), "src/lib.rs");
}

#[test]
fn binary_format_header_validates_magic() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    // Write a file with bad magic
    let mut header_bytes = vec![0u8; Header::SIZE];
    header_bytes[0..4].copy_from_slice(b"BAAD");
    std::fs::write(&index_path, &header_bytes).unwrap();

    match vex::store::reader::IndexReader::open(&index_path) {
        Ok(_) => panic!("should fail with bad magic"),
        Err(e) => assert!(
            e.to_string().contains("corrupted (bad magic)"),
            "unexpected error: {e}"
        ),
    }
}

#[test]
fn binary_format_rejects_truncated_file() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    // Write a file smaller than Header::SIZE
    std::fs::write(&index_path, b"VEX").unwrap();

    match vex::store::reader::IndexReader::open(&index_path) {
        Ok(_) => panic!("should fail with truncated file"),
        Err(e) => assert!(e.to_string().contains("too small"), "unexpected error: {e}"),
    }
}

#[test]
fn binary_format_rejects_wrong_version() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let mut header_bytes = vec![0u8; Header::SIZE];
    header_bytes[0..4].copy_from_slice(MAGIC);
    // Set version to 99
    header_bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
    std::fs::write(&index_path, &header_bytes).unwrap();

    match vex::store::reader::IndexReader::open(&index_path) {
        Ok(_) => panic!("should fail with wrong version"),
        Err(e) => assert!(
            e.to_string().contains("version mismatch"),
            "unexpected error: {e}"
        ),
    }
}

#[test]
fn symbol_out_of_bounds_returns_none() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![make_parsed_file(
        "test.rs",
        vec![make_symbol("foo", SymbolKind::Function, 1)],
    )];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    assert!(reader.symbol(0).is_some());
    assert!(reader.symbol(1).is_none());
    assert!(reader.symbol(999).is_none());
}

// --- Vector roundtrip ---

#[test]
fn vector_roundtrip_preserves_data() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![make_parsed_file(
        "test.rs",
        vec![
            make_symbol("foo", SymbolKind::Function, 1),
            make_symbol("bar", SymbolKind::Function, 5),
        ],
    )];

    // Create 384-dim vectors
    let vectors = vec![vec![0.1f32; 384], vec![0.9f32; 384]];

    vex::store::writer::write_index_full(&files, &vectors, 384, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    assert!(reader.has_vectors());

    let rec0 = reader.symbol(0).unwrap();
    let vec0 = reader.vector(rec0.vector_index).unwrap();
    assert_eq!(vec0.len(), 384);
    assert!((vec0[0] - 0.1).abs() < 1e-6);

    let rec1 = reader.symbol(1).unwrap();
    let vec1 = reader.vector(rec1.vector_index).unwrap();
    assert!((vec1[0] - 0.9).abs() < 1e-6);
}

#[test]
fn index_without_vectors_reports_no_vectors() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![make_parsed_file(
        "test.rs",
        vec![make_symbol("foo", SymbolKind::Function, 1)],
    )];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    assert!(!reader.has_vectors());
}

// --- Fuzzy search ---

#[test]
fn fuzzy_search_finds_close_matches() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![make_parsed_file(
        "test.rs",
        vec![
            make_symbol("PaymentService", SymbolKind::Class, 1),
            make_symbol("PaymentGateway", SymbolKind::Trait, 10),
            make_symbol("UserRepository", SymbolKind::Class, 20),
        ],
    )];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    // Exact match
    let results = vex::search::structural::search_with_fuzzy(&reader, "PaymentService", 10);
    assert!(!results.is_empty());
    assert_eq!(results[0].name, "PaymentService");

    // Prefix match: "Payment" should find both Payment* symbols
    let results = vex::search::structural::search_with_fuzzy(&reader, "Payment", 10);
    assert!(results.len() >= 2);

    // Fuzzy match: typo "PaymentServce" should find PaymentService via Levenshtein
    let results = vex::search::structural::search_with_fuzzy(&reader, "PaymentServce", 10);
    assert!(
        !results.is_empty(),
        "fuzzy search should find PaymentService despite typo"
    );
}

// --- Refs FST ---

#[test]
fn refs_roundtrip_and_search() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![ParsedFile {
        path: "main.rs".to_string(),
        symbols: vec![make_symbol("main", SymbolKind::Function, 1)],
        refs: vec![
            ParsedRef {
                name: "Config".to_string(),
                line: 3,
                context: Some("use crate::Config;".to_string()),
            },
            ParsedRef {
                name: "Config".to_string(),
                line: 10,
                context: Some("let cfg = Config::new();".to_string()),
            },
            ParsedRef {
                name: "Logger".to_string(),
                line: 5,
                context: Some("use log::Logger;".to_string()),
            },
        ],
        call_edges: Vec::new(),
        bound_refs: Vec::new(),
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
        trigram_bloom: None,
    }];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    assert!(reader.has_refs());
    let ref_reader = reader.ref_reader().unwrap();

    let config_refs = ref_reader.find("Config");
    assert_eq!(config_refs.len(), 2);
    assert_eq!(config_refs[0].line, 3);
    assert_eq!(config_refs[1].line, 10);

    let logger_refs = ref_reader.find("Logger");
    assert_eq!(logger_refs.len(), 1);

    let missing_refs = ref_reader.find("NonExistent");
    assert!(missing_refs.is_empty());
}

// --- File table ---

#[test]
fn file_table_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![
        make_parsed_file(
            "src/main.rs",
            vec![make_symbol("main", SymbolKind::Function, 1)],
        ),
        make_parsed_file(
            "src/lib.rs",
            vec![make_symbol("add", SymbolKind::Function, 1)],
        ),
        make_parsed_file(
            "src/util/helper.rs",
            vec![make_symbol("help", SymbolKind::Function, 1)],
        ),
    ];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    let paths = reader.file_paths();
    assert_eq!(paths.len(), 3);
    assert!(paths.contains(&"src/main.rs".to_string()));
    assert!(paths.contains(&"src/lib.rs".to_string()));
    assert!(paths.contains(&"src/util/helper.rs".to_string()));
}

// --- Multi-language parsing ---

#[test]
fn parse_all_fixture_languages() {
    let fixtures = fixtures_dir();
    let expected = vec![
        (
            "sample.rs",
            vex::parse::language::Language::Rust,
            vec!["PaymentService", "PaymentGateway"],
        ),
        (
            "sample.py",
            vex::parse::language::Language::Python,
            vec!["UserRepository"],
        ),
        (
            "sample.go",
            vex::parse::language::Language::Go,
            vec!["InvoiceService"],
        ),
        (
            "sample.kt",
            vex::parse::language::Language::Kotlin,
            vec!["PaymentProcessor"],
        ),
        (
            "sample.ts",
            vex::parse::language::Language::TypeScript,
            vec!["UserService"],
        ),
    ];

    for (filename, lang, expected_symbols) in expected {
        let path = fixtures.join(filename);
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed = vex::parse::parse_file(filename, &content, lang).unwrap();

        for expected_name in &expected_symbols {
            assert!(
                parsed.symbols.iter().any(|s| s.name == *expected_name),
                "{filename}: expected symbol '{expected_name}' not found. Got: {:?}",
                parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }
    }
}

// --- Reranking ---

#[test]
fn rerank_exact_match_beats_partial() {
    let results = vec![
        SearchResult {
            name: "ConfigManager".to_string(),
            kind: "class".to_string(),
            path: "src/config.rs".to_string(),
            line: 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
        SearchResult {
            name: "Config".to_string(),
            kind: "struct".to_string(),
            path: "src/config.rs".to_string(),
            line: 20,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
    ];

    let ctx = vex::search::rerank::RerankContext::default();
    let ranked = vex::search::rerank::rerank("Config", &ctx, results);
    assert_eq!(ranked[0].name, "Config", "exact match should rank first");
}

#[test]
fn rerank_preserves_all_results() {
    let results: Vec<SearchResult> = (0..20)
        .map(|i| SearchResult {
            name: format!("Symbol{i}"),
            kind: "function".to_string(),
            path: format!("src/file{i}.rs"),
            line: i + 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        })
        .collect();

    let ctx = vex::search::rerank::RerankContext::default();
    let ranked = vex::search::rerank::rerank("Symbol5", &ctx, results);
    assert_eq!(ranked.len(), 20, "rerank should not drop results");
}

#[test]
fn rerank_kind_hint_and_context_path() {
    let results = vec![
        SearchResult {
            name: "Config".to_string(),
            kind: "class".to_string(),
            path: "src/auth/config.rs".to_string(),
            line: 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
        SearchResult {
            name: "Config".to_string(),
            kind: "struct".to_string(),
            path: "src/billing/config.rs".to_string(),
            line: 10,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
    ];

    let ctx = vex::search::rerank::RerankContext {
        kind_hints: vec![vex::search::rerank::KindSelector::Symbol(
            vex::index::symbols::SymbolKind::Struct,
        )],
        context_path: Some("src/billing/gateway.rs"),
    };
    let ranked = vex::search::rerank::rerank("Config", &ctx, results);
    assert_eq!(
        ranked[0].path, "src/billing/config.rs",
        "struct in same dir as context should rank first"
    );
}

// --- Reranking stress ---

#[test]
fn rerank_single_nan_result_sanitized() {
    let results = vec![SearchResult {
        name: "Foo".to_string(),
        kind: "function".to_string(),
        path: "src/a.rs".to_string(),
        line: 1,
        signature: None,
        score: f64::NAN,
        match_type: MatchType::Structural,
    }];
    let ctx = RerankContext::default();
    let ranked = vex::search::rerank::rerank("Foo", &ctx, results);
    assert_eq!(ranked.len(), 1);
    assert!(
        !ranked[0].score.is_nan(),
        "single-result NaN must be sanitized"
    );
}

#[test]
fn rerank_score_zero_no_panic() {
    let results = vec![
        SearchResult {
            name: "Alpha".to_string(),
            kind: "function".to_string(),
            path: "src/a.rs".to_string(),
            line: 1,
            signature: None,
            score: 0.0,
            match_type: MatchType::Structural,
        },
        SearchResult {
            name: "Beta".to_string(),
            kind: "function".to_string(),
            path: "src/b.rs".to_string(),
            line: 2,
            signature: None,
            score: 0.0,
            match_type: MatchType::Structural,
        },
    ];

    let ctx = RerankContext::default();
    let ranked = vex::search::rerank::rerank("Alpha", &ctx, results);

    assert_eq!(ranked.len(), 2, "rerank should not drop zero-score results");
    for r in &ranked {
        assert!(
            r.score >= 0.0,
            "zero-score result should not produce negative score after rerank, got {}",
            r.score
        );
    }
}

#[test]
fn rerank_score_nan_no_propagation() {
    // One result has NaN score — rerank must not panic, and ideally sanitises it.
    // If this assertion fails it documents a bug: NaN propagates through score *= boost.
    let results = vec![
        SearchResult {
            name: "Good".to_string(),
            kind: "function".to_string(),
            path: "src/a.rs".to_string(),
            line: 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
        SearchResult {
            name: "Bad".to_string(),
            kind: "function".to_string(),
            path: "src/b.rs".to_string(),
            line: 2,
            signature: None,
            score: f64::NAN,
            match_type: MatchType::Structural,
        },
    ];

    let ctx = RerankContext::default();
    // Must not panic regardless of NaN
    let ranked = vex::search::rerank::rerank("Good", &ctx, results);

    assert_eq!(ranked.len(), 2, "rerank should not drop NaN-score results");
    for r in &ranked {
        assert!(
            !r.score.is_nan(),
            "NaN score should not propagate after rerank for result '{}' — this is a bug",
            r.name
        );
    }
}

#[test]
fn rerank_score_max_no_infinity() {
    // f64::MAX * any boost factor > 1.0 overflows to infinity.
    // Rerank should not produce infinite scores (or at least not panic).
    let results = vec![
        SearchResult {
            name: "Overflow".to_string(),
            kind: "function".to_string(),
            path: "src/a.rs".to_string(),
            line: 1,
            signature: None,
            score: f64::MAX,
            match_type: MatchType::Structural,
        },
        SearchResult {
            name: "Normal".to_string(),
            kind: "function".to_string(),
            path: "src/b.rs".to_string(),
            line: 2,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
    ];

    // Full context to exercise all boost paths
    let ctx = RerankContext {
        kind_hints: vec![vex::search::rerank::KindSelector::Symbol(
            SymbolKind::Function,
        )],
        context_path: Some("src/a.rs"),
    };
    let ranked = vex::search::rerank::rerank("Overflow", &ctx, results);

    assert_eq!(ranked.len(), 2, "rerank should not drop max-score results");
    for r in &ranked {
        assert!(
            !r.score.is_infinite(),
            "score should not overflow to infinity for result '{}', got {}",
            r.name,
            r.score
        );
    }
}

#[test]
fn rerank_large_result_set() {
    let results: Vec<SearchResult> = (0..10_000)
        .map(|i| SearchResult {
            name: format!("Symbol{i}"),
            kind: "function".to_string(),
            path: format!("src/mod{}/file{i}.rs", i % 100),
            line: 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        })
        .collect();

    let ctx = RerankContext::default();
    let ranked = vex::search::rerank::rerank("Symbol5000", &ctx, results);

    assert_eq!(ranked.len(), 10_000, "rerank must not drop any results");
    assert_eq!(
        ranked[0].name, "Symbol5000",
        "exact name match should rank first in 10k result set"
    );
}

#[test]
fn rerank_empty_context_path() {
    let results = vec![
        SearchResult {
            name: "Foo".to_string(),
            kind: "function".to_string(),
            path: "src/a.rs".to_string(),
            line: 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
        SearchResult {
            name: "Bar".to_string(),
            kind: "function".to_string(),
            path: "src/b.rs".to_string(),
            line: 2,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
    ];

    // context_path: Some("") — dir_of("") returns "" — should not panic
    let ctx = RerankContext {
        kind_hints: Vec::new(),
        context_path: Some(""),
    };
    let ranked = vex::search::rerank::rerank("Foo", &ctx, results);
    assert_eq!(ranked.len(), 2, "empty context_path must not drop results");
}

#[test]
fn rerank_root_context_path() {
    let results = vec![
        SearchResult {
            name: "Foo".to_string(),
            kind: "function".to_string(),
            path: "src/a.rs".to_string(),
            line: 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
        SearchResult {
            name: "Bar".to_string(),
            kind: "function".to_string(),
            path: "src/b.rs".to_string(),
            line: 2,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
    ];

    // context_path: Some("/") — splits to ["", ""] — should not panic
    let ctx = RerankContext {
        kind_hints: Vec::new(),
        context_path: Some("/"),
    };
    let ranked = vex::search::rerank::rerank("Foo", &ctx, results);
    assert_eq!(ranked.len(), 2, "root context_path must not drop results");
}

#[test]
fn rerank_single_component_path() {
    let results = vec![
        SearchResult {
            name: "Foo".to_string(),
            kind: "function".to_string(),
            path: "src/a.rs".to_string(),
            line: 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
        SearchResult {
            name: "Bar".to_string(),
            kind: "function".to_string(),
            path: "src/b.rs".to_string(),
            line: 2,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
    ];

    // context_path: Some("file.rs") — no directory separator — dir_of returns "" — no crash
    let ctx = RerankContext {
        kind_hints: Vec::new(),
        context_path: Some("file.rs"),
    };
    let ranked = vex::search::rerank::rerank("Foo", &ctx, results);
    assert_eq!(
        ranked.len(),
        2,
        "single-component context_path must not drop results"
    );
}

// --- RRF Fusion ---

#[test]
fn fusion_marks_hybrid_correctly() {
    let structural = vec![SearchResult {
        name: "process".to_string(),
        kind: "function".to_string(),
        path: "src/main.rs".to_string(),
        line: 1,
        signature: None,
        score: 1.0,
        match_type: MatchType::Structural,
    }];
    let semantic = vec![SearchResult {
        name: "process".to_string(),
        kind: "function".to_string(),
        path: "src/main.rs".to_string(),
        line: 1,
        signature: None,
        score: 0.9,
        match_type: MatchType::Semantic,
    }];

    let fused = fusion::fuse(structural, semantic, 10);
    assert_eq!(fused.len(), 1);
    assert!(matches!(fused[0].match_type, MatchType::Hybrid));
}

#[test]
fn fusion_deduplicates_by_path_name_line() {
    let structural = vec![
        SearchResult {
            name: "foo".to_string(),
            kind: "function".to_string(),
            path: "a.rs".to_string(),
            line: 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        },
        SearchResult {
            name: "bar".to_string(),
            kind: "function".to_string(),
            path: "b.rs".to_string(),
            line: 5,
            signature: None,
            score: 0.8,
            match_type: MatchType::Structural,
        },
    ];
    let semantic = vec![SearchResult {
        name: "foo".to_string(),
        kind: "function".to_string(),
        path: "a.rs".to_string(),
        line: 1,
        signature: None,
        score: 0.9,
        match_type: MatchType::Semantic,
    }];

    let fused = fusion::fuse(structural, semantic, 10);
    // foo appears in both → deduplicated to 1 entry; bar only structural → 1 entry
    assert_eq!(fused.len(), 2);
}

// --- SymbolKind TryFrom edge cases ---

#[test]
fn symbol_kind_all_variants_have_distinct_u8() {
    let all = [
        SymbolKind::Function,
        SymbolKind::Method,
        SymbolKind::Struct,
        SymbolKind::Class,
        SymbolKind::Interface,
        SymbolKind::Trait,
        SymbolKind::Enum,
        SymbolKind::TypeAlias,
        SymbolKind::Impl,
        SymbolKind::Constant,
        SymbolKind::Property,
        SymbolKind::Package,
        SymbolKind::Heading,
    ];
    let mut seen = std::collections::HashSet::new();
    for kind in all {
        let val = kind as u8;
        assert!(seen.insert(val), "duplicate u8 value {val} for {kind:?}");
        // Roundtrip
        assert_eq!(SymbolKind::try_from(val).unwrap(), kind);
    }
}

// --- Incremental update (unit-level) ---

#[test]
fn incremental_update_reuses_unchanged_symbols() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();

    // Write initial source files
    std::fs::write(
        project_dir.join("src/stable.rs"),
        "pub fn stable_func() {}\npub struct StableStruct {}",
    )
    .unwrap();
    std::fs::write(project_dir.join("src/changing.rs"), "pub fn old_func() {}").unwrap();

    // Full index
    let (count, _) = vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
    assert!(count >= 3, "expected at least 3 symbols, got {count}");

    // Verify initial search
    let index_path = vex::util::config::index_path(&project_dir.canonicalize().unwrap());
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();
    let results = vex::search::structural::search_with_fuzzy(&reader, "stable_func", 10);
    assert!(!results.is_empty(), "stable_func should be found");
    let results = vex::search::structural::search_with_fuzzy(&reader, "old_func", 10);
    assert!(!results.is_empty(), "old_func should be found");

    // Modify one file
    std::fs::write(
        project_dir.join("src/changing.rs"),
        "pub fn new_func() {}\npub fn another_func() {}",
    )
    .unwrap();

    // Incremental update
    let (total, changed, deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
    assert_eq!(changed, 1, "only one file changed");
    assert_eq!(deleted, 0);
    assert!(total >= 4, "expected at least 4 symbols, got {total}");

    // Verify: stable symbols still found, old_func gone, new symbols present
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    let results = vex::search::structural::search_with_fuzzy(&reader, "stable_func", 10);
    assert!(
        !results.is_empty(),
        "stable_func should survive incremental update"
    );

    let results = vex::search::structural::search_with_fuzzy(&reader, "StableStruct", 10);
    assert!(
        !results.is_empty(),
        "StableStruct should survive incremental update"
    );

    let results = vex::search::structural::search_with_fuzzy(&reader, "new_func", 10);
    assert!(!results.is_empty(), "new_func should appear after update");

    let results = vex::search::structural::search_with_fuzzy(&reader, "old_func", 10);
    assert!(results.is_empty(), "old_func should be gone after update");
}

#[test]
fn incremental_update_handles_deleted_files() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();

    std::fs::write(project_dir.join("src/keep.rs"), "pub fn keep_me() {}").unwrap();
    std::fs::write(project_dir.join("src/remove.rs"), "pub fn remove_me() {}").unwrap();

    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    // Delete a file
    std::fs::remove_file(project_dir.join("src/remove.rs")).unwrap();

    let (total, _changed, deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
    assert_eq!(deleted, 1);
    assert!(total >= 1);

    let index_path = vex::util::config::index_path(&project_dir.canonicalize().unwrap());
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    let results = vex::search::structural::search_with_fuzzy(&reader, "keep_me", 10);
    assert!(!results.is_empty());

    let results = vex::search::structural::search_with_fuzzy(&reader, "remove_me", 10);
    assert!(results.is_empty(), "removed symbol should not be found");
}

#[test]
fn incremental_update_noop_when_nothing_changed() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();

    std::fs::write(project_dir.join("src/main.rs"), "pub fn main() {}").unwrap();

    let (count, _) = vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let (total, changed, deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
    assert_eq!(changed, 0);
    assert_eq!(deleted, 0);
    assert_eq!(total, count);
}

// --- String pool deduplication ---

#[test]
fn string_pool_deduplicates_paths() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    // Two symbols in the same file — path should be deduplicated in string pool
    let files = vec![make_parsed_file(
        "src/main.rs",
        vec![
            make_symbol("foo", SymbolKind::Function, 1),
            make_symbol("bar", SymbolKind::Function, 5),
        ],
    )];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    let rec0 = reader.symbol(0).unwrap();
    let rec1 = reader.symbol(1).unwrap();

    // Both symbols should reference the same string offset for file path
    assert_eq!(rec0.file_offset, rec1.file_offset);
    assert_eq!(reader.read_string(rec0.file_offset), "src/main.rs");
}

// --- 11.1.3b: reference_edges section ---

#[test]
fn ref_edges_roundtrip_module_symbols() {
    use vex::parse::scope::{BindTarget, BoundRef, RefKind};

    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    // One file with two top-level symbols (`Payment_Type` at line 1,
    // `caller_fn` at line 2) and two bound refs targeting them by
    // file-local idx.
    let files = vec![ParsedFile {
        path: "main.rs".to_string(),
        symbols: vec![
            make_symbol("Payment_Type", SymbolKind::Struct, 1),
            make_symbol("caller_fn", SymbolKind::Function, 2),
        ],
        refs: Vec::new(),
        call_edges: Vec::new(),
        bound_refs: vec![
            BoundRef {
                name: "Payment_Type".into(),
                line: 3,
                col: 12,
                target: BindTarget::ModuleSymbol(0),
                kind: RefKind::Type,
            },
            BoundRef {
                name: "Payment_Type".into(),
                line: 7,
                col: 5,
                target: BindTarget::ModuleSymbol(0),
                kind: RefKind::Type,
            },
            BoundRef {
                name: "caller_fn".into(),
                line: 10,
                col: 4,
                target: BindTarget::ModuleSymbol(1),
                kind: RefKind::Value,
            },
            // Locals & Unresolved & Imported get dropped in 11.1.3b — only
            // ModuleSymbol survives until cross-file resolution lands in
            // 11.1.3c.
            BoundRef {
                name: "ghost".into(),
                line: 20,
                col: 1,
                target: BindTarget::Unresolved,
                kind: RefKind::Value,
            },
        ],
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
        trigram_bloom: None,
    }];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();
    assert!(reader.has_ref_edges(), "v5 index must carry ref edges now");

    let edges_for_0 = reader.find_ref_edges_by_symbol(0);
    assert_eq!(edges_for_0.len(), 2, "two refs target Payment_Type (idx 0)");
    assert_eq!(edges_for_0[0].line, 3);
    assert_eq!(edges_for_0[1].line, 7);

    let edges_for_1 = reader.find_ref_edges_by_symbol(1);
    assert_eq!(edges_for_1.len(), 1, "one ref targets caller_fn (idx 1)");
    assert_eq!(edges_for_1[0].line, 10);
    assert_eq!(edges_for_1[0].column(), 4);
    assert_eq!(edges_for_1[0].ref_kind_bits(), RefKind::Value as u8);

    let unknown = reader.find_ref_edges_by_symbol(99);
    assert!(unknown.is_empty(), "missing key returns empty vec");
}

#[test]
fn ref_edges_resolve_imported_use_path_cross_file() {
    use vex::parse::scope::{BindTarget, BoundRef, RefKind, UsePath};

    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    // File A defines the symbol; File B imports + uses it. The binder
    // in File B can't see File A's symbol table, so its BoundRef is
    // `Imported(UsePath{["crate","Important_Type"]})`. The writer's
    // Pass-2 cross-file resolution (11.1.3c) must rewrite that into a
    // global ModuleSymbol idx pointing at File A's record.
    let files = vec![
        ParsedFile {
            path: "a.rs".into(),
            symbols: vec![make_symbol("Important_Type", SymbolKind::Struct, 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
        ParsedFile {
            path: "b.rs".into(),
            symbols: vec![make_symbol("caller_fn", SymbolKind::Function, 5)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![BoundRef {
                name: "Important_Type".into(),
                line: 7,
                col: 12,
                target: BindTarget::Imported(UsePath {
                    segments: vec!["crate".into(), "Important_Type".into()],
                }),
                kind: RefKind::Type,
            }],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
    ];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    // global idx 0 = Important_Type (from file A); 1 = caller_fn (file B).
    let edges = reader.find_ref_edges_by_symbol(0);
    assert_eq!(
        edges.len(),
        1,
        "the b.rs `use crate::Important_Type;` site must resolve to a.rs's Important_Type",
    );
    assert_eq!(edges[0].line, 7);
    assert_eq!(edges[0].from_file_id, 1, "ref came from b.rs (file_id 1)");
}

#[test]
fn ref_edges_same_name_three_files_first_wins() {
    use vex::parse::scope::{BindTarget, BoundRef, RefKind, UsePath};

    // Pin the documented "first hit on ambiguity" behaviour. Three
    // files all define `Common_Name`; a fourth has an Imported ref
    // whose use_path ends in `Common_Name`. The writer must resolve
    // to the FIRST defining file's symbol idx — anything else would
    // be load-order-sensitive corruption.
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![
        ParsedFile {
            path: "a.rs".into(),
            symbols: vec![make_symbol("Common_Name", SymbolKind::Struct, 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
        ParsedFile {
            path: "b.rs".into(),
            symbols: vec![make_symbol("Common_Name", SymbolKind::Struct, 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
        ParsedFile {
            path: "c.rs".into(),
            symbols: vec![make_symbol("Common_Name", SymbolKind::Struct, 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
        ParsedFile {
            path: "user.rs".into(),
            symbols: vec![make_symbol("user_fn", SymbolKind::Function, 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![BoundRef {
                name: "Common_Name".into(),
                line: 5,
                col: 1,
                target: BindTarget::Imported(UsePath {
                    segments: vec!["crate".into(), "Common_Name".into()],
                }),
                kind: RefKind::Type,
            }],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
    ];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    // Global symbol order: a.rs:Common_Name=0, b.rs:Common_Name=1,
    // c.rs:Common_Name=2, user.rs:user_fn=3. Pass-2 takes the first
    // hit, so the import resolves to idx 0 (a.rs).
    let to_0 = reader.find_ref_edges_by_symbol(0);
    let to_1 = reader.find_ref_edges_by_symbol(1);
    let to_2 = reader.find_ref_edges_by_symbol(2);
    assert_eq!(to_0.len(), 1, "first-defining file must capture the edge");
    assert!(
        to_1.is_empty(),
        "second file must not see the ambiguous ref"
    );
    assert!(to_2.is_empty(), "third file must not see the ambiguous ref");
    assert_eq!(to_0[0].line, 5);
}

#[test]
fn ref_edges_drop_imported_when_use_path_unresolvable() {
    use vex::parse::scope::{BindTarget, BoundRef, RefKind, UsePath};

    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    // The imported name `Outside_Crate_Type` exists in no file's
    // symbols → cross-file resolution must skip the ref. The legacy
    // refs FST keeps catching it for `vex usages` (no --strict).
    let files = vec![ParsedFile {
        path: "b.rs".into(),
        symbols: vec![make_symbol("caller_fn", SymbolKind::Function, 1)],
        refs: vec![],
        call_edges: vec![],
        bound_refs: vec![BoundRef {
            name: "Outside_Crate_Type".into(),
            line: 3,
            col: 1,
            target: BindTarget::Imported(UsePath {
                segments: vec!["external".into(), "Outside_Crate_Type".into()],
            }),
            kind: RefKind::Type,
        }],
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
        trigram_bloom: None,
    }];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();
    // caller_fn is sym_idx 0; no other symbol exists.
    let edges = reader.find_ref_edges_by_symbol(0);
    assert!(
        edges.is_empty(),
        "external symbol must not produce a fake edge against an unrelated local symbol",
    );
}

#[test]
fn ref_edges_ts_imported_cross_file() {
    // Pass-2 cross-file resolution must work for TypeScript named
    // imports the same way it works for Rust `use`. A regression in
    // the writer that breaks the Rust path would surface there; this
    // pins the TS path independently.
    use vex::parse::scope::{BindTarget, BoundRef, RefKind, UsePath};

    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![
        ParsedFile {
            path: "ext.ts".into(),
            symbols: vec![make_symbol("someExtFn", SymbolKind::Function, 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
        ParsedFile {
            path: "user.ts".into(),
            symbols: vec![make_symbol("caller_fn", SymbolKind::Function, 3)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![BoundRef {
                name: "someExtFn".into(),
                line: 5,
                col: 11,
                target: BindTarget::Imported(UsePath {
                    segments: vec!["./ext".into(), "someExtFn".into()],
                }),
                kind: RefKind::Value,
            }],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
    ];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();
    let edges = reader.find_ref_edges_by_symbol(0);
    assert_eq!(
        edges.len(),
        1,
        "TS named import must resolve to ext.ts's someExtFn",
    );
    assert_eq!(edges[0].line, 5);
}

#[test]
fn ref_edges_python_imported_cross_file() {
    // Same shape as the TS test, for `from external_pkg import
    // Some_Type`. The Python binder emits segments=["external_pkg",
    // "Some_Type"]; Pass-2 looks up "Some_Type" and finds the file-A
    // definition.
    use vex::parse::scope::{BindTarget, BoundRef, RefKind, UsePath};

    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![
        ParsedFile {
            path: "a.py".into(),
            symbols: vec![make_symbol("Some_Type", SymbolKind::Class, 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
        ParsedFile {
            path: "b.py".into(),
            symbols: vec![make_symbol("caller_fn", SymbolKind::Function, 3)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![BoundRef {
                name: "Some_Type".into(),
                line: 4,
                col: 12,
                target: BindTarget::Imported(UsePath {
                    segments: vec!["external_pkg".into(), "Some_Type".into()],
                }),
                kind: RefKind::Type,
            }],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
    ];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();
    let edges = reader.find_ref_edges_by_symbol(0);
    assert_eq!(
        edges.len(),
        1,
        "Python `from x import Y` must resolve to file-A's Y",
    );
    assert_eq!(edges[0].line, 4);
}

#[test]
fn ref_edges_ts_default_import_does_not_resolve_cross_file() {
    // `import Foo from './x';` binds with segments=["./x"]. Pass-2
    // looks up `segments.last()` = "./x" in name_to_global, which
    // will never match a real symbol name. This pins the documented
    // honest-failure behaviour: default imports DO NOT cross-file
    // resolve. A future writer change that accidentally normalized
    // path strings and let them match a file path would be caught
    // here.
    use vex::parse::scope::{BindTarget, BoundRef, RefKind, UsePath};

    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files = vec![
        ParsedFile {
            path: "x.ts".into(),
            symbols: vec![make_symbol("defaultExport", SymbolKind::Function, 1)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
        ParsedFile {
            path: "y.ts".into(),
            symbols: vec![make_symbol("caller_fn", SymbolKind::Function, 3)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![BoundRef {
                name: "defaultExport".into(),
                line: 5,
                col: 1,
                target: BindTarget::Imported(UsePath {
                    segments: vec!["./x".into()],
                }),
                kind: RefKind::Value,
            }],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
        },
    ];

    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();
    let edges = reader.find_ref_edges_by_symbol(0);
    assert!(
        edges.is_empty(),
        "default-import path segments=[source] must not match file-level symbols",
    );
}

// --- Empty index ---

#[test]
fn empty_index_has_zero_symbols() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let files: Vec<ParsedFile> = vec![];
    vex::store::writer::write_index(&files, &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    assert_eq!(reader.symbol_count(), 0);
    assert!(!reader.has_vectors());
    let results = vex::search::structural::search_with_fuzzy(&reader, "anything", 10);
    assert!(results.is_empty());
}
