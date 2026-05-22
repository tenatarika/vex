//! Integration tests for the C# scope binder (11.1.5).
//!
//! Same shape as `scope_binder_rust_test.rs` / `scope_binder_-
//! typescript_test.rs`. C# `using` imports + namespace traversal land
//! in a follow-up; names that need imports stay `Unresolved` here.

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef, UsePath};

fn bind(src: &str) -> (Vec<ParsedSymbol>, Vec<BoundRef>) {
    let (symbols, _) =
        extract_symbols_and_imports(src, Language::CSharp).expect("c# grammar must load");
    let refs = bind_refs(src, Language::CSharp, &symbols).expect("binder must not fail");
    (symbols, refs)
}

fn find_ref<'a>(refs: &'a [BoundRef], name: &str, line: usize) -> &'a BoundRef {
    refs.iter()
        .find(|r| r.name == name && r.line == line)
        .unwrap_or_else(|| panic!("no ref `{name}` at line {line} in {refs:?}"))
}

#[test]
fn method_local_var_binds_to_local() {
    let src = "class Holder {\n    void Run() {\n        var valueOne = 1;\n        var _x = valueOne;\n    }\n}\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "valueOne", 4);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "expected Local, got {:?}",
        r.target
    );
}

#[test]
fn method_parameter_binds_to_local() {
    let src =
        "class Holder {\n    int Add(int leftOp, int rightOp) { return leftOp + rightOp; }\n}\n";
    let (_syms, refs) = bind(src);
    let l = find_ref(&refs, "leftOp", 2);
    assert!(matches!(l.target, BindTarget::Local(_)));
    let r = find_ref(&refs, "rightOp", 2);
    assert!(matches!(r.target, BindTarget::Local(_)));
}

#[test]
fn top_level_class_resolves_to_module_symbol() {
    let src = "class PaymentGateway {}\nclass Holder {\n    void Run(PaymentGateway p) {}\n}\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "PaymentGateway", 3);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "PaymentGateway");
}

#[test]
fn top_level_interface_resolves_to_module_symbol() {
    let src = "interface UserData {}\nclass Holder {\n    void Run(UserData p) {}\n}\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "UserData", 3);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "UserData");
}

#[test]
fn unknown_name_is_unresolved() {
    let src = "class Holder {\n    void Run() { ghostName(); }\n}\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "ghostName", 2);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "expected Unresolved, got {:?}",
        r.target
    );
}

#[test]
fn using_simple_binds_tail_as_imported() {
    let src = "using App.Lib.Gateway;\nclass Holder {\n    void Run() { Gateway.Charge(); }\n}\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Gateway", 3);
    let path = match &r.target {
        BindTarget::Imported(p) => p.clone(),
        other => panic!("expected Imported, got {other:?}"),
    };
    assert_eq!(
        path,
        UsePath {
            segments: vec!["App".into(), "Lib".into(), "Gateway".into()],
        }
    );
}

#[test]
fn using_alias_binds_alias_name() {
    let src = "using Dict = System.Collections.Generic.Dictionary;\nclass Holder {\n    void Run() { var d = new Dict(); }\n}\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Dict", 3);
    let path = match &r.target {
        BindTarget::Imported(p) => p.clone(),
        other => panic!("expected Imported (alias), got {other:?}"),
    };
    assert_eq!(
        path.segments,
        vec![
            "System".to_string(),
            "Collections".into(),
            "Generic".into(),
            "Dictionary".into()
        ]
    );
}

#[test]
fn using_static_binds_tail_class() {
    let src = "using static System.Math;\nclass Holder {\n    void Run() { var x = Math.Pi; }\n}\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Math", 3);
    let path = match &r.target {
        BindTarget::Imported(p) => p.clone(),
        other => panic!("expected Imported (static), got {other:?}"),
    };
    assert_eq!(
        path.segments,
        vec!["System".to_string(), "Math".to_string()]
    );
}

#[test]
fn global_using_binds_like_plain_using() {
    let src =
        "global using App.Lib.Gateway;\nclass Holder {\n    void Run() { Gateway.Charge(); }\n}\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Gateway", 3);
    assert!(
        matches!(r.target, BindTarget::Imported(_)),
        "expected Imported, got {:?}",
        r.target
    );
}

#[test]
fn using_global_colon_qualifier_is_stripped_from_path() {
    // Reviewer H2: `using global::App.Lib.Gateway;` parses as a
    // `qualified_name` whose leftmost segment is `alias_qualified_name
    // global::App`. The original text-split implementation produced
    // `["global::App", "Lib", "Gateway"]`; the AST-walk path must
    // strip `global::` and yield the canonical 3-segment path.
    let src = "using global::App.Lib.Gateway;\nclass Holder { void Run() { Gateway.Charge(); } }\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Gateway", 2);
    let path = match &r.target {
        BindTarget::Imported(p) => p.clone(),
        other => panic!("expected Imported, got {other:?}"),
    };
    assert_eq!(
        path.segments,
        vec!["App".to_string(), "Lib".into(), "Gateway".into()],
        "global:: prefix must not leak into segments",
    );
}

#[test]
fn extern_alias_directive_does_not_leak_phantom_ref() {
    // `extern alias MyAlias;` is a declaration, not a reference.
    // Pre-fix the bare `MyAlias` identifier inside the directive
    // surfaced as an `Unresolved` ref at line 1.
    let src = "extern alias MyAlias;\nclass C {}\n";
    let (_syms, refs) = bind(src);
    assert!(
        !refs.iter().any(|r| r.name == "MyAlias" && r.line == 1),
        "extern alias declaration must not emit a phantom ref: {refs:?}",
    );
}

#[test]
fn using_path_identifiers_do_not_become_refs() {
    // 11.1.1 plus the new import handler must not emit refs for
    // namespace segments — they're a binding site only.
    let src = "using App.Lib.Gateway;\nclass C {}\n";
    let (_syms, refs) = bind(src);
    for seg in ["App", "Lib"] {
        assert!(
            !refs.iter().any(|r| r.name == seg && r.line == 1),
            "namespace segment `{seg}` leaked into refs at line 1: {refs:?}",
        );
    }
}

#[test]
fn refs_skip_comment_noise() {
    // Sanity check that the 11.1.1 AST filter is active for C#.
    let src = "class Holder {\n    // FakeMention is just a comment\n    void Run() { realFunction(); }\n}\n";
    let (_syms, refs) = bind(src);
    assert!(
        !refs.iter().any(|r| r.name == "FakeMention"),
        "comment ident leaked into refs: {refs:?}"
    );
    assert!(
        refs.iter().any(|r| r.name == "realFunction"),
        "real ident missing: {refs:?}"
    );
}
