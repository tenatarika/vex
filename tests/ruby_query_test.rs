//! Ruby grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Ruby)
        .expect("ruby grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

#[test]
fn ruby_grammar_loads() {
    let _ = extract_symbols_and_imports("", Language::Ruby)
        .expect("ruby grammar must load on empty input");
}

#[test]
fn ruby_class_and_module() {
    let src = "module Helpers\nend\n\nclass User\n  def name\n    @name\n  end\nend\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Helpers" && *k == SymbolKind::Class),
        "expected module Helpers (mapped to Class), got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "User" && *k == SymbolKind::Class),
        "expected class User, got {s:?}"
    );
}

#[test]
fn ruby_instance_method() {
    let src = "class A\n  def hello\n    'hi'\n  end\nend\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "hello" && *k == SymbolKind::Function),
        "expected method hello, got {s:?}"
    );
}

#[test]
fn ruby_singleton_method() {
    let src = "class A\n  def self.build\n    new\n  end\nend\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "build" && *k == SymbolKind::Function),
        "expected singleton method build, got {s:?}"
    );
}
