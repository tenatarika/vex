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
