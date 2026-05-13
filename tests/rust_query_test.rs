//! Rust grammar regression coverage.
//!
//! Verifies the tree-sitter-rust grammar still parses and extracts the
//! symbols we care about. Catches ABI mismatches and AST node renames
//! across grammar version bumps.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Rust)
        .expect("rust grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::Rust)
        .expect("rust grammar must load")
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn rust_grammar_loads() {
    let _ = extract_symbols_and_imports("", Language::Rust)
        .expect("rust grammar must load on empty input");
}

#[test]
fn rust_free_function() {
    let s = symbols("fn add(a: i32, b: i32) -> i32 { a + b }");
    assert!(
        s.iter()
            .any(|(n, k)| n == "add" && *k == SymbolKind::Function),
        "expected fn add, got {s:?}"
    );
}

#[test]
fn rust_struct_and_enum() {
    let src = "struct Point { x: i32, y: i32 }\nenum Status { Ok, Err }";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Point" && *k == SymbolKind::Struct),
        "expected struct Point, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Status" && *k == SymbolKind::Enum),
        "expected enum Status, got {s:?}"
    );
}

#[test]
fn rust_trait_and_impl() {
    let src = r#"
        trait Greet { fn hello(&self); }
        struct Greeter;
        impl Greeter { fn make() -> Self { Greeter } }
    "#;
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Greet" && *k == SymbolKind::Trait),
        "expected trait Greet, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Greeter" && *k == SymbolKind::Impl),
        "expected impl Greeter, got {s:?}"
    );
}

#[test]
fn rust_use_imports() {
    let src = "use std::collections::HashMap;\nuse anyhow::Result;\n";
    let imp = imports(src);
    assert!(
        !imp.is_empty(),
        "expected at least one import captured, got {imp:?}"
    );
}

#[test]
fn rust_type_alias() {
    let src = "type Bytes = Vec<u8>;\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Bytes" && *k == SymbolKind::TypeAlias),
        "expected type alias Bytes, got {s:?}"
    );
}

#[test]
fn rust_const_item() {
    let src = "const MAX_SIZE: usize = 1024;\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "MAX_SIZE" && *k == SymbolKind::Constant),
        "expected const MAX_SIZE, got {s:?}"
    );
}

#[test]
fn rust_impl_method_captured() {
    let src = r#"
        struct Foo;
        impl Foo {
            fn build() -> Self { Foo }
            fn run(&self) {}
        }
    "#;
    let s = symbols(src);
    assert!(
        s.iter().any(|(n, _)| n == "build"),
        "expected impl method build, got {s:?}"
    );
    assert!(
        s.iter().any(|(n, _)| n == "run"),
        "expected impl method run, got {s:?}"
    );
}
