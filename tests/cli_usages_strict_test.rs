//! CLI tests for `vex usages --strict` (11.1.3d).
//!
//! After 11.1.3d the `--strict` flag reads from the persistent
//! `reference_edges` section (v5 index) instead of the legacy refs FST.
//! Only scope-binder-resolved references show up; identifier matches in
//! comments, strings, or unrelated scopes are filtered out at index
//! build time.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

mod common;
use common::assert_ran;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// A tiny project where the scope binder will produce exactly one
/// `ModuleSymbol`-targeted ref: the call site on the body of `caller`.
fn write_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn payment_processor() {}\n\nfn caller_fn() {\n    payment_processor();\n}\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn strict_returns_binder_resolved_call_site() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "payment_processor", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("src/lib.rs:4") || stdout.contains("src\\lib.rs:4"),
        "expected the line-4 call site under --strict, got: {stdout}"
    );
}

#[test]
fn strict_does_not_emit_deferral_warning_anymore() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "payment_processor", "--strict"]));
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        !stderr.contains("type-aware refs not yet built"),
        "deferral warning must be gone once 11.1.3d wires the section; stderr: {stderr}"
    );
}

#[test]
fn no_strict_does_not_print_warning() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "payment_processor"]));
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        !stderr.contains("type-aware refs not yet built"),
        "no warning expected without --strict, got: {stderr}"
    );
}

#[test]
fn strict_bails_when_index_has_no_ref_edges() {
    // A project with no Rust files (only a stray Go file) → binder
    // never runs → reference_edges section is empty → has_ref_edges()
    // is false → `--strict` must bail with the rebuild message.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("a.go"),
        "package main\nfunc payment_processor() {}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = vex_in(tmp.path())
        .args(["usages", "payment_processor", "--strict"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("--strict") && stderr.contains("Re-run `vex index`"),
        "expected rebuild bail message for index without ref_edges, got: {stderr}"
    );
}

#[test]
fn strict_prints_no_usages_when_symbol_has_zero_refs() {
    // Two top-level symbols; only one is referenced from a call site.
    // The unreferenced one should produce "No usages found" under
    // --strict instead of an empty result that looks like a crash.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn alpha_fn() {}\n\
         pub fn beta_fn() {}\n\
         \n\
         fn caller_fn() {\n\
         \x20\x20\x20\x20alpha_fn();\n\
         }\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    // alpha_fn has one ref (from caller_fn); beta_fn has zero.
    // v1.12.0 S8.2: `vex usages` exits 1 when no refs match — the test
    // is for the "no usages" message on stdout, not the exit code.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "beta_fn", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("No usages found"),
        "expected the 'No usages found' summary for an unreferenced symbol, got: {stdout}"
    );
}

#[test]
fn strict_resolves_csharp_using_directive_cross_file() {
    // Two-file C# project: lib defines PaymentGateway, caller pulls
    // it in via `using App.Lib.PaymentGateway;` and instantiates it.
    // Pass-2 must rewrite the Imported(["App","Lib","PaymentGateway"])
    // binding into a ref edge pointing at PaymentGateway in lib.cs.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("lib.cs"),
        "namespace App.Lib;\npublic class PaymentGateway {\n    public void Charge() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("caller.cs"),
        "using App.Lib.PaymentGateway;\nclass Caller {\n    void Run() {\n        var gw = new PaymentGateway();\n    }\n}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "PaymentGateway", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // line 4 of caller.cs: `        var gw = new PaymentGateway();`
    assert!(
        stdout.contains("src/caller.cs:4") || stdout.contains("src\\caller.cs:4"),
        "expected the line-4 PaymentGateway ref under --strict, got: {stdout}"
    );
    // The defining file (lib.cs) must NOT show up in `--strict`:
    // definitions are bindings, not refs. This pins the directional
    // invariant — ref edges point caller → definition only.
    assert!(
        !stdout.contains("lib.cs"),
        "definition file lib.cs leaked into usages output: {stdout}"
    );
}

