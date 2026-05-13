//! Java grammar regression coverage.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Java)
        .expect("java grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::Java)
        .expect("java grammar must load")
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn java_grammar_loads() {
    let _ = extract_symbols_and_imports("", Language::Java)
        .expect("java grammar must load on empty input");
}

#[test]
fn java_class_with_method() {
    let src = "public class Foo {\n    public void bar() {}\n}\n";
    let s = symbols(src);
    assert!(
        s.iter().any(|(n, k)| n == "Foo" && *k == SymbolKind::Class),
        "expected class Foo, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "bar" && *k == SymbolKind::Function),
        "expected method bar, got {s:?}"
    );
}

#[test]
fn java_interface_and_enum() {
    let src = "public interface Greeter { String greet(); }\npublic enum Status { OK, FAIL }\n";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Greeter" && *k == SymbolKind::Interface),
        "expected interface Greeter, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Status" && *k == SymbolKind::Enum),
        "expected enum Status, got {s:?}"
    );
}

#[test]
fn java_imports() {
    let src = "import java.util.List;\nimport com.example.Foo;\nclass C {}\n";
    let imp = imports(src);
    assert!(imp.iter().any(|i| i == "List"), "missing List: {imp:?}");
    assert!(imp.iter().any(|i| i == "Foo"), "missing Foo: {imp:?}");
}

#[test]
fn java_constructor() {
    let src = r#"
        public class Foo {
            public Foo(int x) { }
        }
    "#;
    let s = symbols(src);
    // Constructor name === class name; the query maps it to @fn.name
    let foo_hits: usize = s.iter().filter(|(n, _)| n == "Foo").count();
    assert!(
        foo_hits >= 2,
        "expected Foo to appear twice (class + constructor), got {foo_hits} in {s:?}"
    );
}
