//! Bash grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Bash)
        .expect("bash grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::Bash)
        .expect("bash grammar must load")
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn bash_grammar_loads() {
    extract_symbols_and_imports("", Language::Bash).expect("bash grammar must load on empty input");
}

#[test]
fn bash_extracts_posix_style_function() {
    let s = symbols("greet() {\n    echo hi\n}\n");
    assert!(s.contains(&("greet".into(), SymbolKind::Function)), "{s:?}");
}

#[test]
fn bash_extracts_function_keyword_style() {
    let s = symbols("function deploy {\n    echo deploying\n}\n");
    assert!(
        s.contains(&("deploy".into(), SymbolKind::Function)),
        "{s:?}"
    );
}

#[test]
fn bash_extracts_source_imports() {
    let src = r#"#!/bin/bash
source ./lib/common.sh
. ./helpers.sh
"#;
    let imp = imports(src);
    assert!(imp.iter().any(|n| n.contains("common.sh")), "{imp:?}");
    assert!(imp.iter().any(|n| n.contains("helpers.sh")), "{imp:?}");
}
