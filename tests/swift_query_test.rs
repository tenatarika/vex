//! Swift grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Swift)
        .expect("swift grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::Swift)
        .expect("swift grammar must load")
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn swift_grammar_loads() {
    let _ = extract_symbols_and_imports("", Language::Swift)
        .expect("swift grammar must load on empty input");
}

#[test]
fn swift_function() {
    let s = symbols("func greet(name: String) -> String { return name }");
    assert!(
        s.iter()
            .any(|(n, k)| n == "greet" && *k == SymbolKind::Function),
        "expected fn greet, got {s:?}"
    );
}

#[test]
fn swift_class_and_protocol() {
    let src = "class Foo {}\nprotocol Bar {}\n";
    let s = symbols(src);
    assert!(
        s.iter().any(|(n, k)| n == "Foo" && *k == SymbolKind::Class),
        "expected class Foo, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Bar" && *k == SymbolKind::Interface),
        "expected protocol Bar (mapped to Interface), got {s:?}"
    );
}

#[test]
fn swift_enum() {
    let s = symbols("enum Status { case ok, fail }");
    assert!(
        s.iter()
            .any(|(n, k)| n == "Status" && *k == SymbolKind::Enum),
        "expected enum Status, got {s:?}"
    );
    // Regression guard: tree-sitter folds enum into `class_declaration`, so a
    // catch-all `(class_declaration name: ...)` pattern would double-capture
    // it as Class. Per-`declaration_kind` patterns prevent that.
    assert!(
        !s.iter()
            .any(|(n, k)| n == "Status" && *k == SymbolKind::Class),
        "enum Status must not also be indexed as Class — got {s:?}"
    );
}

#[test]
fn swift_struct_distinct_from_class() {
    let src = "class Foo {}\nstruct Bar {}\n";
    let s = symbols(src);
    assert!(
        s.iter().any(|(n, k)| n == "Foo" && *k == SymbolKind::Class),
        "expected Foo as Class, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Bar" && *k == SymbolKind::Struct),
        "expected Bar as Struct, got {s:?}"
    );
    // Neither should leak into the other kind.
    assert!(
        !s.iter().any(|(n, k)| n == "Bar" && *k == SymbolKind::Class),
        "struct Bar must not also be Class: {s:?}"
    );
    assert!(
        !s.iter()
            .any(|(n, k)| n == "Foo" && *k == SymbolKind::Struct),
        "class Foo must not also be Struct: {s:?}"
    );
}

#[test]
fn swift_imports() {
    let imp = imports("import Foundation\nimport UIKit\n");
    assert!(imp.iter().any(|i| i == "Foundation"), "{imp:?}");
    assert!(imp.iter().any(|i| i == "UIKit"), "{imp:?}");
}
