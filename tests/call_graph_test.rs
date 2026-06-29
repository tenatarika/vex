use std::path::PathBuf;

use tempfile::TempDir;
use vex::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
use vex::parse::language::Language;
use vex::store::call_graph::{
    build_callers_fst, encode_caller_key, find_callees_fast, find_callers_fast, CallEdgeBuilder,
    CallGraphFstReader,
};
use vex::store::format::{CallEdge, CallGraphHeader, Header, MIN_SUPPORTED_VERSION, VERSION};
use vex::store::reader::IndexReader;
use vex::store::writer::write_index_with_call_graph;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_symbol(name: &str, line: usize) -> ParsedSymbol {
    ParsedSymbol {
        name: name.to_string(),
        kind: SymbolKind::Function,
        line,
        signature: None,
        doc: None,
        body_tokens: None,
    }
}

fn make_file(path: &str, symbols: Vec<ParsedSymbol>) -> ParsedFile {
    ParsedFile {
        path: path.to_string(),
        symbols,
        refs: Vec::new(),
        call_edges: Vec::new(),
        bound_refs: Vec::new(),
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
    }
}

fn edge(caller: u32, callee: &str, line: u32) -> CallEdgeBuilder {
    CallEdgeBuilder {
        caller_sym_idx: caller,
        callee_name: callee.to_string(),
        line,
    }
}

fn write_and_open(tmp: &TempDir, parsed: &[ParsedFile], edges: &[CallEdgeBuilder]) -> IndexReader {
    let path = tmp.path().join("index.vex");
    write_index_with_call_graph(parsed, &[], 384, edges, None, &path).unwrap();
    IndexReader::open(&path).unwrap()
}

// ---------------------------------------------------------------------------
// Format version tests
// ---------------------------------------------------------------------------

#[test]
fn current_version_is_5() {
    // Bumped to 7 in multi-repo Phase 6 (adds UnresolvedRefsHeader for
    // cross-repo strict-usages fallback). v3..v6 indexes still open
    // because MIN_SUPPORTED_VERSION stays at 3.
    assert_eq!(VERSION, 7);
}

#[test]
fn min_supported_v3() {
    assert_eq!(MIN_SUPPORTED_VERSION, 3);
}

#[test]
fn header_size_unchanged() {
    // Header is 144 bytes (18 fields, no padding between u64 groups).
    // v4 does NOT extend Header — CallGraphHeader is a SEPARATE struct.
    // If this changes, the format is broken for all existing indexes.
    assert_eq!(Header::SIZE, 144);
}

#[test]
fn call_graph_header_size() {
    // 10 × u64 (call graph, 9.3) + 6 × u64 (BM25, 9.4) = 128 bytes.
    assert_eq!(CallGraphHeader::SIZE, 128);
}

#[test]
fn call_edge_size() {
    // 3 × u32 + 1 × u32 pad = 16 bytes
    assert_eq!(CallEdge::SIZE, 16);
}

// ---------------------------------------------------------------------------
// Writer + reader roundtrip
// ---------------------------------------------------------------------------

#[test]
fn empty_call_graph_round_trip() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("foo", 1)])];
    let reader = write_and_open(&tmp, &parsed, &[]);

    // has_call_graph returns false when there are 0 edges
    assert!(!reader.has_call_graph());
    assert_eq!(reader.call_edge_count(), 0);

    // call_graph_header is always Some for v4 indexes
    let cgh = reader.call_graph_header();
    assert!(cgh.is_some(), "v4 index should always have CallGraphHeader");
    // call_edges_len must be 0 for an empty graph
    assert_eq!(cgh.unwrap().call_edges_len, 0);
    // FST sections may be non-zero even with 0 edges (FST emits a minimal
    // header regardless), so we do not assert fst_len == 0 here.
}

#[test]
fn single_edge_round_trip() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("caller", 1)])];
    let edges = vec![edge(0, "external_fn", 42)];
    let reader = write_and_open(&tmp, &parsed, &edges);

    assert!(reader.has_call_graph());
    assert_eq!(reader.call_edge_count(), 1);

    let e = reader.call_edge(0).expect("edge 0 must exist");
    assert_eq!(e.caller_sym_idx, 0);
    assert_eq!(e.line, 42);
    let callee = reader.read_string(e.callee_name_offset);
    assert_eq!(callee, "external_fn");
}