#[test]
fn strict_resolves_cpp_using_declaration_cross_file() {
    // Two-file C++ project: gateway.cpp defines PaymentGateway,
    // caller.cpp brings it in via `using app::PaymentGateway;` and
    // references it. Pass-2 must rewrite the import binding into a
    // ref edge.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("gateway.cpp"),
        "namespace app {\nclass PaymentGateway {};\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("caller.cpp"),
        "using app::PaymentGateway;\nvoid Run() {\n    PaymentGateway gw;\n}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "PaymentGateway", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // line 3 of caller.cpp: `    PaymentGateway gw;`
    assert!(
        stdout.contains("src/caller.cpp:3") || stdout.contains("src\\caller.cpp:3"),
        "expected the line-3 PaymentGateway ref under --strict, got: {stdout}"
    );
    // Definition file (gateway.cpp) must NOT appear — pins the
    // caller-only direction of ref edges, same as the C# case.
    assert!(
        !stdout.contains("gateway.cpp"),
        "definition file gateway.cpp leaked into usages output: {stdout}"
    );
}

#[test]
fn strict_filters_out_string_literal_noise_that_legacy_fst_keeps() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    // Two occurrences: a real call (line 4) and a string-literal
    // mention (line 5). 11.1.1 already removes the string mention from
    // the legacy refs FST; this test pins the stricter binder behaviour
    // — only the real call survives under `--strict`.
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn payment_processor() {}\n\
         \n\
         fn caller_fn() {\n\
         \x20\x20\x20\x20payment_processor();\n\
         \x20\x20\x20\x20let _msg = \"payment_processor is unused here\";\n\
         }\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "payment_processor", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let line5 = stdout.lines().any(|l| l.contains(":5"));
    assert!(
        !line5,
        "string-literal mention on line 5 must not survive strict mode, got: {stdout}"
    );
}

#[test]
fn strict_resolves_cross_file_ref_past_module_row_in_target_file() {
    // Regression for the v1.14.1 name_to_global index-space fix.
    //
    // Phase 14.1 inserts a synthetic `<module:path>` SymbolKind::Module
    // row at file.symbols[0] whenever a file has a module-scope call
    // edge (sentinel `caller_fn_name == "" && caller_fn_line == 0`).
    // Pre-1.14.1, `name_to_global` was keyed by the post-Module-filter
    // enumeration index instead of the real SymbolRecord position —
    // so any cross-file `Imported` (or v1.14 `Unresolved` C++ BFS) ref
    // whose target lived AFTER a Module row in its defining file got
    // its `to_sym_idx` silently pointed at the Module row, not the
    // intended symbol. User-visible: `vex usages --strict <fn>` ran
    // empty for Python / Rust / TS files with any top-level expression.
    //
    // This test pins the post-fix behaviour. `payment_processor` is
    // defined in a.py *after* a module-scope `print(...)` call (which
    // triggers the sentinel + Module row). b.py imports and calls it.
    // Pre-fix: zero results (silent bug). Post-fix: line 4 of b.py.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("a.py"),
        "print(\"loaded\")\n\
         \n\
         def payment_processor():\n\
         \x20\x20\x20\x20return 1\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("b.py"),
        "from a import payment_processor\n\
         \n\
         def caller_fn():\n\
         \x20\x20\x20\x20payment_processor()\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "payment_processor", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // The call site in b.py is line 4 (`    payment_processor()`).
    assert!(
        stdout.contains("b.py:4") || stdout.contains("b.py:4\n"),
        "expected b.py:4 call site for cross-file `payment_processor` after module-row offset fix; \
         got:\n{stdout}"
    );
}

