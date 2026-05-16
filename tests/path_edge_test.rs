use tempfile::TempDir;
use vex::index::pipeline;
use vex::search::rerank::{rerank, RerankContext};
use vex::search::{MatchType, SearchResult};
use vex::store::reader::IndexReader;
use vex::util::config;

// Helper to build a minimal SearchResult for rerank-only tests.
fn make_result(name: &str, kind: &str, path: &str) -> SearchResult {
    SearchResult {
        name: name.to_string(),
        kind: kind.to_string(),
        path: path.to_string(),
        line: 1,
        signature: None,
        score: 1.0,
        match_type: MatchType::Structural,
    }
}

// --- Pipeline + search helpers ---

fn run_index(project_dir: &std::path::Path) -> usize {
    pipeline::run(project_dir, false, "minilm-l6-v2", &[]).expect("pipeline::run failed")
}

fn open_reader(project_dir: &std::path::Path) -> IndexReader {
    let canon = project_dir
        .canonicalize()
        .expect("canonicalize project dir");
    let index_path = config::index_path(&canon);
    IndexReader::open(&index_path).expect("IndexReader::open failed")
}

// --- Tests ---

/// A Rust file inside a directory whose name contains spaces must be indexed
/// and its symbols must be discoverable by name.
#[test]
fn path_with_spaces() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    let module_dir = project_dir.join("src").join("my module");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(module_dir.join("file.rs"), "pub fn spaced() {}").unwrap();

    let count = run_index(&project_dir);
    assert!(count >= 1, "expected at least 1 symbol, got {count}");

    let reader = open_reader(&project_dir);
    let results = vex::search::structural::search_with_fuzzy(&reader, "spaced", 10);
    assert!(
        !results.is_empty(),
        "symbol 'spaced' must be found in a space-containing path"
    );

    // Verify the returned path actually contains the space.
    let found_path = &results[0].path;
    assert!(
        found_path.contains("my module") || found_path.contains("my%20module"),
        "result path should reference the space-containing directory; got: {found_path}"
    );
}

/// A symbol 20 directories deep must be indexed without any panic.
#[test]
fn deeply_nested_path() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");

    // Build src/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/
    let deep_dir = project_dir
        .join("src")
        .join("a")
        .join("b")
        .join("c")
        .join("d")
        .join("e")
        .join("f")
        .join("g")
        .join("h")
        .join("i")
        .join("j")
        .join("k")
        .join("l")
        .join("m")
        .join("n")
        .join("o")
        .join("p")
        .join("q")
        .join("r")
        .join("s");
    std::fs::create_dir_all(&deep_dir).unwrap();
    std::fs::write(deep_dir.join("deep.rs"), "pub fn deep_fn() {}").unwrap();

    // Must not panic.
    let count = run_index(&project_dir);
    assert!(count >= 1, "expected at least 1 symbol, got {count}");

    let reader = open_reader(&project_dir);
    let results = vex::search::structural::search_with_fuzzy(&reader, "deep_fn", 10);
    assert!(
        !results.is_empty(),
        "deep_fn must be found in a 20-level-deep path"
    );
}

/// Reranking a result with a relative path against an absolute context_path must
/// not panic.  Because the path separator algorithm splits on '/' and the absolute
/// path starts with an empty component before the leading '/', the two paths share
/// zero common components — so the path-overlap boost remains 1.0.
#[test]
fn rerank_absolute_vs_relative_context_path() {
    let relative_result = vec![make_result("Config", "struct", "src/billing/config.rs")];

    // Absolute context path — must not panic.
    let ctx_absolute = RerankContext {
        kind_hint: None,
        context_path: Some("/absolute/src/billing/gateway.rs"),
    };
    let ranked_absolute = rerank("Config", &ctx_absolute, relative_result.clone());
    assert_eq!(ranked_absolute.len(), 1, "must not drop results");
    assert!(
        !ranked_absolute[0].score.is_nan(),
        "score must not be NaN after absolute context_path"
    );

    // Relative context path — shares "src/billing" directory → expects a higher score.
    let ctx_relative = RerankContext {
        kind_hint: None,
        context_path: Some("src/billing/gateway.rs"),
    };
    let ranked_relative = rerank("Config", &ctx_relative, relative_result);
    assert_eq!(ranked_relative.len(), 1, "must not drop results");

    // The relative context shares directory components; its score should be at least
    // as high as the absolute-context score (which contributes no path overlap).
    assert!(
        ranked_relative[0].score >= ranked_absolute[0].score,
        "relative context (same dir) should score >= absolute context (no shared components): \
         relative={}, absolute={}",
        ranked_relative[0].score,
        ranked_absolute[0].score
    );
}

/// Reranking with a Windows-style backslash path as context_path must not panic.
/// The path overlap algorithm splits on '/' so backslash paths yield zero shared
/// components — the boost stays at 1.0.  This is a known limitation; we only
/// verify no crash.
#[test]
fn rerank_windows_backslash_path() {
    let results = vec![make_result("Config", "struct", "src/billing/config.rs")];

    let ctx = RerankContext {
        kind_hint: None,
        context_path: Some("src\\billing\\gateway.rs"),
    };

    // Must not panic regardless of the backslash-containing path.
    let ranked = rerank("Config", &ctx, results);
    assert_eq!(ranked.len(), 1, "must not drop results");
    assert!(
        !ranked[0].score.is_nan(),
        "score must not be NaN for backslash context_path"
    );
    assert!(
        !ranked[0].score.is_infinite(),
        "score must not be infinite for backslash context_path"
    );
}

/// An empty src/ directory (no source files) must produce a valid index with
/// zero symbols — pipeline::run() and IndexReader::open() must both succeed.
#[test]
fn empty_directory_indexes_without_error() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    // Create an empty src/ subdirectory — no files inside.
    std::fs::create_dir_all(project_dir.join("src")).unwrap();

    let count = pipeline::run(&project_dir, false, "minilm-l6-v2", &[])
        .expect("pipeline::run must succeed on empty dir");
    assert_eq!(
        count, 0,
        "empty directory should produce 0 symbols, got {count}"
    );

    let reader = open_reader(&project_dir);
    assert_eq!(
        reader.symbol_count(),
        0,
        "IndexReader must report 0 symbols for an empty project"
    );
}

/// Indexing a directory that contains a symlink to a source file must not panic.
/// The symbol from the real file must be found at least once; whether the symlink
/// is followed or skipped is an implementation detail.
#[cfg(unix)]
#[test]
fn symlink_to_file() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    let src_dir = project_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    // Real file with a symbol.
    std::fs::write(src_dir.join("real.rs"), "pub fn real_fn() {}").unwrap();

    // Symlink: src/link.rs -> src/real.rs
    symlink(src_dir.join("real.rs"), src_dir.join("link.rs")).unwrap();

    // Must not panic — pipeline handles the symlink gracefully.
    let count = pipeline::run(&project_dir, false, "minilm-l6-v2", &[])
        .expect("pipeline::run must not fail with symlink");
    assert!(count >= 1, "at least one symbol expected, got {count}");

    let reader = open_reader(&project_dir);
    let results = vex::search::structural::search_with_fuzzy(&reader, "real_fn", 10);
    assert!(
        !results.is_empty(),
        "real_fn must be found (symlink followed or real file indexed directly)"
    );
}