#[test]
fn multiple_edges_preserve_order() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("owner", 1)])];
    let callees_names = ["alpha", "beta", "gamma", "delta", "epsilon"];
    let edges: Vec<CallEdgeBuilder> = callees_names
        .iter()
        .enumerate()
        .map(|(i, name)| edge(0, name, (i + 10) as u32))
        .collect();
    let reader = write_and_open(&tmp, &parsed, &edges);

    assert_eq!(reader.call_edge_count(), 5);
    for (i, expected) in callees_names.iter().enumerate() {
        let e = reader
            .call_edge(i)
            .unwrap_or_else(|| panic!("edge {i} missing"));
        let callee = reader.read_string(e.callee_name_offset);
        assert_eq!(callee, *expected, "edge {i} callee mismatch");
    }
}

// ---------------------------------------------------------------------------
// Callers FST fast path
// ---------------------------------------------------------------------------

#[test]
fn find_callers_fast_basic() {
    let tmp = TempDir::new().unwrap();
    // 3 symbols: call_a (idx 0), call_b (idx 1), target (idx 2)
    let parsed = vec![make_file(
        "src/lib.rs",
        vec![
            make_symbol("call_a", 1),
            make_symbol("call_b", 5),
            make_symbol("target", 10),
        ],
    )];
    let edges = vec![edge(0, "target", 3), edge(1, "target", 7)];
    let reader = write_and_open(&tmp, &parsed, &edges);

    let matches = find_callers_fast(&reader, "target", 50);
    assert_eq!(matches.len(), 2, "both callers should appear");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"call_a"), "call_a should be a caller");
    assert!(names.contains(&"call_b"), "call_b should be a caller");
}

#[test]
fn find_callers_fast_unknown_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("foo", 1)])];
    let reader = write_and_open(&tmp, &parsed, &[edge(0, "bar", 2)]);

    let matches = find_callers_fast(&reader, "Nonexistent", 50);
    assert!(matches.is_empty());
}

#[test]
fn find_callers_fast_case_insensitive() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file(
        "src/lib.rs",
        vec![make_symbol("caller_fn", 1), make_symbol("Target", 10)],
    )];
    // Edge stores "Target" (mixed case) as callee name; FST stores lowercase.
    let edges = vec![edge(0, "Target", 5)];
    let reader = write_and_open(&tmp, &parsed, &edges);

    // Search with lowercase — should find the caller
    let matches = find_callers_fast(&reader, "target", 50);
    assert_eq!(
        matches.len(),
        1,
        "case-insensitive lookup should find caller"
    );
    assert_eq!(matches[0].name, "caller_fn");
}

#[test]
fn find_callers_fast_respects_limit() {
    let tmp = TempDir::new().unwrap();
    let syms: Vec<ParsedSymbol> = (0..5u32)
        .map(|i| make_symbol(&format!("caller_{i}"), i as usize + 1))
        .collect();
    let parsed = vec![make_file("src/lib.rs", syms)];
    let edges: Vec<CallEdgeBuilder> = (0..5u32).map(|i| edge(i, "target", i + 10)).collect();
    let reader = write_and_open(&tmp, &parsed, &edges);

    let matches = find_callers_fast(&reader, "target", 2);
    assert_eq!(matches.len(), 2);
}

