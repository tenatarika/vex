//! Lua grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Lua)
        .expect("lua grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::Lua)
        .expect("lua grammar must load")
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn lua_grammar_loads() {
    extract_symbols_and_imports("", Language::Lua).expect("lua grammar must load on empty input");
}

#[test]
fn lua_extracts_top_level_function() {
    let s = symbols("function greet(name) return 'hello ' .. name end\n");
    assert!(s.contains(&("greet".into(), SymbolKind::Function)), "{s:?}");
}

#[test]
fn lua_extracts_local_function() {
    let s = symbols("local function private_helper() return 42 end\n");
    assert!(
        s.contains(&("private_helper".into(), SymbolKind::Function)),
        "{s:?}"
    );
}

#[test]
fn lua_extracts_module_function() {
    let s = symbols("function M.process(x) return x * 2 end\n");
    assert!(
        s.contains(&("process".into(), SymbolKind::Function)),
        "{s:?}"
    );
}

#[test]
fn lua_extracts_method_function() {
    let s = symbols("function Class:method() return self end\n");
    assert!(
        s.contains(&("method".into(), SymbolKind::Function)),
        "{s:?}"
    );
}

#[test]
fn lua_extracts_require_import() {
    let imp = imports("local util = require(\"util\")\n");
    // Quotes are stripped in the extractor's import-name path so that
    // `vex usages util` matches a `require("util")` site.
    assert!(
        imp.iter().any(|n| n == "util"),
        "expected import name to be 'util' without quotes, got {imp:?}"
    );
}

#[test]
fn lua_extracts_require_import_single_quotes() {
    let imp = imports("require('config')\n");
    assert!(
        imp.iter().any(|n| n == "config"),
        "single-quoted require should also strip cleanly, got {imp:?}"
    );
}
