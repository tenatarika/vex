//! Go grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Go)
        .expect("go grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::Go)
        .expect("go grammar must load")
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn go_grammar_loads() {
    let _ =
        extract_symbols_and_imports("", Language::Go).expect("go grammar must load on empty input");
}

#[test]
fn go_function_and_method() {
    let src = "package main\n\nfunc Add(a, b int) int { return a + b }\n\ntype T struct{}\nfunc (t T) Method() {}\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Add" && *k == SymbolKind::Function),
        "expected fn Add, got {s:?}"
    );
    // Methods get the `@impl.method` capture in queries/go.scm — extractor.rs
    // maps that to SymbolKind::Method (NOT Function), and we want the kind
    // distinction preserved across grammar upgrades.
    assert!(
        s.iter()
            .any(|(n, k)| n == "Method" && *k == SymbolKind::Method),
        "expected method Method as Method kind, got {s:?}"
    );
}

#[test]
fn go_struct_and_interface() {
    let src =
        "package main\ntype Point struct { X, Y int }\ntype Greeter interface { Greet() string }\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Point" && *k == SymbolKind::Struct),
        "expected struct Point, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Greeter" && *k == SymbolKind::Interface),
        "expected interface Greeter, got {s:?}"
    );
}

#[test]
fn go_imports() {
    let src = "package main\nimport (\n    \"fmt\"\n    \"os\"\n)\n";
    let imp = imports(src);
    assert!(imp.iter().any(|i| i == "fmt"), "{imp:?}");
    assert!(imp.iter().any(|i| i == "os"), "{imp:?}");
}
