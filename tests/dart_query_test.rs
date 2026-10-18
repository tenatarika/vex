use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;
use vex::callgraph::extract_call_edges;
use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_references_ast;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn dart_identity_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("a.dart"),
        "class Foo { Foo(); Foo.named(); }\nclass Util { static int computeTotal(int x) => x; }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("b.dart"),
        "int Bar<T>(T value) => value;\nvoid run() {\n  Foo();\n  Util.computeTotal(3);\n  Foo.named();\n  new Foo.named();\n  Bar<int>(1);\n  fetch<int>();\n}\n",
    )
    .unwrap();
    tmp
}

fn query_json(dir: &Path, command: &str, name: &str) -> serde_json::Value {
    let output = vex_in(dir)
        .args([command, name, "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("query stdout is JSON")
}

fn query_json_allow_empty(dir: &Path, command: &str, name: &str) -> serde_json::Value {
    let output = vex_in(dir)
        .args([command, name, "--format", "json"])
        .output()
        .unwrap();
    serde_json::from_slice(&output.stdout).expect("query stdout is JSON")
}

fn result_names(json: &serde_json::Value) -> Vec<String> {
    json["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["name"].as_str().map(str::to_owned))
        .collect()
}

fn assert_dart_callgraph_queries(dir: &Path, expected_named_calls: usize) {
    for name in ["Foo", "computeTotal", "Foo.named"] {
        let callers = result_names(&query_json(dir, "callers", name));
        assert!(
            callers.iter().any(|caller| caller == "run"),
            "callers({name}) = {callers:?}"
        );
    }

    let callees = result_names(&query_json(dir, "callees", "run"));
    for name in ["Foo", "computeTotal", "Foo.named", "Bar", "fetch"] {
        assert!(
            callees.iter().any(|callee| callee == name),
            "callees(run) missing {name}: {callees:?}"
        );
    }
    for unexpected in ["Foo.new", "Util.computeTotal", "Bar.new", "fetch.new"] {
        assert!(
            !callees.iter().any(|callee| callee == unexpected),
            "callees(run) has {unexpected}: {callees:?}"
        );
    }
    let named_calls = callees
        .iter()
        .filter(|callee| callee.as_str() == "Foo.named")
        .count();
    assert_eq!(
        named_calls, expected_named_calls,
        "named constructor call projection differs by query mode: {callees:?}"
    );
}

fn assert_dart_usage_queries(dir: &Path) {
    let usages = query_json(dir, "usages", "Foo.named");
    let usage_rows = usages["results"].as_array().unwrap();
    assert_eq!(usage_rows.len(), 2, "usages(Foo.named) = {usage_rows:?}");
    assert!(usage_rows.iter().all(|row| row["path"] == "b.dart"));

    assert_eq!(
        query_json(dir, "usages", "computeTotal")["results"],
        serde_json::json!([{"path":"b.dart","line":4}]),
        "static method usages use the bare name"
    );
}

fn dart() -> Language {
    Language::from_extension("dart").expect(".dart must be registered")
}

#[test]
fn dart_grammar_loads() {
    extract_symbols_and_imports("", dart()).expect("Dart grammar must load");
}

#[test]
fn dart_record_syntax_fixture_has_no_tree_errors() {
    let source =
        "(int, String) pair = (1, 'item');\nvoid unpack() { final (count, label) = pair; }\n";
    let language: tree_sitter::Language = tree_sitter_dart_orchard::LANGUAGE.into();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "record type, literal, and destructuring fixture had parse errors: {}",
        tree.root_node().to_sexp()
    );
}

#[test]
fn dart_symbols_and_imports() {
    let source = include_str!("fixtures/sample.dart");
    let (symbols, imports) = extract_symbols_and_imports(source, dart()).unwrap();
    for name in ["Repository", "Logging", "Service"] {
        assert!(
            symbols
                .iter()
                .any(|s| s.name == name && s.kind == SymbolKind::Class),
            "missing class {name}: {symbols:?}"
        );
    }
    for name in ["helper", "run", "save", "refresh"] {
        assert!(
            symbols
                .iter()
                .any(|s| s.name == name
                    && matches!(s.kind, SymbolKind::Function | SymbolKind::Method)),
            "missing callable {name}: {symbols:?}"
        );
    }
    assert!(imports.iter().any(|r| r.name == "dart:async"));
}

#[test]
fn dart_unnamed_constructor_and_type_kinds_are_distinct() {
    let (symbols, _) =
        extract_symbols_and_imports("class Repository { Repository(); }", dart()).unwrap();
    assert!(symbols
        .iter()
        .any(|s| s.name == "Repository" && s.kind == SymbolKind::Class));
    assert!(symbols
        .iter()
        .any(|s| s.name == "Repository" && s.kind == SymbolKind::Method));
}

#[test]
fn dart_constructor_names_refs_and_callers_use_declared_identity() {
    let source = "class Repo { Repo(); Repo.named(Object value); factory Repo.make() { return Repo.named(helper()); } void named() {} } class OtherRepo { factory OtherRepo.make() { return Repo.named(helper()); } } int helper() => 1; void run() { Repo(); Repo.named(helper()); new Repo.named(helper()); const Repo.named(helper()); value.named(); value.loadNamed(); }";
    let parsed = vex::parse::parse_file("sample.dart", source, dart()).unwrap();
    for name in ["Repo", "Repo.named", "Repo.make"] {
        assert!(
            parsed.symbols.iter().any(|s| s.name == name),
            "missing qualified constructor {name}: {:?}",
            parsed.symbols
        );
    }
    assert!(
        parsed.symbols.iter().any(|s| s.name == "named"),
        "ordinary method keeps its bare name: {:?}",
        parsed.symbols
    );
    let refs = extract_references_ast(source, dart()).unwrap();
    assert!(refs.iter().any(|r| r.name == "Repo"), "{refs:?}");
    assert!(refs.iter().any(|r| r.name == "Repo.named"), "{refs:?}");
    assert_eq!(
        refs.iter().filter(|r| r.name == "named").count(),
        0,
        "bare constructor suffixes do not enter the refs index: {refs:?}"
    );
    assert!(refs.iter().any(|r| r.name == "loadNamed"));
    assert!(
        parsed
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name == "Repo.make" && e.callee_name == "helper"),
        "factory helper caller is qualified: {:?}",
        parsed.call_edges
    );
    assert!(
        parsed
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name == "OtherRepo.make" && e.callee_name == "Repo.named"),
        "sibling factory caller is qualified: {:?}",
        parsed.call_edges
    );
    assert!(
        parsed
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name == "Repo.make" && e.callee_name == "Repo.named"),
        "factory constructor call is qualified: {:?}",
        parsed.call_edges
    );
    assert_eq!(
        parsed
            .call_edges
            .iter()
            .filter(|e| e.caller_fn_name == "Repo.make" && e.callee_name == "Repo.named")
            .count(),
        1,
        "nested helper argument must not duplicate the constructor edge: {:?}",
        parsed.call_edges
    );
    assert!(
        parsed
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name == "Repo.make" && e.callee_name == "helper"),
        "nested ordinary call keeps its own edge: {:?}",
        parsed.call_edges
    );
}

