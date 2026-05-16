use std::time::Duration;
use tempfile::TempDir;
use vex::parse;
use vex::parse::language::Language;

// ---------------------------------------------------------------------------
// Helper: create a subdirectory with one file, return the project root dir.
// The walker discovers files relative to the project root, so source files
// must live in at least one level of subdirectory (e.g., "src/").
// ---------------------------------------------------------------------------

fn write_src_file(dir: &TempDir, name: &str, content: &str) {
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join(name), content).unwrap();
}

// ---------------------------------------------------------------------------
// 1. same_symbol_name_across_languages
//
// Goal: the full pipeline must index all three files and the symbol "Config"
// must appear once per language, each with a distinct file path.
// ---------------------------------------------------------------------------

#[test]
fn same_symbol_name_across_languages() {
    let tmp = TempDir::new().unwrap();

    write_src_file(&tmp, "config.rs", "pub struct Config {}");
    write_src_file(&tmp, "config.py", "class Config: pass");
    write_src_file(&tmp, "config.ts", "export class Config {}");

    // Full structural index (no embeddings).
    let count = vex::index::pipeline::run(tmp.path(), false, "minilm-l6-v2", &[]).unwrap();
    assert!(count >= 3, "expected at least 3 symbols, got {count}");

    // Open the resulting index.
    let index_path = vex::util::config::index_path(&tmp.path().canonicalize().unwrap());
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();

    // Collect all results for the query "Config".
    let results = vex::search::structural::search_with_fuzzy(&reader, "Config", 50);

    let config_results: Vec<_> = results.iter().filter(|r| r.name == "Config").collect();

    assert!(
        config_results.len() >= 3,
        "expected at least 3 'Config' results (one per language), got {}: {:?}",
        config_results.len(),
        config_results.iter().map(|r| &r.path).collect::<Vec<_>>()
    );

    // Each result must have a different file path.
    let paths: Vec<&str> = config_results.iter().map(|r| r.path.as_str()).collect();
    let mut unique_paths = paths.clone();
    unique_paths.sort_unstable();
    unique_paths.dedup();
    assert_eq!(
        unique_paths.len(),
        config_results.len(),
        "all 'Config' results should come from different files, got paths: {paths:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. wrong_extension_does_not_crash
//
// Goal: passing Rust source code to the Python parser must not panic.
// It may return Ok with 0 or more symbols, or Err — but never abort.
// ---------------------------------------------------------------------------

#[test]
fn wrong_extension_does_not_crash() {
    // This is valid Rust syntax, but we tell the parser it is Python.
    let content = "fn this_is_rust() {}";

    let result = parse::parse_file("wrong.py", content, Language::Python);

    // We accept any outcome (Ok or Err) — the one thing we must not see is a panic.
    match result {
        Ok(parsed) => {
            // If the Python parser happens to extract something, names must be
            // valid non-empty strings (no garbage from mis-parsed bytes).
            for sym in &parsed.symbols {
                assert!(
                    !sym.name.is_empty(),
                    "any extracted symbol name must be non-empty; got: {sym:?}"
                );
            }
            // No assertion on count: 0 symbols is the expected case, but the
            // test does not mandate it.
        }
        Err(_) => {
            // A graceful error is also an acceptable outcome.
        }
    }
}

// ---------------------------------------------------------------------------
// 3. very_large_file_no_timeout
//
// Goal: parsing a Python file with 1 000 function definitions must complete
// in reasonable time and must extract ~1 000 symbols.
// ---------------------------------------------------------------------------

#[test]
fn very_large_file_no_timeout() {
    // Build the content in one allocation.
    let mut content = String::with_capacity(30 * 1_000);
    for i in 0..1_000 {
        content.push_str(&format!("def func_{i}(): pass\n"));
    }

    let start = std::time::Instant::now();
    let result = parse::parse_file("large.py", &content, Language::Python);
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "parsing a 1 000-function Python file must succeed"
    );

    let parsed = result.unwrap();

    // The extractor should find all 1 000 functions (exact count expected because
    // there are no other constructs in the file).
    assert_eq!(
        parsed.symbols.len(),
        1_000,
        "expected exactly 1 000 symbols, got {}",
        parsed.symbols.len()
    );

    // Generous upper-bound: a tree-sitter parse of 1 000 trivial functions
    // should complete well under 5 seconds even on slow CI runners.
    assert!(
        elapsed < Duration::from_secs(5),
        "parsing 1 000 functions took {elapsed:?}, expected < 5 s"
    );
}

// ---------------------------------------------------------------------------
// 4. deeply_nested_code_no_stack_overflow
//
// Goal: source with 200 levels of if-nesting must not crash with a stack
// overflow.  A function definition is embedded at the deepest level so there
// is at least one symbol for the parser to visit.
// ---------------------------------------------------------------------------

#[test]
fn deeply_nested_code_no_stack_overflow() {
    const DEPTH: usize = 200;

    // Build preamble: 200 `if True:` lines, each indented 4 extra spaces.
    let mut content = String::new();
    for level in 0..DEPTH {
        let indent = "    ".repeat(level);
        content.push_str(&format!("{indent}if True:\n"));
    }

    // Place a function definition at the deepest level.
    let deepest_indent = "    ".repeat(DEPTH);
    content.push_str(&format!("{deepest_indent}def deep_function(): pass\n"));

    // Must not panic (stack overflow would abort the process, not return Err).
    let result = parse::parse_file("nested.py", &content, Language::Python);

    match result {
        Ok(parsed) => {
            // If parsing succeeded the deeply-nested function should be found.
            // (tree-sitter performs iterative parsing so this is expected.)
            let found = parsed.symbols.iter().any(|s| s.name == "deep_function");
            assert!(
                found,
                "deep_function should be found in the deeply-nested AST; got symbols: {:?}",
                parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }
        Err(_) => {
            // A parse error is also acceptable — the key invariant is no abort.
        }
    }
}

// ---------------------------------------------------------------------------
// 5. file_with_syntax_errors_parsed_partially
//
// Goal: tree-sitter performs error recovery, so a Rust file with one broken
// declaration between two valid ones must yield at least the valid symbols.
// The parser must not return Err or panic.
// ---------------------------------------------------------------------------

#[test]
fn file_with_syntax_errors_parsed_partially() {
    // `fn invalid(` has no matching `)` or body — this is a syntax error.
    // `fn valid()` and `fn also_valid()` are well-formed.
    let content = "fn valid() {} fn invalid( {} fn also_valid() {}";

    let result = parse::parse_file("broken.rs", content, Language::Rust);

    assert!(
        result.is_ok(),
        "parse_file must not return Err for syntax-error input; got: {:?}",
        result.err()
    );

    let parsed = result.unwrap();
    let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();

    // tree-sitter should recover and extract at least one of the two valid functions.
    // We assert the weaker guarantee (>=1) to stay robust across grammar versions.
    assert!(
        names.contains(&"valid") || names.contains(&"also_valid"),
        "at least one valid function ('valid' or 'also_valid') must be extracted after error recovery; got: {names:?}"
    );
}
