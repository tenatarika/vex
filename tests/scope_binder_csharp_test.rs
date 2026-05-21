//! Integration tests for the C# scope binder (11.1.5).
//!
//! Same shape as `scope_binder_rust_test.rs` / `scope_binder_-
//! typescript_test.rs`. C# `using` imports + namespace traversal land
//! in a follow-up; names that need imports stay `Unresolved` here.

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef};

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
