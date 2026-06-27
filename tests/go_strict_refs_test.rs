//! End-to-end coverage for the Go scope binder through the real CLI:
//! `vex index` exercises the binder + Pass-2 resolution, then
//! `vex usages --strict` probes the `reference_edges` section. Before the
//! Go binder, `bind_refs` returned empty for Go so `--strict` was always
//! unavailable for Go repos. Mirrors `csharp_strict_refs_test.rs`.
//!
//! Probes use exported (Capitalized) names: Go's cross-package symbols
//! are capitalized by definition, and lowercase-no-underscore idents are
//! dropped by `is_meaningful_identifier` before resolution.

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
fn within_package_cross_file_call_resolves() {
    // `Helper` defined in helper.go, called bare from main.go in the same
    // package. The binder emits Unresolved; Pass-2's single-candidate
    // fallback links the cross-file edge.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("app")).unwrap();
    std::fs::write(
        tmp.path().join("app").join("helper.go"),
        "package app\nfunc Helper() int { return 1 }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("app").join("main.go"),
        "package app\nfunc Run() int { return Helper() }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "Helper", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "app/main.go", 2);
}

#[test]
fn cross_package_selector_call_resolves() {
    // `util.DoThing()` — the package-qualified call. The operand `util`
    // is filtered (lowercase), but the trailing `DoThing` field is a
    // by-name ref that resolves cross-package to the unique symbol in
    // util/thing.go.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    std::fs::create_dir_all(tmp.path().join("util")).unwrap();
    std::fs::write(
        tmp.path().join("util").join("thing.go"),
        "package util\nfunc DoThing() int { return 1 }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("main.go"),
        "package main\nimport \"x/util\"\nfunc Run() int { return util.DoThing() }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "DoThing", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "main.go", 3);
}

#[test]
fn ambiguous_cross_package_name_produces_no_edge() {
    // Two packages each define `Process`. The qualified `a.Process()` ref
    // is Unresolved with two `Process` candidates → the single-candidate
    // fallback declines (no edge). A uniquely-named `Beacon` call keeps
    // the reference_edges section non-empty so `--strict` doesn't bail.
    let tmp = TempDir::new().unwrap();
    write_local_cache_config(tmp.path());
    for pkg in ["a", "b"] {
        std::fs::create_dir_all(tmp.path().join(pkg)).unwrap();
        std::fs::write(
            tmp.path().join(pkg).join("proc.go"),
            format!("package {pkg}\nfunc Process() int {{ return 1 }}\n"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(tmp.path().join("beac")).unwrap();
    std::fs::write(
        tmp.path().join("beac").join("beacon.go"),
        "package beac\nfunc Beacon() int { return 0 }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("main.go"),
        "package main\nimport (\n\t\"x/a\"\n\t\"x/beac\"\n)\nfunc Run() int { return a.Process() + beac.Beacon() }\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["index"]).assert().success();

    // Sanity: the unique `Beacon` resolved → section exists.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "Beacon", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_contains_call_site(&stdout, "main.go", 6);

    // Ambiguous `Process` (2 candidates) must NOT resolve to main.go.
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "Process", "--strict"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        !stdout.contains("main.go"),
        "ambiguous `Process` (2 candidates) must not resolve via the \
         single-candidate fallback; got:\n{stdout}"
    );
}
