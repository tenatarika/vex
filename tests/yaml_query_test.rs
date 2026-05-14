//! YAML grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Yaml)
        .expect("yaml grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

#[test]
fn yaml_grammar_loads() {
    extract_symbols_and_imports("", Language::Yaml).expect("yaml grammar must load on empty input");
}

#[test]
fn yaml_extracts_top_level_keys() {
    let src = r#"name: vex
version: 1.4.2
description: code search
"#;
    let s = symbols(src);
    assert!(s.contains(&("name".into(), SymbolKind::Property)), "{s:?}");
    assert!(
        s.contains(&("version".into(), SymbolKind::Property)),
        "{s:?}"
    );
    assert!(
        s.contains(&("description".into(), SymbolKind::Property)),
        "{s:?}"
    );
}

#[test]
fn yaml_does_not_extract_nested_keys() {
    let src = r#"server:
  host: localhost
  port: 8080
"#;
    let s = symbols(src);
    // We index only document-root keys so deeply nested configs don't
    // flood the index. `server` is a top-level key; `host` and `port`
    // are nested and excluded by design.
    assert!(
        s.contains(&("server".into(), SymbolKind::Property)),
        "{s:?}"
    );
    assert!(
        !s.iter().any(|(n, _)| n == "host" || n == "port"),
        "nested key leaked: {s:?}"
    );
}

#[test]
fn yaml_extracts_quoted_key() {
    let src = "\"complex-key\": value\n";
    let s = symbols(src);
    // tree-sitter-yaml 0.7 captures the double_quote_scalar node whose
    // text spans the surrounding quotes — i.e. `"complex-key"` (six chars
    // plus delimiters). Asserting the exact form so a future grammar
    // change that flips quote handling fails loudly.
    assert!(
        s.iter()
            .any(|(n, k)| n == "\"complex-key\"" && *k == SymbolKind::Property),
        "{s:?}"
    );
}

#[test]
fn yaml_multi_document_indexes_both_root_keys() {
    let src = "name: alpha\n---\nname: beta\n";
    let s = symbols(src);
    let name_count = s.iter().filter(|(n, _)| n == "name").count();
    // Each document is a separate `document` subtree; both should fire
    // the anchored top-level-keys query.
    assert_eq!(name_count, 2, "{s:?}");
}
