use vex::parse;
use vex::parse::language::Language;

// --- BOM handling ---

/// A UTF-8 BOM followed by valid Python should not cause a panic.
/// The parser either extracts the symbol or skips it — either outcome is acceptable,
/// but it must not crash.
#[test]
fn bom_at_file_start() {
    // UTF-8 BOM (U+FEFF encoded as EF BB BF) prepended to valid Python.
    let content = "\u{FEFF}def hello(): pass";
    let result = parse::parse_file("bom_test.py", content, Language::Python);
    assert!(
        result.is_ok(),
        "parsing BOM-prefixed Python should not error"
    );

    let parsed = result.unwrap();
    // The symbol may or may not be found depending on whether tree-sitter
    // treats the BOM as whitespace, but there must be no panic.
    // If found, the name must be correct.
    for sym in &parsed.symbols {
        assert_eq!(sym.name, "hello", "symbol name must not include BOM bytes");
    }
}

// --- Mixed line endings ---

/// Rust source with mixed \r\n and \n line endings must be parsed without
/// crashing, and the reported line number for `mixed_fn` must correspond to
/// the physical line where the `fn` keyword appears (counting \n as the line
/// terminator, which is what tree-sitter does).
#[test]
fn mixed_line_endings() {
    // Line 1: "fn first() {}"  ends with \r\n
    // Line 2: "fn mixed_fn() {}"  ends with \n
    let content = "fn first() {}\r\nfn mixed_fn() {}\n";
    let result = parse::parse_file("mixed.rs", content, Language::Rust);
    assert!(
        result.is_ok(),
        "parsing Rust with mixed line endings should not error"
    );

    let parsed = result.unwrap();
    let mixed = parsed.symbols.iter().find(|s| s.name == "mixed_fn");
    assert!(
        mixed.is_some(),
        "mixed_fn should be extracted; got: {:?}",
        parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    // tree-sitter counts rows from 0 and we add 1, so mixed_fn is on line 2.
    assert_eq!(
        mixed.unwrap().line,
        2,
        "mixed_fn should be reported on line 2"
    );
}

// --- Unicode identifiers ---

/// Python 3 allows non-ASCII identifiers. tree-sitter-python supports them.
/// The parser must not panic and must return symbols with the correct names.
#[test]
fn unicode_identifiers_python() {
    let content = "def café(): pass\nclass Ñoño: pass\n";
    let result = parse::parse_file("unicode_ids.py", content, Language::Python);
    assert!(
        result.is_ok(),
        "parsing Python with unicode identifiers should not error"
    );

    let parsed = result.unwrap();
    let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();

    // Both symbols must be present with their exact unicode names.
    assert!(
        names.contains(&"café"),
        "expected 'café' in symbols; got: {names:?}"
    );
    assert!(
        names.contains(&"Ñoño"),
        "expected 'Ñoño' in symbols; got: {names:?}"
    );
}

// --- Empty file ---

/// An empty Rust file (0 bytes) must return an empty symbol list without panicking.
#[test]
fn empty_file_no_panic() {
    let content = "";
    let result = parse::parse_file("empty.rs", content, Language::Rust);
    assert!(
        result.is_ok(),
        "parsing an empty Rust file should not error"
    );

    let parsed = result.unwrap();
    assert!(
        parsed.symbols.is_empty(),
        "empty file should produce no symbols; got: {:?}",
        parsed.symbols
    );
}

// --- Whitespace-only file ---

/// A Python file that contains only spaces and newlines must return an empty
/// symbol list without panicking.
#[test]
fn file_with_only_whitespace() {
    let content = "   \n\n   \n  \t  \n";
    let result = parse::parse_file("whitespace.py", content, Language::Python);
    assert!(
        result.is_ok(),
        "parsing a whitespace-only Python file should not error"
    );

    let parsed = result.unwrap();
    assert!(
        parsed.symbols.is_empty(),
        "whitespace-only file should produce no symbols; got: {:?}",
        parsed.symbols
    );
}

// --- Null bytes in source ---

/// Rust source containing embedded null bytes between function definitions.
/// tree-sitter operates on byte slices so it may produce a partial or empty
/// parse — either is acceptable as long as there is no panic and the result
/// is returned (Ok or Err, but not a process abort).
#[test]
fn null_bytes_in_source() {
    // Embed two null bytes between two function definitions.
    let content = "fn foo() {}\0\0fn bar() {}";
    // parse_file takes &str; Rust strings may contain interior null bytes.
    // The test verifies no panic only — the exact symbol list is unspecified.
    let result = parse::parse_file("nullbytes.rs", content, Language::Rust);

    // We accept both Ok and Err — what we must NOT get is a panic/abort.
    match result {
        Ok(parsed) => {
            // If parsing succeeded, each reported symbol name must be valid UTF-8
            // and non-empty.
            for sym in &parsed.symbols {
                assert!(
                    !sym.name.is_empty(),
                    "symbol name must not be empty; got: {sym:?}"
                );
                assert!(
                    !sym.name.contains('\0'),
                    "symbol name must not contain null bytes; got: {:?}",
                    sym.name
                );
            }
        }
        Err(_) => {
            // A graceful error is also an acceptable outcome.
        }
    }
}
