//! TOML grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Toml)
        .expect("toml grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

#[test]
fn toml_grammar_loads() {
    extract_symbols_and_imports("", Language::Toml).expect("toml grammar must load on empty input");
}

#[test]
fn toml_extracts_table_header() {
    let src = r#"[server]
port = 8080
"#;
    let s = symbols(src);
    assert!(s.contains(&("server".into(), SymbolKind::Class)), "{s:?}");
    assert!(s.contains(&("port".into(), SymbolKind::Property)), "{s:?}");
}

#[test]
fn toml_extracts_dotted_table_header() {
    let src = r#"[server.http]
port = 80
"#;
    let s = symbols(src);
    // Dotted keys are captured as a single name; the grammar exposes the
    // whole `server.http` segment as one dotted_key node.
    assert!(
        s.iter()
            .any(|(n, k)| n == "server.http" && *k == SymbolKind::Class),
        "{s:?}"
    );
}

#[test]
fn toml_extracts_table_array() {
    let src = r#"[[products]]
name = "vex"
"#;
    let s = symbols(src);
    assert!(s.contains(&("products".into(), SymbolKind::Class)), "{s:?}");
    assert!(s.contains(&("name".into(), SymbolKind::Property)), "{s:?}");
}

#[test]
fn toml_extracts_top_level_pair() {
    let src = "name = \"vex\"\nversion = \"1.0\"\n";
    let s = symbols(src);
    assert!(s.contains(&("name".into(), SymbolKind::Property)), "{s:?}");
    assert!(
        s.contains(&("version".into(), SymbolKind::Property)),
        "{s:?}"
    );
}