#[test]
fn dart_calls_keep_function_ownership() {
    let source = include_str!("fixtures/sample.dart");
    let edges = extract_call_edges(source, dart());
    for (caller, callee) in [
        ("save", "helper"),
        ("save", "log"),
        ("refresh", "helper"),
        ("run", "save"),
        ("run", "refresh"),
    ] {
        assert!(
            edges
                .iter()
                .any(|(from, _, to, _)| from == caller && to == callee),
            "missing {caller}->{callee}: {edges:?}"
        );
    }
}

#[test]
fn dart_calls_exclude_bare_identifiers_and_comments() {
    let source = "void run() { final value = existing; print(value); /* ghost(); */ final text = 'fake()'; }";
    let edges = extract_call_edges(source, dart());
    assert!(edges
        .iter()
        .any(|(from, _, to, _)| from == "run" && to == "print"));
    for unwanted in ["existing", "value", "ghost", "fake"] {
        assert!(
            !edges.iter().any(|(_, _, to, _)| to == unwanted),
            "spurious {unwanted}: {edges:?}"
        );
    }
}

#[test]
fn dart_generic_and_conditional_calls() {
    let source = "void run(Client? client) { client?.fetch(); Future<void>.delayed(Duration.zero); List<int>.of([]); Box<int>(); new Crate<int>(); const Item<int>(); }";
    let edges = extract_call_edges(source, dart());
    for name in ["fetch", "Future.delayed", "List.of", "Box", "Crate", "Item"] {
        assert!(
            edges
                .iter()
                .any(|(from, _, to, _)| from == "run" && to == name),
            "missing {name}: {edges:?}"
        );
    }
}

