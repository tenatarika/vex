//! HTML grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Html)
        .expect("html grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

#[test]
fn html_grammar_loads() {
    extract_symbols_and_imports("", Language::Html).expect("html grammar must load on empty input");
}

#[test]
fn html_extracts_id_attribute() {
    let s = symbols("<div id=\"hero-banner\">welcome</div>\n");
    assert!(
        s.contains(&("hero-banner".into(), SymbolKind::Constant)),
        "{s:?}"
    );
}

#[test]
fn html_extracts_custom_element_tag() {
    let s = symbols("<my-button label=\"Save\"></my-button>\n");
    assert!(
        s.contains(&("my-button".into(), SymbolKind::Class)),
        "{s:?}"
    );
}

#[test]
fn html_skips_standard_tag_names() {
    let s = symbols("<div></div>\n<span></span>\n<p>hi</p>\n");
    // Standard HTML tags lack a hyphen and should NOT be indexed — they
    // would otherwise flood the index on any non-trivial HTML file.
    assert!(
        !s.iter()
            .any(|(n, _)| matches!(n.as_str(), "div" | "span" | "p")),
        "standard tag leaked: {s:?}"
    );
}

#[test]
fn html_self_closing_custom_element() {
    let s = symbols("<my-icon name=\"check\" />\n");
    assert!(s.contains(&("my-icon".into(), SymbolKind::Class)), "{s:?}");
}
