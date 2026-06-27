//! End-to-end coverage for C# cross-file reference resolution through the
//! real CLI surface — the C# counterpart to `cpp_strict_refs_test.rs`,
//! which previously had no equivalent (audit 2026-06-27, gap H5). Each
//! test builds a tiny project, runs `vex index` to exercise the full
//! Pass-2 resolution in `store::writer`, then probes `reference_edges`
//! via `vex usages --strict`.
//!
//! C# cross-file resolution differs from C++: there is no include graph.
//! A `using App.Lib;` namespace import creates NO name binding, so a bare
//! `Gateway` reference is `BindTarget::Unresolved` and resolves only via
//! the writer's single-candidate fallback (`writer.rs`, the
//! `name_to_global` unique-hit arm). That fallback is the load-bearing
//! path for real C# code — these tests pin it, including the deliberate
//! NON-resolution of ambiguous names (audit risk C3).

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

fn write_local_cache_config(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
}

fn assert_contains_call_site(stdout: &str, path: &str, line: usize) {
    let posix = format!("{path}:{line}");
    let windows = format!("{}:{line}", path.replace('/', "\\"));
    assert!(
        stdout.contains(&posix) || stdout.contains(&windows),
        "expected call site `{posix}` in strict usages, got:\n{stdout}"
    );
}

#[test]
fn using_namespace_import_resolves_unique_class_cross_file() {
    // `using App.Lib;` imports the namespace (no name binding); the bare
    // `Gateway` ref is Unresolved and resolves via the writer's
    // single-candidate fallback because exactly one `Gateway` exists in
    // the corpus. This is the dominant real-world C# cross-file path and
    // had zero E2E coverage before this test.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("gateway.cs"),
        "namespace App.Lib { public class Gateway { public int DoCharge() { return 1; } } }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cs"),
        "using App.Lib;\nnamespace App { class Holder { void Run() { var gw = new Gateway(); gw.DoCharge(); } } }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    // Class construction site resolves cross-file.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "Gateway", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cs", 2);

    // Method call site resolves cross-file too (unique method name).
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "DoCharge", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cs", 2);
}

#[test]
fn ambiguous_class_name_produces_no_cross_file_edge() {
    // Audit risk C3: two distinct classes share the name `Widget`. A bare
    // `new Widget()` ref is Unresolved; the single-candidate fallback sees
    // TWO `Widget` symbols and declines (no edge) rather than guess. We
    // keep a uniquely-named `Beacon` referenced in the same file so the
    // `reference_edges` section is non-empty (an all-unresolved project
    // makes `--strict` bail with a "needs reference_edges" error, which
    // would mask this assertion).
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("a.cs"),
        "namespace A { public class Widget { public int Spin() { return 1; } } }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("b.cs"),
        "namespace B { public class Widget { public int Spin() { return 2; } } }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("beacon.cs"),
        "namespace C { public class Beacon { public int Ping() { return 0; } } }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("main.cs"),
        "using A;\nusing C;\nnamespace App { class Use { void R() { var w = new Widget(); var b = new Beacon(); b.Ping(); } } }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    // Sanity: the unique `Beacon` DID resolve, proving the section exists
    // and the project isn't bailing for unrelated reasons.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "Beacon", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "src/main.cs", 3);

    // The ambiguous `Widget` must NOT produce a cross-file edge to main.cs.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "Widget", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        !stdout.contains("main.cs"),
        "ambiguous `Widget` (2 candidates) must not resolve to a call site via the \
         single-candidate fallback; got:\n{stdout}"
    );
}
