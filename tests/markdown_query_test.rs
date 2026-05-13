//! Markdown grammar regression coverage.
//!
//! Catches ABI mismatches and AST node renames against `tree-sitter-md`. The
//! 0.3 → 0.5 jump in this crate restructured `atx_heading`'s heading-text
//! field; this test would have failed loudly if the query was not adapted.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Markdown)
        .expect("markdown grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name.trim().to_string(), s.kind))
        .collect()
}

#[test]
fn markdown_grammar_loads() {
    let _ = extract_symbols_and_imports("", Language::Markdown)
        .expect("markdown grammar must load on empty input");
}

#[test]
fn markdown_atx_heading_h1() {
    let s = symbols("# Top level\n\nsome text\n");
    assert!(
        s.iter()
            .any(|(n, k)| n == "Top level" && *k == SymbolKind::Heading),
        "expected H1 heading, got {s:?}"
    );
}

#[test]
fn markdown_atx_heading_h2_and_h3() {
    let src = "## Section\n\n### Subsection\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Section" && *k == SymbolKind::Heading),
        "expected H2 'Section', got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Subsection" && *k == SymbolKind::Heading),
        "expected H3 'Subsection', got {s:?}"
    );
}

#[test]
fn markdown_atx_heading_h4_h5_h6() {
    let src = "#### Four\n##### Five\n###### Six\n";
    let s = symbols(src);
    for expected in ["Four", "Five", "Six"] {
        assert!(
            s.iter()
                .any(|(n, k)| n == expected && *k == SymbolKind::Heading),
            "expected heading {expected:?}, got {s:?}"
        );
    }
}
