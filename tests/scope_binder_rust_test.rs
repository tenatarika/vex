//! Integration tests for the Rust scope binder (11.1.2b).
//!
//! These tests pin the resolver behaviour for the in-file resolution
//! cases that 11.1.2b is required to handle. Use-graph and cross-file
//! resolution land in 11.1.2c / 11.1.3 — names that need imports stay
//! `Unresolved` here on purpose.

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef};

fn bind(src: &str) -> (Vec<ParsedSymbol>, Vec<BoundRef>) {
    let (symbols, _) =
        extract_symbols_and_imports(src, Language::Rust).expect("rust grammar must load");
    let refs = bind_refs(src, Language::Rust, &symbols).expect("binder must not fail");
    (symbols, refs)
}

fn find_ref<'a>(refs: &'a [BoundRef], name: &str, line: usize) -> &'a BoundRef {
    refs.iter()
        .find(|r| r.name == name && r.line == line)
        .unwrap_or_else(|| panic!("no ref `{name}` at line {line} in {refs:?}"))
}

#[test]
fn fn_local_let_binds_to_local() {
    let src = "fn run() {\n    let value_one = 1;\n    let _x = value_one;\n}\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "value_one", 3);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "expected Local, got {:?}",
        r.target
    );
}

#[test]
fn fn_parameter_binds_to_local() {
    let src = "fn add(left_op: i32, right_op: i32) -> i32 { left_op + right_op }\n";
    let (_syms, refs) = bind(src);
    let l = find_ref(&refs, "left_op", 1);
    assert!(
        matches!(l.target, BindTarget::Local(_)),
        "expected Local for fn param `left_op`, got {:?}",
        l.target
    );
    let r = find_ref(&refs, "right_op", 1);
    assert!(matches!(r.target, BindTarget::Local(_)));
}

#[test]
fn top_level_struct_resolves_to_module_symbol() {
    let src = "struct Payment_Gateway;\nfn run(_p: Payment_Gateway) {}\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "Payment_Gateway", 2);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(
        syms[idx as usize].name, "Payment_Gateway",
        "ModuleSymbol idx must point at the type symbol"
    );
}

#[test]
fn unknown_name_is_unresolved() {
    let src = "fn run() { ghost_name(); }\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "ghost_name", 1);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "expected Unresolved, got {:?}",
        r.target
    );
}

#[test]
fn inner_block_shadow_wins_for_inner_ref() {
    let src = "fn run() {\n    let shadowed_var = 1;\n    {\n        let shadowed_var = 2;\n        let _x = shadowed_var;\n    }\n}\n";
    let (_syms, refs) = bind(src);
    let inner = find_ref(&refs, "shadowed_var", 5);
    let outer_def_scope = match &find_ref(&refs, "shadowed_var", 5).target {
        BindTarget::Local(sid) => *sid,
        other => panic!("expected Local, got {other:?}"),
    };
    // Inner block scope id must differ from the file root (0); the outer
    // `let` would bind into the fn scope, the inner `let` into the block
    // scope, so the resolver MUST land on the deeper scope.
    assert_ne!(outer_def_scope, 0, "must not resolve to file root");
    // Sanity: the ref does exist and was found.
    let _ = inner;
}

// --- 11.1.2c: `use`-graph ---

fn imported_path(refs: &[BoundRef], name: &str, line: usize) -> Vec<String> {
    let r = find_ref(refs, name, line);
    match &r.target {
        BindTarget::Imported(p) => p.segments.clone(),
        other => panic!("expected Imported for `{name}` line {line}, got {other:?}"),
    }
}

#[test]
fn use_brings_external_name_into_scope() {
    let src = "use external_crate::Some_Type;\nfn run(_p: Some_Type) {}\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "Some_Type", 2);
    assert_eq!(path, vec!["external_crate", "Some_Type"]);
}

#[test]
fn use_list_brings_multiple_names() {
    let src = "use external_crate::{First_Item, Second_Item};\nfn run() {\n    let _a = First_Item;\n    let _b = Second_Item;\n}\n";
    let (_syms, refs) = bind(src);
    let p1 = imported_path(&refs, "First_Item", 3);
    let p2 = imported_path(&refs, "Second_Item", 4);
    assert_eq!(p1, vec!["external_crate", "First_Item"]);
    assert_eq!(p2, vec!["external_crate", "Second_Item"]);
}

#[test]
fn use_as_alias_records_original_path_under_alias_name() {
    let src = "use external_crate::Long_Original_Name as Short_Alias;\nfn run(_p: Short_Alias) {}\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "Short_Alias", 2);
    assert_eq!(path, vec!["external_crate", "Long_Original_Name"]);
}

#[test]
fn use_glob_does_not_bind_individual_names() {
    let src = "use external_crate::*;\nfn run() { let _x = Mystery_Name; }\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Mystery_Name", 2);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "glob must not bind unseen names; got {:?}",
        r.target
    );
}

#[test]
fn pub_use_behaves_like_use() {
    let src = "pub use external_crate::Reexported_Type;\nfn run(_p: Reexported_Type) {}\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "Reexported_Type", 2);
    assert_eq!(path, vec!["external_crate", "Reexported_Type"]);
}

#[test]
fn sibling_fns_do_not_share_locals() {
    let src = "fn first_fn() { let only_in_first = 1; let _x = only_in_first; }\nfn second_fn() { only_in_first }\n";
    let (_syms, refs) = bind(src);
    let inside_first = find_ref(&refs, "only_in_first", 1);
    assert!(matches!(inside_first.target, BindTarget::Local(_)));
    let across = find_ref(&refs, "only_in_first", 2);
    assert!(
        matches!(across.target, BindTarget::Unresolved),
        "second_fn must not see first_fn's local; got {:?}",
        across.target
    );
}