#[test]
fn dart_initializer_calls_belong_to_constructor() {
    let source = "int helper() => 1; class C { final int field; C() : field = helper(); }";
    let edges = extract_call_edges(source, dart());
    assert!(
        edges
            .iter()
            .any(|(from, _, to, _)| from == "C" && to == "helper"),
        "constructor owner lost: {edges:?}"
    );
}

#[test]
fn dart_enum_and_mixin_hierarchy() {
    use vex::parse::parse_file;
    use vex::store::format::EdgeKind;
    let source = "abstract class Contract {} mixin Logging {} mixin Worker implements Contract {} enum State with Logging implements Contract { ready; }";
    let parsed = parse_file("sample.dart", source, dart()).unwrap();
    for (child, base, kind) in [
        ("Worker", "Contract", EdgeKind::Implements),
        ("State", "Contract", EdgeKind::Implements),
        ("State", "Logging", EdgeKind::Uses),
    ] {
        assert!(
            parsed
                .hierarchy_captures
                .iter()
                .any(|c| c.child_name == child && c.parent_name == base && c.kind == kind as u8),
            "missing {child}->{base}: {:?}",
            parsed.hierarchy_captures
        );
    }
}

#[test]
fn dart_body_tokens_include_function_body() {
    let (symbols, _) =
        extract_symbols_and_imports(include_str!("fixtures/sample.dart"), dart()).unwrap();
    let refresh = symbols.iter().find(|s| s.name == "refresh").unwrap();
    assert!(
        refresh
            .body_tokens
            .as_deref()
            .unwrap_or("")
            .contains("helper"),
        "body tokens lost: {refresh:?}"
    );
}

#[test]
fn dart_type_aliases_do_not_capture_return_types() {
    let source =
        "typedef Items = List<int>; typedef Callback = void Function(); typedef int Legacy();";
    let (symbols, _) = extract_symbols_and_imports(source, dart()).unwrap();
    let aliases: Vec<_> = symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::TypeAlias)
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(aliases, ["Items", "Callback", "Legacy"]);
}

#[test]
fn dart_hierarchy_keeps_relation_kinds() {
    use vex::parse::parse_file;
    use vex::store::format::EdgeKind;
    let source = "class Base {} abstract class Contract {} mixin Logging {} class Child extends Base with Logging implements Contract {}";
    let parsed = parse_file("sample.dart", source, dart()).unwrap();
    for (base, kind) in [
        ("Base", EdgeKind::Extends),
        ("Contract", EdgeKind::Implements),
        ("Logging", EdgeKind::Uses),
    ] {
        assert!(
            parsed
                .hierarchy_captures
                .iter()
                .any(|c| c.child_name == "Child" && c.parent_name == base && c.kind == kind as u8),
            "missing Child->{base}: {:?}",
            parsed.hierarchy_captures
        );
    }
}

#[test]
fn dart_ast_refs_keep_interpolation_and_drop_literal_text() {
    use vex::parse::extractor::extract_references_ast;
    let source = "void run() { final text = 'literalGhost ${actualValue}'; // commentGhost\n }";
    let refs = extract_references_ast(source, dart()).unwrap();
    assert!(refs.iter().any(|r| r.name == "actualValue"));
    assert!(!refs
        .iter()
        .any(|r| r.name == "literalGhost" || r.name == "commentGhost"));
}

#[test]
fn dart_static_method_calls_use_bare_identity_and_keep_owner() {
    let source = "int helper() => 1; class Util { static int computeTotal() => helper(); } void invoke() { Util.computeTotal(); }";
    let parsed = vex::parse::parse_file("sample.dart", source, dart()).unwrap();

    assert!(
        parsed.symbols.iter().any(|s| s.name == "computeTotal"),
        "static method uses its bare symbol identity: {:?}",
        parsed.symbols
    );
    assert!(
        parsed
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name == "computeTotal" && e.callee_name == "helper"),
        "helper call belongs to the bare static method: {:?}",
        parsed.call_edges
    );
    assert!(
        parsed
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name == "invoke" && e.callee_name == "computeTotal"),
        "static method call uses bare callee identity: {:?}",
        parsed.call_edges
    );
}