#[test]
fn find_callers_fast_v3_index_returns_empty() {
    // Build a minimal v3 file by hand: MAGIC + version=3 + zeros for remaining header
    // A v3 index does not have a CallGraphHeader and has_call_graph() must return false.
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("v3.vex");

    // Header field offsets (repr C, 144 bytes total):
    //   [0..4]   magic
    //   [4..8]   version (u32)
    //   [8..16]  symbol_count (u64)
    //   [16..20] vector_dim (u32)
    //   [20..24] _padding (u32)
    //   [24..32] symbols_offset (u64)
    //   [32..40] vectors_offset (u64)
    //   [40..48] strings_offset (u64)
    //   [48..56] inverted_offset (u64)
    //   [56..64] hnsw_offset (u64)
    //   [64..72] fst_offset (u64)
    //   [72..80] fst_len (u64)
    //   [80..88] postings_offset (u64)
    //   [88..96] postings_len (u64)
    //   [96..104] file_table_offset (u64)
    //   [104..108] file_table_count (u32)
    //   [108..112] _padding2 (u32)
    //   [112..120] sym_fst_offset (u64)
    //   [120..128] sym_fst_len (u64)
    //   [128..136] sym_postings_offset (u64)
    //   [136..144] sym_postings_len (u64)
    let mut data = vec![0u8; Header::SIZE];
    data[0..4].copy_from_slice(b"VEXI");
    data[4..8].copy_from_slice(&3u32.to_le_bytes()); // version = 3

    // Point all offsets to Header::SIZE (no content beyond header, no symbols)
    let base: u64 = Header::SIZE as u64;
    for field_offset in [24usize, 32, 40, 48, 56, 64, 80, 96, 112, 128] {
        data[field_offset..field_offset + 8].copy_from_slice(&base.to_le_bytes());
    }

    std::fs::write(&path, &data).unwrap();

    let reader = IndexReader::open(&path).unwrap();
    assert!(!reader.has_call_graph(), "v3 index has no call graph");
    let matches = find_callers_fast(&reader, "anything", 50);
    assert!(matches.is_empty(), "v3 fast path returns empty");
}

// ---------------------------------------------------------------------------
// Callees FST fast path
// ---------------------------------------------------------------------------

#[test]
fn find_callees_fast_basic() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("main", 1)])];
    let edges = vec![edge(0, "f", 5), edge(0, "g", 6), edge(0, "h", 7)];
    let reader = write_and_open(&tmp, &parsed, &edges);

    let matches = find_callees_fast(&reader, "main", 50);
    assert_eq!(matches.len(), 3, "main calls f, g, h");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"f"));
    assert!(names.contains(&"g"));
    assert!(names.contains(&"h"));
}

#[test]
fn find_callees_fast_unknown_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("foo", 1)])];
    let reader = write_and_open(&tmp, &parsed, &[edge(0, "bar", 2)]);

    let matches = find_callees_fast(&reader, "UnknownCaller", 50);
    assert!(matches.is_empty());
}

#[test]
fn find_callees_fast_dedup() {
    // main calls helper twice on different lines — both should appear (distinct by line)
    let tmp = TempDir::new().unwrap();
    let parsed = vec![make_file("src/lib.rs", vec![make_symbol("main", 1)])];
    let edges = vec![
        edge(0, "helper", 5),
        edge(0, "helper", 9), // same callee, different line
    ];
    let reader = write_and_open(&tmp, &parsed, &edges);

    let matches = find_callees_fast(&reader, "main", 50);
    // Dedup key is (callee_name, path, line): two distinct lines → 2 matches
    assert_eq!(matches.len(), 2, "different lines should not be deduped");
}

// ---------------------------------------------------------------------------
// Same name in different files
// ---------------------------------------------------------------------------

#[test]
fn same_name_in_different_files_via_callees() {
    let tmp = TempDir::new().unwrap();
    // Two files, each with a `process` function calling a distinct callee.
    // process in a.rs is sym_idx=0, process in b.rs is sym_idx=1.
    let parsed = vec![
        make_file("src/a.rs", vec![make_symbol("process", 1)]),
        make_file("src/b.rs", vec![make_symbol("process", 1)]),
    ];
    let edges = vec![edge(0, "callee_a", 5), edge(1, "callee_b", 5)];
    let reader = write_and_open(&tmp, &parsed, &edges);

    let matches = find_callees_fast(&reader, "process", 50);
    assert_eq!(
        matches.len(),
        2,
        "both process definitions should yield callees"
    );
    let callee_names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(callee_names.contains(&"callee_a"), "callee_a expected");
    assert!(callee_names.contains(&"callee_b"), "callee_b expected");
}

