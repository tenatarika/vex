//! Integration tests for the C++ scope binder (11.1.5).

use vex::index::symbols::ParsedSymbol;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::{bind_refs, BindTarget, BoundRef, UsePath};

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
fn using_declaration_binds_tail_as_imported() {
    let src = "using app::Gateway;\nvoid Run() { Gateway::Charge(); }\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Gateway", 2);
    let path = match &r.target {
        BindTarget::Imported(p) => p.clone(),
        other => panic!("expected Imported, got {other:?}"),
    };
    assert_eq!(
        path,
        UsePath {
            segments: vec!["app".into(), "Gateway".into()],
        }
    );
}

#[test]
fn alias_declaration_binds_alias_to_target_path() {
    let src = "using Vec = std::vector<int>;\nvoid Run() { Vec v; }\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Vec", 2);
    let path = match &r.target {
        BindTarget::Imported(p) => p.clone(),
        other => panic!("expected Imported alias, got {other:?}"),
    };
    // template parameters stripped — only the type's qualified path survives.
    assert_eq!(path.segments, vec!["std".to_string(), "vector".to_string()]);
}

#[test]
fn alias_declaration_to_primitive_resolves_to_local_alias() {
    // No qualified path on RHS — bind as a local Type. The extractor
    // *also* records `Byte` as a top-level type alias symbol, so the
    // file-root binding gets promoted to `ModuleSymbol`; either is
    // fine as long as we don't emit a phantom `Imported(["unsigned
    // char"])` that Pass-2 would try (and fail) to resolve.
    let src = "using Byte = unsigned char;\nvoid Run() { Byte b = 0; }\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "Byte", 2);
    assert!(
        matches!(r.target, BindTarget::Local(_) | BindTarget::ModuleSymbol(_)),
        "expected Local or ModuleSymbol for primitive alias, got {:?}",
        r.target
    );
}

#[test]
fn namespace_alias_binds_alias_to_target_namespace() {
    // Alias name must carry case-boundary or underscore to survive
    // the binder's `is_meaningful_identifier` noise filter; lower-
    // only aliases like `fs` are dropped at ref emission. Use a
    // PascalCase alias so the test pins the import binding itself.
    let src = "namespace MyFs = std::filesystem;\nvoid Run() { MyFs::SomePath p; }\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "MyFs", 2);
    let path = match &r.target {
        BindTarget::Imported(p) => p.clone(),
        other => panic!("expected Imported namespace alias, got {other:?}"),
    };
    assert_eq!(
        path.segments,
        vec!["std".to_string(), "filesystem".to_string()]
    );
}

#[test]
fn using_root_namespace_path_is_single_segment() {
    // Reviewer H1: `using ::GlobalWidget;` parses as a
    // `qualified_identifier` with NO `scope:` field and `name:
    // identifier "GlobalWidget"`. The original positional-children
    // fallback visited `name:` once via the loop and once via the
    // `name:` field arm, producing `["GlobalWidget", "GlobalWidget"]`.
    let src = "using ::GlobalWidget;\nvoid Run() { GlobalWidget gw; }\n";
    let (_syms, refs) = bind(src);
    let r = find_ref(&refs, "GlobalWidget", 2);
    let path = match &r.target {
        BindTarget::Imported(p) => p.clone(),
        other => panic!("expected Imported, got {other:?}"),
    };
    assert_eq!(
        path,
        UsePath {
            segments: vec!["GlobalWidget".into()],
        },
        "root-namespace `::X` must collapse to a single-segment path",
    );
}

#[test]
fn using_path_segments_do_not_become_refs() {
    let src = "using app::Gateway;\nvoid Run() {}\n";
    let (_syms, refs) = bind(src);
    assert!(
        !refs.iter().any(|r| r.name == "app" && r.line == 1),
        "namespace segment `app` leaked into refs at line 1: {refs:?}",
    );
}

#[test]
fn preproc_include_emits_no_bindings_or_phantom_refs() {
    // `#include` stays a wildcard / deferred — verify neither the
    // header path nor `include` itself becomes a ref or binding.
    let src = "#include \"app/Gateway.h\"\nvoid Run() {}\n";
    let (_syms, refs) = bind(src);
    assert!(
        !refs.iter().any(|r| r.line == 1),
        "no refs should come out of the include line, got: {refs:?}",
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
