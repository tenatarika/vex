use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Cpp)
        .unwrap()
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::Cpp)
        .unwrap()
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn cpp_free_function() {
    let s = symbols("int add(int a, int b) { return a + b; }");
    assert!(
        s.iter()
            .any(|(n, k)| n == "add" && *k == SymbolKind::Function),
        "{:?}",
        s
    );
}

#[test]
fn cpp_class_and_struct() {
    let src = "class Foo {};\nstruct Bar {};";
    let s = symbols(src);
    assert!(
        s.iter().any(|(n, k)| n == "Foo" && *k == SymbolKind::Class),
        "{:?}",
        s
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "Bar" && *k == SymbolKind::Struct),
        "{:?}",
        s
    );
}

#[test]
fn cpp_enum_class() {
    let s = symbols("enum class Status { Ok, Err };");
    assert!(
        s.iter()
            .any(|(n, k)| n == "Status" && *k == SymbolKind::Enum),
        "{:?}",
        s
    );
}

#[test]
fn cpp_using_type_alias() {
    let s = symbols("using MyInt = int;");
    assert!(
        s.iter()
            .any(|(n, k)| n == "MyInt" && *k == SymbolKind::TypeAlias),
        "{:?}",
        s
    );
}

#[test]
fn cpp_typedef() {
    let s = symbols("typedef int MyInt;");
    assert!(
        s.iter()
            .any(|(n, k)| n == "MyInt" && *k == SymbolKind::TypeAlias),
        "{:?}",
        s
    );
}

#[test]
fn cpp_function_prototype() {
    let s = symbols("void process(int x);");
    assert!(
        s.iter()
            .any(|(n, k)| n == "process" && *k == SymbolKind::Function),
        "{:?}",
        s
    );
}

#[test]
fn cpp_template_function() {
    let src = "template<typename T>\nT identity(T x) { return x; }";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "identity" && *k == SymbolKind::Function),
        "template fn not captured: {:?}",
        s
    );
}

#[test]
fn cpp_template_class() {
    let src = "template<typename T>\nclass Container {};";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "Container" && *k == SymbolKind::Class),
        "template class not captured: {:?}",
        s
    );
}

#[test]
fn cpp_qualified_method_definition() {
    let src = "void Foo::bar() {}";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "bar" && *k == SymbolKind::Function),
        "qualified method not captured: {:?}",
        s
    );
}

#[test]
fn cpp_includes() {
    let src = "#include \"config.h\"\n#include <vector>\n";
    let imp = imports(src);
    assert!(imp.iter().any(|i| i.contains("config.h")), "{:?}", imp);
    assert!(imp.iter().any(|i| i.contains("vector")), "{:?}", imp);
}

#[test]
fn cpp_destructor_no_panic() {
    // Destructor ~Foo() should not panic — outcome (captured or not) is acceptable
    let src = "class Foo { ~Foo() {} };";
    let _ = symbols(src);
}

#[test]
fn cpp_hierarchy_single_base() {
    use std::fs;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("a.cpp"),
        "class Base {};\nclass Derived : public Base {};\n",
    )
    .unwrap();
    let results = vex::hierarchy::find_implementations(dir.path(), "Base", 10, &[]).unwrap();
    assert!(
        results.iter().any(|m| m.name == "Derived"),
        "Derived not found as inheritor of Base: {:?}",
        results
    );
}

#[test]
fn cpp_hierarchy_multiple_bases_all_matched() {
    // Query matches ALL type_identifier children of base_class_clause, so both A and B should match
    use std::fs;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("a.cpp"),
        "class A {};\nclass B {};\nclass D : public A, protected B {};\n",
    )
    .unwrap();
    let from_a = vex::hierarchy::find_implementations(dir.path(), "A", 10, &[]).unwrap();
    let from_b = vex::hierarchy::find_implementations(dir.path(), "B", 10, &[]).unwrap();
    assert!(
        from_a.iter().any(|m| m.name == "D"),
        "D not found from A: {:?}",
        from_a
    );
    assert!(
        from_b.iter().any(|m| m.name == "D"),
        "D not found from B: {:?}",
        from_b
    );
}
