//! Integration tests for the Java scope binder.
//!
//! Probes use Capitalized or camelCase names on purpose:
//! `is_meaningful_identifier` drops pure-lowercase-without-underscore
//! tokens before they reach the ref table, so an all-lowercase Java ident
//! (`run`, `parse`) would make a test pass for the wrong reason.

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef};

fn bind(src: &str) -> (Vec<ParsedSymbol>, Vec<BoundRef>) {
    let (symbols, _) =
        extract_symbols_and_imports(src, Language::Java).expect("java grammar must load");
    let refs = bind_refs(src, Language::Java, &symbols).expect("binder must not fail");
    (symbols, refs)
}

fn find_ref<'a>(refs: &'a [BoundRef], name: &str, line: usize) -> &'a BoundRef {
    refs.iter()
        .find(|r| r.name == name && r.line == line)
        .unwrap_or_else(|| panic!("no ref `{name}` at line {line} in {refs:?}"))
}

#[test]
fn parameter_binds_to_local() {
    let src = "class C {\n  int run(int Seed) {\n    return Seed;\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "Seed", 3);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "expected Local, got {:?}",
        r.target
    );
}

#[test]
fn local_var_binds_to_local() {
    let src = "class C {\n  int run(int Seed) {\n    int LocalVal = Seed;\n    return LocalVal;\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "LocalVal", 4);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "got {:?}",
        r.target
    );
}

#[test]
fn varargs_param_binds_to_local() {
    // `int... Elems` is a `spread_parameter` whose name lives in a nested
    // `variable_declarator`; without that handling the name leaks as a
    // phantom Unresolved ref instead of binding to Local.
    let src = "class C {\n  int sum(int... Elems) {\n    return Elems.length;\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "Elems", 3);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "varargs param must bind to Local, got {:?}",
        r.target
    );
}

#[test]
fn same_class_method_call_resolves_to_local() {
    // Methods are bound in the enclosing `Class` scope, not the file root,
    // so a same-class call resolves to `Local` (not `ModuleSymbol`, which
    // is gated on root-scope bindings — see walker::resolve). What matters
    // is that it is NOT `Unresolved`: a same-file call must never become a
    // phantom cross-file edge.
    let src = "class C {\n  int helperFn(int X) {\n    return X;\n  }\n  int run() {\n    return helperFn(1);\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "helperFn", 6);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "same-class call must resolve to Local, got {:?}",
        r.target
    );
}

#[test]
fn same_file_class_ref_resolves_to_module_symbol() {
    // Class names ARE bound at the file root, so a reference to a
    // same-file class (`new Helper()`) promotes to `ModuleSymbol`.
    let src = "class Helper {}\nclass Main {\n  Object run() {\n    return new Helper();\n  }\n}\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "Helper", 4);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "Helper");
}

#[test]
fn anonymous_class_member_does_not_leak_to_outer_scope() {
    // A capitalized method defined inside an anonymous class body must be
    // contained in that body's fresh Class scope — a sibling call after
    // the `new …(){}` must NOT resolve to it (it would be a phantom Local
    // without the dedicated scope). It stays Unresolved (no such method on
    // the enclosing class).
    let src = "class C {\n  void run() {\n    Runnable R = new Runnable() {\n      public void DoTask() {}\n    };\n    DoTask();\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "DoTask", 6);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "anon-class method must not leak to the outer scope, got {:?}",
        r.target
    );
}

#[test]
fn cross_file_bare_call_is_unresolved() {
    // `siblingFn` is defined in another file of the package (not in this
    // file's symbols) → Unresolved here; Pass-2 links it cross-file.
    let src = "class C {\n  int run() {\n    return siblingFn(2);\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "siblingFn", 3);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "expected Unresolved, got {:?}",
        r.target
    );
}

#[test]
fn single_type_import_binds_tail_name() {
    // `import java.util.ArrayList;` binds `ArrayList`; the operand of
    // `new ArrayList<>()` then resolves to the import. The lowercase
    // `java`/`util` segments are filtered, so they never leak as refs.
    let src = "import java.util.ArrayList;\nclass C {\n  Object run() {\n    return new ArrayList();\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "ArrayList", 4);
    assert!(
        matches!(r.target, BindTarget::Imported(_)),
        "expected Imported, got {:?}",
        r.target
    );
    // Package segments must not leak as refs.
    assert!(
        !refs.iter().any(|r| r.name == "java" || r.name == "util"),
        "lowercase package segments must not leak: {refs:?}"
    );
}

#[test]
fn wildcard_import_binds_nothing() {
    // `import java.util.*;` must not create a binding, and the path must
    // not leak any ref at the import line.
    let src = "import java.util.*;\nclass C {\n  void run() {}\n}\n";
    let (_s, refs) = bind(src);
    assert!(
        !refs
            .iter()
            .any(|r| r.name == "java" || r.name == "util" || r.line == 1),
        "wildcard import must bind/emit nothing: {refs:?}"
    );
}

#[test]
fn annotation_label_is_not_emitted_as_ref() {
    // `@Override` is an annotation label, not a ref — it must not bloat
    // the ref table.
    let src = "class C {\n  @Override\n  public String toString() {\n    return \"x\";\n  }\n}\n";
    let (_s, refs) = bind(src);
    assert!(
        !refs.iter().any(|r| r.name == "Override"),
        "annotation label must not be emitted as a ref: {refs:?}"
    );
}
