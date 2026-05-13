//! Python grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Python)
        .expect("python grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::Python)
        .expect("python grammar must load")
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn python_grammar_loads() {
    let _ = extract_symbols_and_imports("", Language::Python)
        .expect("python grammar must load on empty input");
}

#[test]
fn python_class_and_function() {
    let src =
        "class User:\n    def name(self):\n        return self.n\n\ndef hello():\n    return 1\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "User" && *k == SymbolKind::Class),
        "expected class User, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "hello" && *k == SymbolKind::Function),
        "expected fn hello, got {s:?}"
    );
}

#[test]
fn python_decorated_function() {
    let src = "@staticmethod\ndef wrap():\n    pass\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "wrap" && *k == SymbolKind::Function),
        "expected decorated fn wrap, got {s:?}"
    );
}

#[test]
fn python_async_function() {
    let src = "async def fetch():\n    return 1\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "fetch" && *k == SymbolKind::Function),
        "expected async fn fetch, got {s:?}"
    );
}

#[test]
fn python_imports() {
    let src = "import os\nfrom collections import OrderedDict\n";
    let imp = imports(src);
    // Plain `import X` -> module name as the captured identifier.
    assert!(imp.iter().any(|i| i == "os"), "missing 'os' in {imp:?}");
    // `from M import N` -> our query captures the module path (dotted_name)
    // at the first identifier. Lock that exact contract — disjunctions
    // ("collections OR OrderedDict") would mask a regression that captured
    // only one side.
    assert!(
        imp.iter().any(|i| i == "collections"),
        "missing 'collections' in {imp:?}"
    );
}