#[test]
fn dart_top_level_uppercase_function_call_stays_bare() {
    let source = "int Foo() => 1; void invoke() { Foo(); }";
    let parsed = vex::parse::parse_file("sample.dart", source, dart()).unwrap();

    assert!(
        parsed.symbols.iter().any(|s| s.name == "Foo"),
        "top-level function keeps its declared name: {:?}",
        parsed.symbols
    );
    assert!(
        parsed
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name == "invoke" && e.callee_name == "Foo"),
        "top-level uppercase function call stays bare: {:?}",
        parsed.call_edges
    );
    assert!(
        !parsed.call_edges.iter().any(|e| e.callee_name == "Foo.new"),
        "uppercase function call is not rewritten as a constructor: {:?}",
        parsed.call_edges
    );
}

#[test]
fn dart_constructor_and_instance_method_identities_are_preserved_together() {
    let source = "int helper() => 1; class Owner { Owner(); Owner.named(); int foo() => helper(); } void invoke(Owner value) { Owner(); Owner.named(); value.foo(); }";
    let parsed = vex::parse::parse_file("sample.dart", source, dart()).unwrap();

    for name in ["Owner", "Owner.named"] {
        assert!(
            parsed.symbols.iter().any(|s| s.name == name),
            "missing qualified constructor {name}: {:?}",
            parsed.symbols
        );
        assert!(
            parsed
                .call_edges
                .iter()
                .any(|e| e.caller_fn_name == "invoke" && e.callee_name == name),
            "constructor call uses qualified identity {name}: {:?}",
            parsed.call_edges
        );
    }
    assert!(
        parsed
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name == "invoke" && e.callee_name == "foo"),
        "instance call remains bare: {:?}",
        parsed.call_edges
    );
    assert!(
        parsed.symbols.iter().any(|s| s.name == "foo"),
        "ordinary instance method keeps its bare name: {:?}",
        parsed.symbols
    );
}

#[test]
fn dart_lowercase_local_constructor_keeps_qualified_identity() {
    let source = "class service_box { service_box(); service_box.named(); static int computeTotal() => 1; int ordinaryCall() => 2; } void invoke(service_box targetValue) { service_box.named(); service_box.computeTotal(); targetValue.ordinaryCall(); }";
    let parsed = vex::parse::parse_file("sample.dart", source, dart()).unwrap();
    let refs = extract_references_ast(source, dart()).unwrap();

    assert!(
        refs.iter().any(|r| r.name == "service_box.named"),
        "known lowercase constructor ref keeps its qualified identity: {refs:?}"
    );
    for name in ["computeTotal", "ordinaryCall"] {
        assert!(
            refs.iter().any(|r| r.name == name),
            "member ref uses bare identity {name}: {refs:?}"
        );
        assert!(
            parsed
                .call_edges
                .iter()
                .any(|e| e.caller_fn_name == "invoke" && e.callee_name == name),
            "member call uses bare identity {name}: {:?}",
            parsed.call_edges
        );
    }
    for name in ["service_box.computeTotal", "targetValue.ordinaryCall"] {
        assert!(
            !refs.iter().any(|r| r.name == name),
            "member ref must not retain owner qualification {name}: {refs:?}"
        );
        assert!(
            !parsed.call_edges.iter().any(|e| e.callee_name == name),
            "member call must not retain owner qualification {name}: {:?}",
            parsed.call_edges
        );
    }
    assert!(
        parsed
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name == "invoke" && e.callee_name == "service_box.named"),
        "known lowercase constructor call keeps qualified identity: {:?}",
        parsed.call_edges
    );
}

#[test]
fn dart_unresolved_generic_function_call_stays_bare() {
    let edges = extract_call_edges("void invoke() { fetch<int>(); }", dart());
    assert!(
        edges
            .iter()
            .any(|(from, _, to, _)| from == "invoke" && to == "fetch"),
        "generic function call must not become a constructor: {edges:?}"
    );
    assert!(!edges.iter().any(|(_, _, to, _)| to == "fetch.new"));
}

