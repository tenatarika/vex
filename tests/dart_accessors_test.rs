use assert_cmd::Command;
use serde_json::{json, Value};
use std::path::Path;
use tempfile::TempDir;
use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn command(root: &Path) -> Command {
    let mut command = Command::cargo_bin("vex").unwrap();
    command
        .current_dir(root)
        .env("VEX_CACHE_DIR", root.join(".vex-test-cache"));
    command
}

fn assert_references(source: &str, name: &str, lines: &[u32]) {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(root.path().join("lib.dart"), source).unwrap();
    command(root.path()).arg("index").assert().success();
    let output = command(root.path())
        .args(["usages", name, "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    let expected: Vec<_> = lines
        .iter()
        .map(|line| json!({"path": "lib.dart", "line": line}))
        .collect();
    assert_eq!(result["results"], json!(expected), "wrong refs for {name}");
}

#[test]
fn getter_and_setter_definitions_keep_method_kind() {
    let (symbols, _) = extract_symbols_and_imports(
        "class Box { int get sharedValue => 1; set sharedValue(int next) {} }",
        Language::Dart,
    )
    .unwrap();
    let accessors: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.name == "sharedValue")
        .collect();
    assert_eq!(accessors.len(), 2, "both accessor definitions are retained");
    assert!(accessors
        .iter()
        .all(|symbol| symbol.kind == SymbolKind::Method));
}

#[test]
fn paired_accessors_index_reads_writes_and_compound_assignments() {
    assert_references(
        "class Box {\n  int _value = 0;\n  int get sharedValue => _value;\n  set sharedValue(int next) { _value = next; }\n}\nvoid caller() {\n  final box = Box();\n  print(box.sharedValue);\n  box.sharedValue = 1;\n  box.sharedValue += 2;\n  final emoji = '🦊'; print(box.sharedValue);\n  dynamic unknown = box; unknown.sharedValue = 3;\n  final sharedValue = 4; print(sharedValue);\n}\n",
        "sharedValue",
        &[8, 9, 10, 11, 12, 13, 13],
    );
}

#[test]
fn getter_only_accessor_indexes_reads() {
    assert_references(
        "class Box { int get sharedValue => 1; }\nvoid caller() {\n  final box = Box();\n  print(box.sharedValue);\n  final emoji = '🦊'; print(box.sharedValue);\n  dynamic unknown = box; print(unknown.sharedValue);\n  final sharedValue = 2; print(sharedValue);\n}\n",
        "sharedValue",
        &[4, 5, 6, 7, 7],
    );
}

#[test]
fn setter_only_accessor_indexes_writes() {
    assert_references(
        "class Box { set sharedValue(int next) {} }\nvoid caller() {\n  final box = Box();\n  box.sharedValue = 1;\n  final emoji = '🦊'; box.sharedValue = 2;\n  dynamic unknown = box; unknown.sharedValue = 3;\n  var sharedValue = 4; sharedValue = 5;\n}\n",
        "sharedValue",
        &[4, 5, 6, 7, 7],
    );
}

#[test]
fn top_level_accessors_are_indexed() {
    assert_references(
        "int _value = 0;\nint get globalValue => _value;\nset globalValue(int next) { _value = next; }\nvoid caller() {\n  print(globalValue);\n  globalValue = 1;\n  final emoji = '🦊'; globalValue += 2;\n}\nvoid shadow() { final globalValue = 5; print(globalValue); }\n",
        "globalValue",
        &[5, 6, 7, 9, 9],
    );
}

#[test]
fn same_line_same_name_accessors_preserve_distinct_owner_rows() {
    assert_references(
        "class A { int get sharedValue => 1; set sharedValue(int next) {} } class B { int get sharedValue => 2; set sharedValue(int next) {} }\nvoid caller() {\n  final a = A(); final b = B();\n  print(a.sharedValue);\n  a.sharedValue = 1;\n  print(b.sharedValue);\n  b.sharedValue = 2;\n  final emoji = '🦊'; print(a.sharedValue); print(b.sharedValue);\n  dynamic unknown = a; unknown.sharedValue = 3;\n  final sharedValue = 4; print(sharedValue);\n}\n",
        "sharedValue",
        &[4, 5, 6, 7, 8, 8, 9, 10, 10],
    );
}

#[test]
fn many_owner_accessor_unions_span_multiple_batches() {
    let mut source: String = (0..24)
        .map(|owner| {
            format!("class Box{owner} {{ int get sharedValue => {owner}; set sharedValue(int next) {{}} }}\n")
        })
        .collect();
    source.push_str("void caller() {\n");
    for owner in 0..24 {
        source.push_str(&format!("  Box{owner}().sharedValue = {owner};\n"));
    }
    source.push_str("}\n");
    assert_references(&source, "sharedValue", &(26..50).collect::<Vec<_>>());
}

#[test]
fn lowercase_accessor_name_obeys_common_reference_filter() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        root.path().join("lib.dart"),
        "class Box { int get value => 1; }\nvoid caller() { print(Box().value); }\n",
    )
    .unwrap();
    command(root.path()).arg("index").assert().success();
    let output = command(root.path())
        .args(["usages", "value", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["results"], json!([]));
}
