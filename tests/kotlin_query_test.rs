//! Kotlin grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Kotlin)
        .expect("kotlin grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

#[test]
fn kotlin_grammar_loads() {
    let _ = extract_symbols_and_imports("", Language::Kotlin)
        .expect("kotlin grammar must load on empty input");
}

#[test]
fn kotlin_function() {
    let s = symbols("fun greet(name: String): String = name");
    assert!(
        s.iter()
            .any(|(n, k)| n == "greet" && *k == SymbolKind::Function),
        "expected fn greet, got {s:?}"
    );
}

#[test]
fn kotlin_class_and_interface() {
    let src = "class Foo\ninterface Bar\n";
    let s = symbols(src);
    assert!(
        s.iter().any(|(n, k)| n == "Foo" && *k == SymbolKind::Class),
        "expected class Foo, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Bar" && *k == SymbolKind::Interface),
        "expected interface Bar, got {s:?}"
    );
}

#[test]
fn kotlin_data_class_and_object() {
    let src = "data class Point(val x: Int, val y: Int)\nobject Singleton\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Point" && *k == SymbolKind::Class),
        "expected data class Point, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Singleton" && *k == SymbolKind::Class),
        "expected object Singleton (mapped to Class), got {s:?}"
    );
}

#[test]
fn kotlin_top_level_property() {
    let src = "val MAX_COUNT: Int = 42\nvar logLevel: String = \"info\"\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "MAX_COUNT" && *k == SymbolKind::Property),
        "expected val MAX_COUNT as Property, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "logLevel" && *k == SymbolKind::Property),
        "expected var logLevel as Property, got {s:?}"
    );
}
