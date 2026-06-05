//! v1.14 — end-to-end coverage for C++ `#include`-driven cross-file ref
//! resolution. Each test builds a tiny project under a temp dir, runs
//! `vex index` to exercise the full Pass-2 BFS in `store::writer`, then
//! probes the resulting `reference_edges` section via
//! `vex usages --strict`. The unit tests in `store::include_resolver`
//! already pin the resolver/BFS logic in isolation; these tests prove the
//! pieces wire together correctly through the real CLI surface.
//!
//! User-facing bug this closes: before v1.14, `vex usages --strict` on a
//! C++ codebase returned **empty** for every cross-file symbol — the
//! binder's `Unresolved` target fell through to no ref edge. The
//! reporter (Windows, 50k-symbol `deep-source` repo) saw zero strict
//! usages for every symbol queried. Each test below would have failed
//! prior to Step 3 wiring.

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

/// Drop a `.vex.toml` that pins the cache inside the temp dir. Without
/// this, the cache dir falls back to `~/Library/Caches/vex/<hash>` —
/// cross-test pollution, slow first-run on CI.
fn write_local_cache_config(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
}

/// Assert that `stdout` mentions `path:line` for the strict-usages call
/// site. Tolerates Windows `\` separators so the same test passes on
/// `cargo test` runs in CI on all three OSes.
fn assert_contains_call_site(stdout: &str, path: &str, line: usize) {
    let posix = format!("{path}:{line}");
    let windows = format!("{}:{line}", path.replace('/', "\\"));
    assert!(
        stdout.contains(&posix) || stdout.contains(&windows),
        "expected call site `{posix}` in strict usages, got:\n{stdout}"
    );
}

#[test]
fn direct_include_resolves_cross_file_ref() {
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("util.h"),
        "#pragma once\nint do_thing();\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cpp"),
        "#include \"util.h\"\nint main() { return do_thing(); }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "do_thing", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cpp", 2);
}

#[test]
fn transitive_include_via_two_hops() {
    // A.cpp → B.h → C.h. The symbol lives in C.h; A.cpp only includes B.h.
    // Resolution depth-2 is the headline transitive case — same-dir-only
    // 80/20 (architect's recommendation) would NOT have caught this; the
    // user picked full BFS specifically to cover patterns like this.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("c.h"),
        "#pragma once\nint deep_fn();\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("b.h"),
        "#pragma once\n#include \"c.h\"\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("a.cpp"),
        "#include \"b.h\"\nint main() { return deep_fn(); }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "deep_fn", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/a.cpp", 2);
}

#[test]
fn cycle_in_includes_does_not_hang() {
    // A.h ⇄ B.h mutual include (real before `#pragma once` is processed;
    // also real with guard-macro patterns that the resolver doesn't
    // model). BFS uses a `HashSet<file_id>` visited set, so the index
    // build must terminate AND still resolve the cross-file ref.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("a.h"),
        "#include \"b.h\"\nint helper_a();\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("b.h"),
        "#include \"a.h\"\nint helper_b();\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cpp"),
        "#include \"a.h\"\nint main() { return helper_b(); }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "helper_b", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cpp", 2);
}

#[test]
fn system_include_is_ignored_and_does_not_break_resolution() {
    // `#include <vector>` is `system_lib_string`, dropped by the parser.
    // The index build must still succeed and the project-local
    // resolution must still work — a regression here would suggest the
    // parser filter or the writer pass-2 was choking on the system
    // header path.
    //
    // The probe symbol is `compute_value` (not `compute`) on purpose:
    // `is_meaningful_identifier` in `parse::extractor` filters out pure
    // lowercase tokens without an underscore (`compute`, `calc`,
    // `total`, …) to keep the refs FST from drowning in prose nouns.
    // That filter is unrelated to v1.14, but it would mask the test —
    // ref edges section would be empty and `--strict` would bail.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("util.h"),
        "#pragma once\nint compute_value();\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cpp"),
        "#include <vector>\n#include \"util.h\"\nint main() { return compute_value(); }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "compute_value", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cpp", 3);
}

#[test]
fn using_declaration_still_resolves_after_v1_14() {
    // Regression guard: `using app::Gateway;` was the ONE C++ pattern
    // that resolved cross-file before v1.14 (via `BindTarget::Imported`).
    // The Pass-2 BFS path is a fallback for `Unresolved`; it must not
    // accidentally regress the working `Imported` path.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("gateway.h"),
        "#pragma once\nnamespace app { class Gateway {}; }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cpp"),
        "#include \"gateway.h\"\nusing app::Gateway;\nint main() { Gateway gw; (void)gw; return 0; }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "Gateway", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cpp", 3);
}

