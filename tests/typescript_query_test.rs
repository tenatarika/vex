//! TypeScript / TSX grammar regression coverage.
//!
//! We compile against `LANGUAGE_TSX`, the superset that handles both `.ts`
//! and `.tsx`.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::TypeScript)
        .expect("typescript grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

#[test]
fn typescript_grammar_loads() {
    let _ = extract_symbols_and_imports("", Language::TypeScript)
        .expect("typescript grammar must load on empty input");
}

#[test]
fn typescript_class_and_interface() {
    let src = "export class Foo {}\nexport interface Bar { x: number }\n";
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
fn typescript_function_and_arrow() {
    let src = "function add(a: number, b: number): number { return a + b }\nconst sub = (a: number, b: number) => a - b;\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "add" && *k == SymbolKind::Function),
        "expected fn add, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "sub" && *k == SymbolKind::Function),
        "expected arrow fn sub, got {s:?}"
    );
}

#[test]
fn typescript_enum_and_type_alias() {
    let src = "enum Status { Ok, Err }\ntype Id = string;\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Status" && *k == SymbolKind::Enum),
        "expected enum Status, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Id" && *k == SymbolKind::TypeAlias),
        "expected type alias Id, got {s:?}"
    );
}

#[test]
fn typescript_tsx_compiles() {
    // TSX-flavored input must parse without error since we use LANGUAGE_TSX.
    let src = "const App = () => <div>hi</div>;\n";
    let _ = symbols(src);
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::TypeScript)
        .expect("typescript grammar must load")
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn typescript_named_default_namespace_imports() {
    let src = r#"
        import { Foo, Bar as Baz } from './foo';
        import Default from './default';
        import * as NS from './ns';
    "#;
    let imp = imports(src);
    // We don't pin the exact set (grammar minor versions sometimes shift
    // which identifiers are captured), but every import form should yield
    // at least one identifier or the regression is real.
    assert!(
        imp.iter().any(|i| i == "Foo" || i == "Bar" || i == "Baz"),
        "named import not captured: {imp:?}"
    );
    assert!(
        imp.iter().any(|i| i == "Default"),
        "default import not captured: {imp:?}"
    );
    assert!(
        imp.iter().any(|i| i == "NS"),
        "namespace import not captured: {imp:?}"
    );
}

// --- 11.1.1: AST-aware ref extraction (no comment/string noise) ---

fn refs(src: &str) -> Vec<String> {
    vex::parse::parse_file("test.ts", src, Language::TypeScript)
        .expect("typescript grammar must load")
        .refs
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn typescript_refs_skip_line_comment() {
    let src = "// CommentedSymbol here\nfunction run() { realFunction(); }\n";
    let r = refs(src);
    assert!(
        !r.contains(&"CommentedSymbol".to_string()),
        "line-comment ident leaked into refs: {r:?}"
    );
    assert!(
        r.contains(&"realFunction".to_string()),
        "real ident missing from refs: {r:?}"
    );
}

#[test]
fn typescript_refs_skip_block_comment() {
    let src = "/* BlockSymbol here */\nfunction run() { realFunction(); }\n";
    let r = refs(src);
    assert!(
        !r.contains(&"BlockSymbol".to_string()),
        "block-comment ident leaked into refs: {r:?}"
    );
    assert!(
        r.contains(&"realFunction".to_string()),
        "real ident missing from refs: {r:?}"
    );
}

#[test]
fn typescript_refs_skip_string_literal() {
    let src = "function run() { const s = \"StringSymbol here\"; realFunction(); }\n";
    let r = refs(src);
    assert!(
        !r.contains(&"StringSymbol".to_string()),
        "string-literal ident leaked into refs: {r:?}"
    );
    assert!(
        r.contains(&"realFunction".to_string()),
        "real ident missing from refs: {r:?}"
    );
}