// ---------------------------------------------------------------------------
// CLI fast-path dispatch (end-to-end pipeline)
// ---------------------------------------------------------------------------

fn make_tmp_project(tmp: &TempDir, content: &str) -> PathBuf {
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let file = src_dir.join("main.rs");
    std::fs::write(&file, content).unwrap();
    // Canonicalize: on macOS /var/ symlinks to /private/var/ so pipeline::run
    // and config::index_path must agree on the canonical path.
    tmp.path().canonicalize().unwrap()
}

#[test]
fn cli_callers_uses_fast_path_with_v4_index() {
    let tmp = TempDir::new().unwrap();
    let src = "fn caller() { target(); }\nfn target() {}\n";
    let root = make_tmp_project(&tmp, src);

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
        reader.has_call_graph(),
        "pipeline must produce v4 call graph"
    );

    let matches = find_callers_fast(&reader, "target", 50);
    assert_eq!(matches.len(), 1, "caller should call target");
    assert_eq!(matches[0].name, "caller");
}

#[test]
fn cli_callees_uses_fast_path_with_v4_index() {
    let tmp = TempDir::new().unwrap();
    let src = "fn caller() { target(); }\nfn target() {}\n";
    let root = make_tmp_project(&tmp, src);

    vex::index::pipeline::run(
        &root,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let index_path = vex::util::config::index_path(&root);
    let reader = IndexReader::open(&index_path).unwrap();

    let matches = find_callees_fast(&reader, "caller", 50);
    assert_eq!(matches.len(), 1, "caller calls target");
    assert_eq!(matches[0].name, "target");
}

// ---------------------------------------------------------------------------
// extract_call_edges
// ---------------------------------------------------------------------------

#[test]
fn extract_call_edges_rust() {
    let src = "fn outer() { inner(); }\nfn inner() {}\n";
    let edges = vex::callgraph::extract_call_edges(src, Language::Rust);
    assert!(
        !edges.is_empty(),
        "should extract at least one edge from Rust code"
    );
    let found = edges
        .iter()
        .any(|(caller, _, callee, _)| caller == "outer" && callee == "inner");
    assert!(found, "expected (outer, _, inner, _) edge; got: {edges:?}");
}

#[test]
fn extract_call_edges_unsupported_lang_empty() {
    // Lua, TOML, YAML have no call-graph query
    let lua_src = "local function foo() bar() end";
    assert!(
        vex::callgraph::extract_call_edges(lua_src, Language::Lua).is_empty(),
        "Lua should return empty"
    );
    let toml_src = "[package]\nname = \"test\"\n";
    assert!(
        vex::callgraph::extract_call_edges(toml_src, Language::Toml).is_empty(),
        "TOML should return empty"
    );
    let yaml_src = "key: value\n";
    assert!(
        vex::callgraph::extract_call_edges(yaml_src, Language::Yaml).is_empty(),
        "YAML should return empty"
    );
}

// ---------------------------------------------------------------------------
// Build helpers
// ---------------------------------------------------------------------------

#[test]
fn build_callers_fst_dedups_within_callee() {
    // Two distinct edges with the same callee; both should appear in the posting list.
    // Only identical (key, edge_idx) pairs are deduped — different indices are preserved.
    let edges = vec![edge(0, "target", 5), edge(1, "target", 10)];
    let (fst, posts) = build_callers_fst(&edges).unwrap();
    let reader = CallGraphFstReader::new(&fst, &posts).unwrap();
    let indices = reader.find("target");
    assert_eq!(indices.len(), 2, "two distinct edges should both appear");
    assert_eq!(indices[0], 0);
    assert_eq!(indices[1], 1);
}

#[test]
fn encode_caller_key_zero_padded() {
    assert_eq!(encode_caller_key(42), "0000000042");
    assert_eq!(encode_caller_key(0), "0000000000");
    assert_eq!(encode_caller_key(u32::MAX), "4294967295");
}

// ---------------------------------------------------------------------------
// Incremental update preserves edges
// ---------------------------------------------------------------------------

#[test]
fn incremental_update_keeps_edges_for_unchanged_files() {
    let tmp = TempDir::new().unwrap();
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    // a.rs: caller_a → target
    let a_path = src_dir.join("a.rs");
    std::fs::write(&a_path, "fn caller_a() { target(); }\nfn target() {}\n").unwrap();

    // b.rs: caller_b calls helper (not target)
    let b_path = src_dir.join("b.rs");
    std::fs::write(&b_path, "fn caller_b() { helper(); }\nfn helper() {}\n").unwrap();

    // Canonicalize so pipeline::run and config::index_path agree on the root.
    let root = tmp.path().canonicalize().unwrap();

    // Initial full index
    vex::index::pipeline::run(
        &root,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    // Verify initial state: caller_a calls target
    let index_path = vex::util::config::index_path(&root);
    {
        let reader = IndexReader::open(&index_path).unwrap();
        let matches = find_callers_fast(&reader, "target", 50);
        assert!(
            matches.iter().any(|m| m.name == "caller_a"),
            "initial index should have caller_a → target"
        );
    }

    // Modify only a.rs — caller_a now calls something_else instead of target
    std::fs::write(
        &a_path,
        "fn caller_a() { something_else(); }\nfn target() {}\n",
    )
    .unwrap();

    // Incremental update
    vex::index::pipeline::update(
        &root,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    // After update caller_a no longer calls target (re-parsed).
    // b.rs is unchanged; its edges (none to target) still hold.
    let reader = IndexReader::open(&index_path).unwrap();
    let matches = find_callers_fast(&reader, "target", 50);
    assert!(
        !matches.iter().any(|m| m.name == "caller_a"),
        "after update, caller_a should not call target"
    );
}

// ---------------------------------------------------------------------------
// Same-name within a single file — disambiguated by definition line.
// Regression for the CRITICAL bug flagged in the Phase 9.3 review where
// (path, name) keying caused duplicate-named callers to all map to the
// first instance's symbol index.
// ---------------------------------------------------------------------------

#[test]
fn duplicate_function_name_in_same_file_resolves_to_correct_caller() {
    let tmp = TempDir::new().unwrap();
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    // Two functions named `handle` in the same file, each calling a
    // DISTINCT callee. Before the fix, both edges would be attributed to
    // the first `handle` (symbol idx 0); after the fix they go to the
    // correct caller (idx 0 and idx 2 respectively).
    let src = "\
fn handle() { alpha(); }
fn other() {}
fn handle() { beta(); }
fn alpha() {}
fn beta() {}
";
    std::fs::write(src_dir.join("dup.rs"), src).unwrap();
    let root = tmp.path().canonicalize().unwrap();
    vex::index::pipeline::run(
        &root,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let index_path = vex::util::config::index_path(&root);
    let reader = IndexReader::open(&index_path).unwrap();

    // Both `alpha` and `beta` must be reachable as callees of *some*
    // `handle` — proves both call sites were emitted as real edges.
    let alpha_callers = find_callers_fast(&reader, "alpha", 50);
    let beta_callers = find_callers_fast(&reader, "beta", 50);
    assert!(
        alpha_callers.iter().any(|m| m.name == "handle"),
        "alpha should be called from handle; got {alpha_callers:?}"
    );
    assert!(
        beta_callers.iter().any(|m| m.name == "handle"),
        "beta should be called from handle; got {beta_callers:?}"
    );

    // Stronger check: the two edges live at distinct call-site lines (3
    // and 1 in the source above). With the bug, both would resolve to the
    // first handle's symbol and the FIRST handle's call line would
    // dominate one of the result entries — instead we want each callee
    // attributed to its own line.
    let alpha_call_line = alpha_callers
        .iter()
        .find(|m| m.name == "handle")
        .map(|m| m.line);
    let beta_call_line = beta_callers
        .iter()
        .find(|m| m.name == "handle")
        .map(|m| m.line);
    assert_ne!(
        alpha_call_line, beta_call_line,
        "alpha and beta call sites must be attributed to distinct lines"
    );
}
