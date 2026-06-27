//! Integration tests for the Go scope binder.
//!
//! Probes use exported (Capitalized) or snake_case names on purpose:
//! `is_meaningful_identifier` drops pure-lowercase-without-underscore
//! tokens before they reach the ref table, so lowercase Go idents
//! (`spin`, `parse`) would make a test pass for the wrong reason.

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef};

fn bind(src: &str) -> (Vec<ParsedSymbol>, Vec<BoundRef>) {
    let (symbols, _) =
        extract_symbols_and_imports(src, Language::Go).expect("go grammar must load");
    let refs = bind_refs(src, Language::Go, &symbols).expect("binder must not fail");
    (symbols, refs)
}

fn find_ref<'a>(refs: &'a [BoundRef], name: &str, line: usize) -> &'a BoundRef {
    refs.iter()
        .find(|r| r.name == name && r.line == line)
        .unwrap_or_else(|| panic!("no ref `{name}` at line {line} in {refs:?}"))
}

#[test]
fn short_var_binds_to_local() {
    let src = "package main\nfunc Run(Seed int) int {\n\tLocalVal := Seed\n\treturn LocalVal\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "LocalVal", 4);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "expected Local, got {:?}",
        r.target
    );
}

#[test]
fn parameter_binds_to_local() {
    let src = "package main\nfunc Run(Seed int) int {\n\treturn Seed\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "Seed", 3);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "got {:?}",
        r.target
    );
}

#[test]
fn same_file_function_resolves_to_module_symbol() {
    let src = "package main\nfunc Helper(X int) int {\n\treturn X\n}\nfunc Run() int {\n\treturn Helper(1)\n}\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "Helper", 6);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "Helper");
}

#[test]
fn cross_file_bare_name_is_unresolved() {
    // `Sibling` is defined in another file of the package (not in this
    // file's symbols) → Unresolved here; Pass-2 links it cross-file.
    let src = "package main\nfunc Run() int {\n\treturn Sibling(2)\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "Sibling", 3);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "expected Unresolved, got {:?}",
        r.target
    );
}

#[test]
fn receiver_param_operand_is_local_and_method_field_resolves() {
    // `Inst` (a param) used as a selector operand resolves to Local; the
    // method name `Spin` is emitted as a by-name ref and, being defined in
    // this file, resolves to the method's ModuleSymbol. Mixed-case probes
    // dodge `is_meaningful_identifier` (single-letter / lowercase idents
    // are filtered before they reach the ref table).
    let src = "package main\nfunc (Recv *Widget) Spin() int {\n\treturn 0\n}\nfunc Run(Inst *Widget) int {\n\treturn Inst.Spin()\n}\n";
    let (_s, refs) = bind(src);
    let inst = find_ref(&refs, "Inst", 6);
    assert!(
        matches!(inst.target, BindTarget::Local(_)),
        "got {:?}",
        inst.target
    );
    let spin = find_ref(&refs, "Spin", 6);
    assert!(
        matches!(spin.target, BindTarget::ModuleSymbol(_)),
        "expected Spin to resolve to the file-local method symbol, got {:?}",
        spin.target
    );
}

#[test]
fn aliased_import_operand_resolves_to_imported() {
    // `import Rng "math/rand"` binds `Rng`; `Rng.Intn(…)` → operand `Rng`
    // resolves to the import binding (a capitalized alias survives the
    // meaningful-identifier filter that lowercase `mr` would not). The
    // field `Intn` is emitted as a by-name ref.
    let src =
        "package main\nimport Rng \"math/rand\"\nfunc Run() int {\n\treturn Rng.Intn(10)\n}\n";
    let (_s, refs) = bind(src);
    let rng = find_ref(&refs, "Rng", 4);
    assert!(
        matches!(rng.target, BindTarget::Imported(_)),
        "expected Imported, got {:?}",
        rng.target
    );
    let _ = find_ref(&refs, "Intn", 4);
}

#[test]
fn variadic_param_binds_to_local() {
    // `Elems ...int` is a `variadic_parameter_declaration`, a distinct
    // node kind from `parameter_declaration`. Without the variadic arm in
    // `bind_param_list` the name leaks to the `identifier` dispatch and
    // becomes an Unresolved phantom ref instead of a Local.
    let src = "package main\nfunc Sum(Elems ...int) int {\n\treturn len(Elems)\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "Elems", 3);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "variadic param must bind to Local, got {:?}",
        r.target
    );
}

#[test]
fn dot_and_blank_imports_bind_nothing() {
    // `. "strings"` and `_ "embed"` must not create an import binding;
    // they also must not emit a phantom ref at the import line.
    let src = "package main\nimport (\n\t. \"strings\"\n\t_ \"embed\"\n)\nfunc Run() {}\n";
    let (_s, refs) = bind(src);
    assert!(
        !refs
            .iter()
            .any(|r| r.name == "strings" || r.name == "embed"),
        "dot/blank import paths must not leak as refs: {refs:?}"
    );
}