#[test]
fn dart_cross_file_names_match_in_indexed_and_live_queries() {
    let indexed = dart_identity_fixture();
    vex_in(indexed.path())
        .args([
            "index",
            "--no-semantic",
            "--no-clusters",
            "--format",
            "json",
        ])
        .assert()
        .success();
    assert_dart_callgraph_queries(indexed.path(), 2);
    assert_dart_usage_queries(indexed.path());

    let live = dart_identity_fixture();
    let index_path = live.path().join(".vex-test-cache/index.vex");
    assert!(
        !index_path.exists(),
        "live fixture must start without an index"
    );
    assert_dart_callgraph_queries(live.path(), 1);
    assert!(
        !index_path.exists(),
        "live callgraph queries must not bootstrap an index"
    );
}

fn assert_cross_file_uppercase_function_names(dir: &Path) {
    let callers_foo = result_names(&query_json(dir, "callers", "Foo"));
    assert_eq!(callers_foo, ["run"]);

    let callers_mystery = result_names(&query_json(dir, "callers", "Mystery"));
    assert_eq!(callers_mystery, ["run"]);

    let callees = result_names(&query_json(dir, "callees", "run"));
    for name in ["Foo", "Mystery"] {
        assert_eq!(
            callees
                .iter()
                .filter(|callee| callee.as_str() == name)
                .count(),
            1,
            "expected one bare {name} callee: {callees:?}"
        );
    }
    for unexpected in ["Foo.new", "Mystery.new"] {
        assert!(
            !callees.iter().any(|callee| callee == unexpected),
            "unexpected qualified no-member call {unexpected}: {callees:?}"
        );
    }
}

#[test]
fn dart_uppercase_no_member_calls_are_bare_across_files() {
    let make_fixture = || {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
        std::fs::write(tmp.path().join("a.dart"), "int Foo() => 1;\n").unwrap();
        std::fs::write(
            tmp.path().join("b.dart"),
            "void run() { Foo(); Mystery<int>(); }\n",
        )
        .unwrap();
        tmp
    };

    let indexed = make_fixture();
    vex_in(indexed.path())
        .args([
            "index",
            "--no-semantic",
            "--no-clusters",
            "--format",
            "json",
        ])
        .assert()
        .success();
    assert_cross_file_uppercase_function_names(indexed.path());

    let live = make_fixture();
    let index_path = live.path().join(".vex-test-cache/index.vex");
    assert!(
        !index_path.exists(),
        "live fixture must start without an index"
    );
    assert_cross_file_uppercase_function_names(live.path());
    assert!(
        !index_path.exists(),
        "live queries must not bootstrap an index"
    );
}

#[test]
fn dart_incremental_rebind_uses_unchanged_callsite_source() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("a.dart"),
        "class Foo { static int named() => 1; }\n",
    )
    .unwrap();
    let callsite = "void run() { Foo.named(); }\n";
    std::fs::write(tmp.path().join("b.dart"), callsite).unwrap();
    vex_in(tmp.path())
        .args([
            "index",
            "--no-semantic",
            "--no-clusters",
            "--format",
            "json",
        ])
        .assert()
        .success();

    let static_callers = result_names(&query_json(tmp.path(), "callers", "named"));
    assert!(static_callers.iter().any(|caller| caller == "run"));
    let constructor_callers =
        result_names(&query_json_allow_empty(tmp.path(), "callers", "Foo.named"));
    assert!(!constructor_callers.iter().any(|caller| caller == "run"));

    std::fs::write(tmp.path().join("a.dart"), "class Foo { Foo.named(); }\n").unwrap();
    vex_in(tmp.path())
        .args(["update", "--no-semantic", "--format", "json"])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("b.dart")).unwrap(),
        callsite
    );
    let constructor_callers = result_names(&query_json(tmp.path(), "callers", "Foo.named"));
    assert!(constructor_callers.iter().any(|caller| caller == "run"));
    let static_callers = result_names(&query_json_allow_empty(tmp.path(), "callers", "named"));
    assert!(!static_callers.iter().any(|caller| caller == "run"));
}
