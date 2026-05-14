//! CSS grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Css)
        .expect("css grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

#[test]
fn css_grammar_loads() {
    extract_symbols_and_imports("", Language::Css).expect("css grammar must load on empty input");
}

#[test]
fn css_extracts_class_selector() {
    let s = symbols(".primary-btn { color: red; }\n");
    assert!(
        s.contains(&("primary-btn".into(), SymbolKind::Class)),
        "{s:?}"
    );
}

#[test]
fn css_extracts_id_selector() {
    let s = symbols("#site-header { padding: 1rem; }\n");
    assert!(
        s.contains(&("site-header".into(), SymbolKind::Constant)),
        "{s:?}"
    );
}

#[test]
fn css_extracts_keyframes() {
    let s = symbols("@keyframes fade-in { from { opacity: 0; } to { opacity: 1; } }\n");
    assert!(
        s.contains(&("fade-in".into(), SymbolKind::Function)),
        "{s:?}"
    );
}

#[test]
fn css_extracts_custom_property_only() {
    let s = symbols(":root { --primary: #fff; color: red; padding: 1rem; }\n");
    // `--primary` is the only custom property — `color` and `padding` are
    // standard property names and should NOT be indexed (would be noise).
    assert!(
        s.contains(&("--primary".into(), SymbolKind::Property)),
        "{s:?}"
    );
    assert!(
        !s.iter().any(|(n, _)| n == "color"),
        "non-custom property leaked: {s:?}"
    );
    assert!(
        !s.iter().any(|(n, _)| n == "padding"),
        "non-custom property leaked: {s:?}"
    );
}