#[test]
fn strict_resolves_python_class_method_cross_file() {
    // v1.14.1 single-candidate fallback for `BindTarget::Unresolved`.
    //
    // Before the fallback, `gw.do_charge()` in `main.py` got binder
    // target `Unresolved` (Python's binder emits identifier refs but
    // can't disambiguate method calls without type inference). The
    // v1.14 C++ include-BFS path early-outs for non-C++ files, so
    // no ref edge was produced even though `do_charge` IS indexed
    // (Python's tree-sitter grammar uses `function_definition` for
    // both free fns and class methods — already in `records[]`).
    //
    // After the fallback: when `name_to_global` holds exactly one
    // entry for the name, Pass-2 resolves to it. Multi-candidate
    // names stay Unresolved (next test). Heuristic but predictable.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("gateway.py"),
        "class Gateway:\n\
         \x20\x20\x20\x20def do_charge(self):\n\
         \x20\x20\x20\x20\x20\x20\x20\x20return 100\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("main.py"),
        "from gateway import Gateway\n\
         def caller():\n\
         \x20\x20\x20\x20gw = Gateway()\n\
         \x20\x20\x20\x20return gw.do_charge()\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "do_charge", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("main.py:4") || stdout.contains("main.py:4\n"),
        "expected main.py:4 call site for `gw.do_charge()`, got:\n{stdout}"
    );
}

#[test]
fn strict_bails_on_multi_candidate_unresolved_to_avoid_false_positive() {
    // Pin the safety side of the single-candidate fallback: when
    // multiple definitions exist for the same name, Pass-2 must NOT
    // pick one — that would silently mis-attribute the ref. The
    // existing Imported arm's first-match-wins is fine for explicit
    // imports (the user named the path), but Unresolved is by
    // definition "binder couldn't figure it out", so picking a
    // random match is worse than empty.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    // Two unrelated `process_payment` definitions in different files.
    std::fs::write(
        tmp.path().join("billing.py"),
        "class Billing:\n\
         \x20\x20\x20\x20def process_payment(self):\n\
         \x20\x20\x20\x20\x20\x20\x20\x20return \"billing\"\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("refunds.py"),
        "class Refunds:\n\
         \x20\x20\x20\x20def process_payment(self):\n\
         \x20\x20\x20\x20\x20\x20\x20\x20return \"refund\"\n",
    )
    .unwrap();
    // Caller mixes unambiguous + ambiguous. `unique_helper` resolves
    // (single-candidate); `process_payment` must bail (multi-candidate).
    // Need the unambiguous one so the ref_edges section isn't empty —
    // an empty section causes `--strict` to bail with exit 2 before
    // we can inspect stdout for the ambiguity test.
    std::fs::write(
        tmp.path().join("util.py"),
        "def unique_helper():\n\
         \x20\x20\x20\x20return 1\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("main.py"),
        "from util import unique_helper\n\
         def caller(obj):\n\
         \x20\x20\x20\x20unique_helper()\n\
         \x20\x20\x20\x20return obj.process_payment()\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "process_payment", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // The ambiguous call site (`main.py:4`) MUST NOT appear in strict
    // refs. With multi-candidate names the single-candidate fallback
    // bails — no edge is emitted at all, and `--strict` correctly
    // reports "No usages found" (strict shows REFs, not definitions).
    assert!(
        !stdout.contains("main.py:4"),
        "ambiguous call must not resolve under single-candidate fallback, got:\n{stdout}"
    );

    // Positive guard for the OTHER side of the test: confirm
    // single-candidate resolution still works in this same build. If
    // a regression silently empties the ref_edges section entirely
    // (e.g. the name_to_global index-space bug returning), the
    // negative assertion above would pass vacuously. Probing
    // `unique_helper` — which has exactly one definition and one
    // unambiguous call from main.py:3 — proves Pass-2 produced ref
    // edges and reached the disambiguation gate before bailing.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "unique_helper", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("main.py:3") || stdout.contains("main.py:3\n"),
        "expected main.py:3 for `unique_helper()` to confirm Pass-2 emitted \
         ref edges in this build, got:\n{stdout}"
    );
}

