//! Regression coverage for the tree-sitter C# grammar.
//!
//! v1.4.1 shipped tree-sitter 0.24 (ABI 14) against tree-sitter-c-sharp 0.23.5
//! (ABI 15). Every `.cs` file silently failed to parse and the index reported
//! zero C# symbols. These tests fail loudly if the same kind of ABI / query
//! regression sneaks back in.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::CSharp)
        .expect("csharp grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

#[test]
fn csharp_grammar_loads() {
    // Smoke test: even an empty source must not error out.
    let _ = extract_symbols_and_imports("", Language::CSharp)
        .expect("csharp grammar must load on empty input");
}

#[test]
fn csharp_class_extracted() {
    let src = "namespace Foo { public class GridService { public int X; } }";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "GridService" && *k == SymbolKind::Class),
        "expected class GridService, got {s:?}"
    );
}

#[test]
fn csharp_method_extracted() {
    let src = r#"
        public class Service {
            public void DoStuff() { }
        }
    "#;
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "DoStuff" && *k == SymbolKind::Function),
        "expected method DoStuff, got {s:?}"
    );
}

#[test]
fn csharp_interface_and_struct() {
    let src = r#"
        public interface IThing { }
        public struct Point { public int X; public int Y; }
    "#;
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "IThing" && *k == SymbolKind::Interface),
        "expected interface IThing, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Point" && *k == SymbolKind::Struct),
        "expected struct Point, got {s:?}"
    );
}

#[test]
fn csharp_enum() {
    let src = "public enum Status { Ok, Err }";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Status" && *k == SymbolKind::Enum),
        "expected enum Status, got {s:?}"
    );
}

#[test]
fn csharp_property() {
    let src = r#"
        public class User {
            public string Name { get; set; }
        }
    "#;
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Name" && *k == SymbolKind::Property),
        "expected property Name, got {s:?}"
    );
}
