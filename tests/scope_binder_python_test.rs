//! Integration tests for the Python scope binder (11.1.5).
//!
//! Same shape as the other per-language binder tests. Import
//! resolution (`from x import y`) is a follow-up; names that need
//! imports stay `Unresolved` here. Comprehension and class-scope
//! LEGB quirks are documented as deferred.

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef};

fn bind(src: &str) -> (Vec<ParsedSymbol>, Vec<BoundRef>) {
    let (symbols, _) =
        extract_symbols_and_imports(src, Language::Python).expect("python grammar must load");
    let refs = bind_refs(src, Language::Python, &symbols).expect("binder must not fail");
    (symbols, refs)
}

fn find_ref<'a>(refs: &'a [BoundRef], name: &str, line: usize) -> &'a BoundRef {
    refs.iter()
        .find(|r| r.name == name && r.line == line)
        .unwrap_or_else(|| panic!("no ref `{name}` at line {line} in {refs:?}"))
}

#[test]
fn fn_local_assignment_binds_to_local() {
    let src = "def run():\n    value_one = 1\n    other_var = value_one\n";
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
    let src = "def add(left_op, right_op):\n    return left_op + right_op\n";
    let (_syms, refs) = bind(src);
    let l = find_ref(&refs, "left_op", 2);
    assert!(matches!(l.target, BindTarget::Local(_)));
    let r = find_ref(&refs, "right_op", 2);
    assert!(matches!(r.target, BindTarget::Local(_)));
}

#[test]
fn top_level_class_resolves_to_module_symbol() {
    let src = "class PaymentGateway:\n    pass\n\ndef run(p):\n    return PaymentGateway()\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "PaymentGateway", 5);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "PaymentGateway");
}

#[test]
fn top_level_function_resolves_to_module_symbol() {
    let src = "def helper_fn():\n    return 1\n\ndef caller_fn():\n    return helper_fn()\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "helper_fn", 5);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "helper_fn");
}

#[test]
fn unknown_name_is_unresolved() {
    let src = "def run():\n    ghost_name()\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "ghost_name", 2);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "expected Unresolved, got {:?}",
        r.target
    );
}

#[test]
fn sibling_fns_do_not_share_locals() {
    let src = "def first_fn():\n    only_in_first = 1\n    return only_in_first\n\ndef second_fn():\n    return only_in_first\n";
    let (_syms, refs) = bind(src);
    let inside_first = find_ref(&refs, "only_in_first", 3);
    assert!(matches!(inside_first.target, BindTarget::Local(_)));
    let across = find_ref(&refs, "only_in_first", 6);
    assert!(
        matches!(across.target, BindTarget::Unresolved),
        "second_fn must not see first_fn's local; got {:?}",
        across.target
    );
}

// --- Python imports ---

fn imported_path(refs: &[BoundRef], name: &str, line: usize) -> Vec<String> {
    let r = find_ref(refs, name, line);
    match &r.target {
        BindTarget::Imported(p) => p.segments.clone(),
        other => panic!("expected Imported for `{name}` line {line}, got {other:?}"),
    }
}

#[test]
fn from_import_binds_imported_name() {
    let src = "from external_pkg import Some_Type\nclass Holder:\n    def use(self):\n        return Some_Type()\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "Some_Type", 4);
    assert_eq!(path, vec!["external_pkg", "Some_Type"]);
}

#[test]
fn from_import_alias_uses_alias_name() {
    let src = "from external_pkg import Some_Type as Aliased_Name\nclass Holder:\n    def use(self):\n        return Aliased_Name()\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "Aliased_Name", 4);
    assert_eq!(path, vec!["external_pkg", "Some_Type"]);
}

#[test]
fn plain_import_binds_top_level_name() {
    let src = "import external_pkg\nclass Holder:\n    def use(self):\n        return external_pkg.thing()\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "external_pkg", 4);
    // `import x` binds only `x`; the dotted path here is just ["x"].
    assert_eq!(path, vec!["external_pkg"]);
}

#[test]
fn import_alias_uses_alias_name() {
    let src = "import external_pkg as Pkg_Alias\nclass Holder:\n    def use(self):\n        return Pkg_Alias.thing()\n";
    let (_syms, refs) = bind(src);
    let path = imported_path(&refs, "Pkg_Alias", 4);
    assert_eq!(path, vec!["external_pkg"]);
}

#[test]
fn from_import_star_leaves_names_unresolved() {
    let src = "from external_pkg import *\nclass Holder:\n    def use(self):\n        return Mystery_Name()\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Mystery_Name", 4);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "wildcard import must not bind unseen names; got {:?}",
        r.target
    );
}

#[test]
fn refs_skip_comment_and_docstring_noise() {
    // 11.1.1 already filters # comments and string literals (docstrings
    // are bare strings inside the body). The binder must keep that
    // filter active so prose mentions of `Fake_Symbol` don't pollute
    // bound refs either.
    let src = "def run():\n    # Fake_Symbol mentioned\n    \"\"\"Doc_Symbol mentioned\"\"\"\n    real_function()\n";
    let (_syms, refs) = bind(src);
    assert!(!refs.iter().any(|r| r.name == "Fake_Symbol"));
    assert!(!refs.iter().any(|r| r.name == "Doc_Symbol"));
    assert!(refs.iter().any(|r| r.name == "real_function"));
}
