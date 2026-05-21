//! Integration tests for the C++ scope binder (11.1.5).

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef};

fn bind(src: &str) -> (Vec<ParsedSymbol>, Vec<BoundRef>) {
    let (symbols, _) =
        extract_symbols_and_imports(src, Language::Cpp).expect("c++ grammar must load");
    let refs = bind_refs(src, Language::Cpp, &symbols).expect("binder must not fail");
    (symbols, refs)
}

fn find_ref<'a>(refs: &'a [BoundRef], name: &str, line: usize) -> &'a BoundRef {
    refs.iter()
        .find(|r| r.name == name && r.line == line)
        .unwrap_or_else(|| panic!("no ref `{name}` at line {line} in {refs:?}"))
}

#[test]
fn method_local_var_binds_to_local() {
    let src = "class Holder {\npublic:\n    int Add(int leftOp, int rightOp) {\n        int valueOne = leftOp;\n        return valueOne + rightOp;\n    }\n};\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "valueOne", 5);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "expected Local, got {:?}",
        r.target
    );
}

#[test]
fn method_parameter_binds_to_local() {
    let src = "class Holder {\npublic:\n    int Add(int leftOp, int rightOp) {\n        return leftOp + rightOp;\n    }\n};\n";
    let (_syms, refs) = bind(src);
    let l = find_ref(&refs, "leftOp", 4);
    assert!(matches!(l.target, BindTarget::Local(_)));
    let r = find_ref(&refs, "rightOp", 4);
    assert!(matches!(r.target, BindTarget::Local(_)));
}

#[test]
fn top_level_class_resolves_to_module_symbol() {
    let src = "class PaymentGateway {};\nclass Holder {\npublic:\n    void Run(PaymentGateway p) {}\n};\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "PaymentGateway", 4);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "PaymentGateway");
}

#[test]
fn top_level_struct_resolves_to_module_symbol() {
    let src = "struct UserData {};\nclass Holder {\npublic:\n    void Run(UserData p) {}\n};\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "UserData", 4);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "UserData");
}

#[test]
fn unknown_name_is_unresolved() {
    let src = "class Holder {\npublic:\n    void Run() { ghost_name(); }\n};\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "ghost_name", 3);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "expected Unresolved, got {:?}",
        r.target
    );
}

#[test]
fn refs_skip_comment_noise() {
    let src = "class Holder {\npublic:\n    // FakeMention is just a comment\n    void Run() { real_function(); }\n};\n";
    let (_syms, refs) = bind(src);
    assert!(
        !refs.iter().any(|r| r.name == "FakeMention"),
        "comment ident leaked: {refs:?}"
    );
    assert!(
        refs.iter().any(|r| r.name == "real_function"),
        "real ident missing: {refs:?}"
    );
}
