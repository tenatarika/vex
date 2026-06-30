//! Integration tests for the Kotlin scope binder.
//!
//! Probes use Capitalized / camelCase names on purpose:
//! `is_meaningful_identifier` drops pure-lowercase-without-underscore
//! tokens before they reach the ref table, so an all-lowercase Kotlin ident
//! (`run`, `parse`) would make a test pass for the wrong reason.

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef};

fn bind(src: &str) -> (Vec<ParsedSymbol>, Vec<BoundRef>) {
    let (symbols, _) =
        extract_symbols_and_imports(src, Language::Kotlin).expect("kotlin grammar must load");
    let refs = bind_refs(src, Language::Kotlin, &symbols).expect("binder must not fail");
    (symbols, refs)
}

fn find_ref<'a>(refs: &'a [BoundRef], name: &str, line: usize) -> &'a BoundRef {
    refs.iter()
        .find(|r| r.name == name && r.line == line)
        .unwrap_or_else(|| panic!("no ref `{name}` at line {line} in {refs:?}"))
}

#[test]
fn parameter_binds_to_local() {
    let src = "class C {\n  fun run(Seed: Int): Int {\n    return Seed\n  }\n}\n";
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
    let src =
        "class C {\n  fun run(Seed: Int): Int {\n    val LocalVal = Seed\n    return LocalVal\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "LocalVal", 4);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "got {:?}",
        r.target
    );
}

#[test]
fn vararg_param_binds_to_local() {
    // `vararg Elems: Int` carries a sibling `parameter_modifiers` node; the
    // param name must still bind to Local, not leak as a phantom ref.
    let src = "class C {\n  fun sum(vararg Elems: Int): Int {\n    return Elems.size\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "Elems", 3);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "vararg param must bind to Local, got {:?}",
        r.target
    );
}

#[test]
fn same_class_method_call_resolves_to_local() {
    // Methods are bound in the enclosing `Class` scope, so a same-class call
    // resolves to `Local` — never a phantom cross-file edge.
    let src =
        "class C {\n  fun helperFn(X: Int): Int {\n    return X\n  }\n  fun run(): Int {\n    return helperFn(1)\n  }\n}\n";
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
    // Class names ARE bound at the file root, so a same-file constructor
    // call (`Helper()`) promotes to `ModuleSymbol`.
    let src = "class Helper {}\nclass Main {\n  fun run(): Any {\n    return Helper()\n  }\n}\n";
    let (syms, refs) = bind(src);
    let r = find_ref(&refs, "Helper", 4);
    let idx = match &r.target {
        BindTarget::ModuleSymbol(i) => *i,
        other => panic!("expected ModuleSymbol, got {other:?}"),
    };
    assert_eq!(syms[idx as usize].name, "Helper");
}

#[test]
fn cross_file_bare_call_is_unresolved() {
    // `siblingFn` is defined in another file (not in this file's symbols) →
    // Unresolved here; Pass-2 links it cross-file.
    let src = "class C {\n  fun run(): Int {\n    return siblingFn(2)\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "siblingFn", 3);
    assert!(
        matches!(r.target, BindTarget::Unresolved),
        "expected Unresolved, got {:?}",
        r.target
    );
}

#[test]
fn single_import_binds_tail_name() {
    // `import com.example.ArrayThing` binds `ArrayThing`; the operand of
    // `ArrayThing()` then resolves to the import. Lowercase `com`/`example`
    // segments are filtered, so they never leak as refs.
    let src =
        "import com.example.ArrayThing\nclass C {\n  fun run(): Any {\n    return ArrayThing()\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "ArrayThing", 4);
    assert!(
        matches!(r.target, BindTarget::Imported(_)),
        "expected Imported, got {:?}",
        r.target
    );
    assert!(
        !refs.iter().any(|r| r.name == "com" || r.name == "example"),
        "lowercase package segments must not leak: {refs:?}"
    );
}

#[test]
fn wildcard_import_binds_nothing() {
    // `import com.example.*` must not create a binding, and the path must
    // not leak any ref at the import line.
    let src = "import com.example.*\nclass C {\n  fun run() {}\n}\n";
    let (_s, refs) = bind(src);
    assert!(
        !refs
            .iter()
            .any(|r| r.name == "com" || r.name == "example" || r.line == 1),
        "wildcard import must bind/emit nothing: {refs:?}"
    );
}

#[test]
fn import_alias_binds_alias_name() {
    // `import a.b.Widget as Wgt` binds the alias `Wgt` (not the tail
    // `Widget`); a later `Wgt()` resolves to the import. tree-sitter-kotlin
    // has no `import_alias` node — the alias is a bare identifier sibling.
    let src = "import a.b.Widget as Wgt\nclass C {\n  fun run(): Any {\n    return Wgt()\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "Wgt", 4);
    assert!(
        matches!(r.target, BindTarget::Imported(_)),
        "alias `Wgt` must resolve to Imported, got {:?}",
        r.target
    );
    assert!(
        !refs.iter().any(|r| r.name == "Widget"),
        "the un-aliased tail `Widget` must not be bound/emitted: {refs:?}"
    );
}

#[test]
fn string_interpolation_ref_resolves() {
    // A `${doWork()}` template interpolation carries a real ref; the binder
    // descends into `interpolation` children (the surrounding text is
    // dropped). `doWork` is a same-class method → Local.
    let src =
        "class C {\n  fun doWork(): Int {\n    return 1\n  }\n  fun run(): String {\n    return \"x ${doWork()} y\"\n  }\n}\n";
    let (_s, refs) = bind(src);
    let r = find_ref(&refs, "doWork", 6);
    assert!(
        matches!(r.target, BindTarget::Local(_)),
        "interpolation ref must resolve (Local), got {:?}",
        r.target
    );
}

#[test]
fn enum_entry_is_not_emitted_as_ref() {
    // `Red` / `Green` are enum constant *definitions*, not refs — binding
    // them keeps them out of the ref table (they're Capitalized, so they'd
    // leak as phantom Unresolved refs without the `enum_entry` handling).
    let src = "enum class Color {\n  Red,\n  Green\n}\n";
    let (_s, refs) = bind(src);
    assert!(
        !refs.iter().any(|r| r.name == "Red" || r.name == "Green"),
        "enum constants must not be emitted as refs: {refs:?}"
    );
}

#[test]
fn annotation_label_is_not_emitted_as_ref() {
    // `@Marker` is an annotation label (inside `modifiers`), not a ref — it
    // must not bloat the ref table.
    let src = "annotation class Marker\n@Marker\nclass Widget {\n  fun run() {}\n}\n";
    let (_s, refs) = bind(src);
    assert!(
        !refs.iter().any(|r| r.name == "Marker"),
        "annotation label must not be emitted as a ref: {refs:?}"
    );
}
