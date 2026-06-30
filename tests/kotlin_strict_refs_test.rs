//! End-to-end coverage for the Kotlin scope binder through the real CLI:
//! `vex index` exercises the binder + Pass-2 resolution, then
//! `vex usages --strict` probes the `reference_edges` section. Before the
//! Kotlin binder, `bind_refs` returned empty for Kotlin so `--strict` was
//! always unavailable for Kotlin repos. Mirrors `java_strict_refs_test.rs`
//! and `go_strict_refs_test.rs`.
//!
//! Probes use camelCase names: Kotlin's cross-file symbols are public and
//! conventionally camelCase, and lowercase-no-underscore idents are dropped
//! by `is_meaningful_identifier` before resolution.

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
fn same_dir_cross_file_call_resolves() {
    // `doWork` defined in Helper.kt, called bare from Main.kt in the same
    // directory. The binder emits Unresolved; Pass-2's single-candidate
    // fallback links the cross-file edge.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("app")).unwrap();
    std::fs::write(
        tmp.path().join("app").join("Helper.kt"),
        "package app\nfun doWork(): Int = 1\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("app").join("Main.kt"),
        "package app\nclass Main {\n  fun run(): Int = doWork()\n}\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "doWork", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "app/Main.kt", 3);
}

#[test]
fn cross_package_imported_call_resolves() {
    // `Thing.doThing()` with an explicit `import x.util.Thing`. The trailing
    // `doThing` is a by-name ref that resolves cross-package to the unique
    // symbol in util/Thing.kt.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("util")).unwrap();
    std::fs::write(
        tmp.path().join("util").join("Thing.kt"),
        "package x.util\nobject Thing {\n  fun doThing(): Int = 1\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("Main.kt"),
        "package app\nimport x.util.Thing\nclass Main {\n  fun run(): Int = Thing.doThing()\n}\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "doThing", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "Main.kt", 4);
}

#[test]
fn ambiguous_cross_package_name_produces_no_edge() {
    // Two objects each define `doProcess`. The qualified `A.doProcess()` ref
    // is Unresolved with two candidates → the single-candidate fallback
    // declines (no edge). A uniquely-named `doBeacon` keeps the section
    // non-empty so `--strict` doesn't bail.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    for pkg in ["a", "b"] {
        std::fs::create_dir_all(tmp.path().join(pkg)).unwrap();
        std::fs::write(
            tmp.path().join(pkg).join("Proc.kt"),
            format!("package {pkg}\nobject Proc {{\n  fun doProcess(): Int = 1\n}}\n"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(tmp.path().join("beac")).unwrap();
    std::fs::write(
        tmp.path().join("beac").join("Beacon.kt"),
        "package beac\nobject Beacon {\n  fun doBeacon(): Int = 0\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("Main.kt"),
        "package app\nclass Main {\n  fun run(): Int = a.Proc.doProcess() + beac.Beacon.doBeacon()\n}\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    // Sanity: the unique `doBeacon` resolved → section exists.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "doBeacon", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "Main.kt", 3);

    // Ambiguous `doProcess` (2 candidates) must NOT resolve to Main.kt.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "doProcess", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        !stdout.contains("Main.kt"),
        "ambiguous `doProcess` (2 candidates) must not resolve via the \
         single-candidate fallback; got:\n{stdout}"
    );
}