#[test]
fn vex_status_surfaces_cpp_includes_processed_marker() {
    // The new `Manifest::cpp_includes_processed: Option<bool>` field is
    // the user-visible signal that an index was built with v1.14+
    // resolution. Pin both the text and JSON output paths — without an
    // integration test, a regression in `cmd_status.rs` could silently
    // ship a stale `C++ includes: no` line and there'd be no fast way
    // to tell a v1.14 index from a v1.13 one without re-reading the
    // manifest by hand.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    // Text format
    let assert = assert_ran(vex_in(tmp.path()).args(["status"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("C++ includes: yes"),
        "expected `C++ includes: yes` line in `vex status`, got:\n{stdout}"
    );

    // JSON format
    let assert = assert_ran(vex_in(tmp.path()).args(["--format", "json", "status"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("\"cpp_includes_processed\":true")
            || stdout.contains("\"cpp_includes_processed\": true"),
        "expected `cpp_includes_processed: true` JSON key, got:\n{stdout}"
    );
}

#[test]
fn unincluded_header_does_not_pollute_cross_file_refs() {
    // `lonely.h` defines `should_not_show` but no .cpp ever includes it.
    // BFS from `main.cpp` only reaches files via include edges → the
    // symbol IS in the index (parsed from lonely.h) but no cross-file
    // ref to it exists from main.cpp. `vex usages --strict` should
    // therefore only show the decl line in lonely.h itself, not a fake
    // call site in main.cpp (which never references it).
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("lonely.h"),
        "#pragma once\nint should_not_show();\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "should_not_show", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        !stdout.contains("main.cpp"),
        "main.cpp never references `should_not_show`; strict must not invent a ref. stdout:\n{stdout}"
    );
}

#[test]
fn class_member_method_resolves_cross_file_via_include() {
    // v1.14.1 follow-up to the v1.14 cross-file refs work.
    //
    // Before this fix, C++ class member methods were INVISIBLE to vex:
    // (a) the SCM query covered free `function_definition` and file-
    //     level `declaration` shapes, but methods declared inside a
    //     class body are `field_declaration` nodes — they never reached
    //     `records[]`, so `vex search do_charge` returned only the
    //     containing class.
    // (b) consequently `vex usages --strict` had nothing to resolve to;
    //     the v1.14 Pass-2 BFS would walk the include graph looking for
    //     `do_charge`'s defining file but never find a symbol with that
    //     name in `name_to_global`.
    //
    // The fix adds two SCM patterns in `queries/cpp.scm`: one for
    // method declarations (`field_declaration → function_declarator →
    // field_identifier`) and one for inline method definitions
    // (`function_definition → function_declarator → field_identifier`).
    // Both index as `SymbolKind::Method`. This test pins both: the
    // header-declared `do_charge` and the inline `inline_method` must
    // be findable AND their cross-file call sites in `main.cpp` must
    // resolve under `--strict`.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("gateway.h"),
        "#pragma once\n\
         namespace app {\n\
         \x20\x20\x20\x20class Gateway {\n\
         \x20\x20\x20\x20public:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20int do_charge();\n\
         \x20\x20\x20\x20\x20\x20\x20\x20int inline_method() { return 42; }\n\
         \x20\x20\x20\x20};\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cpp"),
        "#include \"gateway.h\"\n\
         int main() {\n\
         \x20\x20\x20\x20app::Gateway gw;\n\
         \x20\x20\x20\x20return gw.do_charge() + gw.inline_method();\n\
         }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    // Declared method: cross-file call site in main.cpp:4 must resolve.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "do_charge", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cpp", 4);

    // Inline method definition: same expectation. Catches a regression
    // where the SCM patch only covered field_declaration prototypes
    // and forgot the inline-definition shape.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "inline_method", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cpp", 4);
}

#[test]
fn qualified_static_method_call_resolves_cross_file() {
    // `app::Gateway::static_method()` is a `qualified_identifier` call
    // expression. The trailing `static_method` part must still match
    // a top-level symbol after the v1.14.1 SCM patch indexes class
    // member methods. Pre-fix: empty (method wasn't a symbol at all).
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("gateway.h"),
        "#pragma once\n\
         namespace app {\n\
         \x20\x20\x20\x20class Gateway {\n\
         \x20\x20\x20\x20public:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20static int static_method();\n\
         \x20\x20\x20\x20};\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cpp"),
        "#include \"gateway.h\"\n\
         int main() {\n\
         \x20\x20\x20\x20return app::Gateway::static_method();\n\
         }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "static_method", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cpp", 3);
}
