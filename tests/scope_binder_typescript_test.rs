//! Integration tests for the TypeScript scope binder (11.1.4a).
//!
//! Mirrors `scope_binder_rust_test.rs` for the cases that 11.1.4a is
//! required to handle. Module imports + JSX + declaration merging
//! land in 11.1.4b/c — names that need imports stay `Unresolved` here.

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef};

fn bind(src: &str) -> (Vec<ParsedSymbol>, Vec<BoundRef>) {
    let (symbols, _) = extract_symbols_and_imports(src, Language::TypeScript)
        .expect("typescript grammar must load");
    let refs = bind_refs(src, Language::TypeScript, &symbols).expect("binder must not fail");
    (symbols, refs)
}

fn find_ref<'a>(refs: &'a [BoundRef], name: &str, line: usize) -> &'a BoundRef {
    refs.iter()
        .find(|r| r.name == name && r.line == line)
        .unwrap_or_else(|| panic!("no ref `{name}` at line {line} in {refs:?}"))
}

#[test]
fn fn_local_const_binds_to_local() {
    let src = "function run() {\n    const valueOne = 1;\n    const _x = valueOne;\n}\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "valueOne", 3);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "expected Local, got {:?}",
        r.target
    );
}

#[test]
fn fn_parameter_binds_to_local() {
    let src =
        "function add(leftOp: number, rightOp: number): number { return leftOp + rightOp; }\n";
    let (_syms, refs) = bind(src);
    let l = find_ref(&refs, "leftOp", 1);
    assert!(
        matches!(l.target, BindTarget::Local(_)),
        "expected Local for fn param `leftOp`, got {:?}",
        l.target
    );
    let r = find_ref(&refs, "rightOp", 1);
    assert!(matches!(r.target, BindTarget::Local(_)));
}

#[test]
fn top_level_class_resolves_to_module_symbol() {
    let src = "class PaymentGateway {}\nfunction run(p: PaymentGateway) {}\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "PaymentGateway", 2);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "PaymentGateway");
}

#[test]
fn top_level_interface_resolves_to_module_symbol() {
    let src = "interface UserData {}\nfunction run(p: UserData) {}\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "UserData", 2);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "UserData");
}

#[test]
fn top_level_type_alias_resolves_to_module_symbol() {
    let src = "type UserId = string;\nfunction run(p: UserId) {}\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "UserId", 2);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "UserId");
}

#[test]
fn unknown_name_is_unresolved() {
    let src = "function run() { ghostName(); }\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "ghostName", 1);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "expected Unresolved, got {:?}",
        r.target
    );
}

#[test]
fn inner_block_shadow_wins_for_inner_ref() {
    let src = "function run() {\n    const shadowedVar = 1;\n    {\n        const shadowedVar = 2;\n        const _x = shadowedVar;\n    }\n}\n";
    let (_syms, refs) = bind(src);
    let inner = find_ref(&refs, "shadowedVar", 5);
    let resolved_scope = match &inner.target {
        BindTarget::Local(sid) => *sid,
        other => panic!("expected Local, got {other:?}"),
    };
    // Inner block scope is not the file root (0); the shadowing const
    // wins for the line-5 ref.
    assert_ne!(resolved_scope, 0, "must not resolve to file root");
}

// --- 11.1.6: ES module imports ---

fn imported_path(refs: &[BoundRef], name: &str, line: usize) -> Vec<String> {
    let r = find_ref(refs, name, line);
    match &r.target {
        BindTarget::Imported(p) => p.segments.clone(),
        other => panic!("expected Imported for `{name}` line {line}, got {other:?}"),
    }
}

#[test]
fn import_named_binds_to_imported() {
    let src = "import { someExtFn } from './ext';\nfunction run() { someExtFn(); }\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "someExtFn", 2);
    assert_eq!(path, vec!["./ext", "someExtFn"]);
}

#[test]
fn import_named_alias_records_original_path_under_alias() {
    let src = "import { someExtFn as renamedFn } from './ext';\nfunction run() { renamedFn(); }\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "renamedFn", 2);
    assert_eq!(path, vec!["./ext", "someExtFn"]);
}

#[test]
fn import_default_binds_local_name_with_module_path() {
    let src = "import defaultExport from './ext';\nfunction run() { defaultExport(); }\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "defaultExport", 2);
    assert_eq!(path, vec!["./ext"]);
}

#[test]
fn import_namespace_alias_binds() {
    let src = "import * as nsModule from './ext';\nfunction run() { nsModule.foo(); }\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "nsModule", 2);
    assert_eq!(path, vec!["./ext"]);
}

#[test]
fn import_type_only_behaves_like_import() {
    let src = "import type { TypeOne } from './ext';\nfunction run(p: TypeOne) {}\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "TypeOne", 2);
    assert_eq!(path, vec!["./ext", "TypeOne"]);
}

#[test]
fn sibling_fns_do_not_share_locals() {
    let src = "function firstFn() { const onlyInFirst = 1; const _x = onlyInFirst; }\nfunction secondFn() { onlyInFirst; }\n";
    let (_syms, refs) = bind(src);
    let inside_first = find_ref(&refs, "onlyInFirst", 1);
    assert!(matches!(inside_first.target, BindTarget::Local(_)));
    let across = find_ref(&refs, "onlyInFirst", 2);
    assert!(
        matches!(across.target, BindTarget::Unresolved),
        "secondFn must not see firstFn's local; got {:?}",
        across.target
    );
}