#[test]
fn strict_resolves_csharp_class_method_cross_file_via_using_namespace() {
    // Mirrors the Python test for C#: `using App;` brings the namespace
    // into scope but our binder only binds the namespace name itself,
    // not its member types. So `new Gateway()` and `gw.DoCharge()` end
    // up `Unresolved`. The single-candidate fallback resolves them when
    // there's exactly one definition project-wide. Before the fix the
    // C# index had `has_ref_edges == false` (all refs were Unresolved,
    // none produced edges), so `vex usages --strict DoCharge` bailed
    // with "this index is v6 or has no resolved refs".
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("Gateway.cs"),
        "namespace App {\n\
         \x20\x20\x20\x20public class Gateway {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20public int DoCharge() { return 100; }\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("Main.cs"),
        "using App;\n\
         public class Main {\n\
         \x20\x20\x20\x20public int Run() {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20var gw = new Gateway();\n\
         \x20\x20\x20\x20\x20\x20\x20\x20return gw.DoCharge();\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "DoCharge", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("src/Main.cs:5") || stdout.contains("src\\Main.cs:5"),
        "expected Main.cs:5 call site for `gw.DoCharge()`, got:\n{stdout}"
    );
}

#[test]
fn strict_resolves_typescript_class_method_cross_file() {
    // v1.14.1 — TypeScript needed BOTH halves of the fix:
    //
    // (1) SCM gap in `queries/typescript.scm` — only free
    //     `function_declaration` was extracted; class methods
    //     (`method_definition`), interface signatures
    //     (`method_signature`), and abstract methods
    //     (`abstract_method_signature`) all weren't symbols.
    //
    // (2) Binder gap in `src/parse/scope/typescript.rs` — even after
    //     SCM indexed the methods, the TS binder only emitted refs for
    //     `identifier` / `type_identifier`. Member access
    //     (`gw.do_charge()` → `property_identifier` on the rhs) was
    //     silently dropped. Added a `member_expression` walker that
    //     emits the property as a Value ref so the single-candidate
    //     fallback in writer's Pass-2 can resolve it cross-file.
    //
    // This test exercises all three method shapes (regular, static,
    // interface signature) in one project so a regression in either
    // (1) or (2) shows up immediately.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("gateway.ts"),
        "export class Gateway {\n\
         \x20\x20\x20\x20do_charge(): number {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20return 100;\n\
         \x20\x20\x20\x20}\n\
         \x20\x20\x20\x20static static_method(): number {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20return 200;\n\
         \x20\x20\x20\x20}\n\
         }\n\
         export interface Processor {\n\
         \x20\x20\x20\x20process_item(x: number): string;\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.ts"),
        "import { Gateway, Processor } from './gateway';\n\
         function caller(p: Processor): number {\n\
         \x20\x20\x20\x20const gw = new Gateway();\n\
         \x20\x20\x20\x20const a = gw.do_charge();\n\
         \x20\x20\x20\x20const b = Gateway.static_method();\n\
         \x20\x20\x20\x20p.process_item(a + b);\n\
         \x20\x20\x20\x20return a + b;\n\
         }\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    // Regular instance method: call site at main.ts:4.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "do_charge", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("src/main.ts:4") || stdout.contains("src\\main.ts:4"),
        "expected main.ts:4 for `gw.do_charge()`, got:\n{stdout}"
    );

    // Static method via `Class.method()` — same `member_expression`
    // shape, ensures the walker doesn't accidentally skip when the
    // object side is a class identifier rather than an instance.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "static_method", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("src/main.ts:5") || stdout.contains("src\\main.ts:5"),
        "expected main.ts:5 for `Gateway.static_method()`, got:\n{stdout}"
    );

    // Interface signature called on a parameter — `process_item` is
    // declared as `method_signature` (new SCM pattern), so both the
    // indexing AND the call-site resolution need to succeed.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "process_item", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("src/main.ts:6") || stdout.contains("src\\main.ts:6"),
        "expected main.ts:6 for `p.process_item(...)`, got:\n{stdout}"
    );
}
