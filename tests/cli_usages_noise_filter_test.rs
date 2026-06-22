//! v1.20.0 D2 — non-strict `usages` strips two classes of noise that
//! the legacy FST lookup used to surface:
//!
//!   1. The row at the queried symbol's own definition line — `find
//!      all callers` doesn't want the declaration showing up as a
//!      usage. Override with `--include-self`.
//!   2. Mentions in `*.md` / `*.markdown` / `*.txt` / `*.rst` /
//!      `*.adoc` files — README/CHANGELOG mentions are prose, not
//!      callers. Override with `--include-docs`.
//!
//! Strict mode is unaffected (the scope-binder excludes def-sites by
//! construction and doesn't index docs in the first place).

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

/// Project: `payment_processor` defined at `src/lib.rs:1`, called at
/// `src/lib.rs:4`, mentioned by name in `README.md`. The FST refs
/// section catches all three; only the body-line call is a real
/// "caller".
fn write_project_with_def_and_doc(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn payment_processor() {}\n\nfn caller_fn() {\n    payment_processor();\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("README.md"),
        "# Demo project\n\nDocuments the `payment_processor` helper.\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn default_strips_def_site_and_readme_mentions() {
    let tmp = TempDir::new().unwrap();
    write_project_with_def_and_doc(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "payment_processor"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    // Body-line call IS a real usage and must remain.
    let has_body_line = stdout.contains("src/lib.rs:4") || stdout.contains("src\\lib.rs:4");
    assert!(
        has_body_line,
        "the body-line call at src/lib.rs:4 must survive D2 filters; got: {stdout}"
    );
    // Def-site at src/lib.rs:1 must be stripped.
    assert!(
        !(stdout.contains("src/lib.rs:1") || stdout.contains("src\\lib.rs:1")),
        "the def-site row at src/lib.rs:1 must be stripped by default; got: {stdout}"
    );
    // README mention must be stripped.
    assert!(
        !stdout.contains("README.md"),
        "README.md prose mentions must be stripped by default; got: {stdout}"
    );
}

#[test]
fn include_self_keeps_def_site() {
    let tmp = TempDir::new().unwrap();
    write_project_with_def_and_doc(tmp.path());

    let assert =
        assert_ran(vex_in(tmp.path()).args(["usages", "payment_processor", "--include-self"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    let has_def = stdout.contains("src/lib.rs:1") || stdout.contains("src\\lib.rs:1");
    assert!(
        has_def,
        "--include-self must preserve the def-site row at src/lib.rs:1; got: {stdout}"
    );
}

#[test]
fn include_docs_keeps_readme_mentions() {
    let tmp = TempDir::new().unwrap();
    write_project_with_def_and_doc(tmp.path());

    let assert =
        assert_ran(vex_in(tmp.path()).args(["usages", "payment_processor", "--include-docs"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    assert!(
        stdout.contains("README.md"),
        "--include-docs must preserve README.md mentions; got: {stdout}"
    );
}

#[test]
fn why_trace_reports_def_site_dropped_and_docs_dropped_counts() {
    let tmp = TempDir::new().unwrap();
    write_project_with_def_and_doc(tmp.path());

    // `--why` emits the trace as the LAST line of stderr prefixed by
    // `VEX_WHY:` (see src/cli/trace.rs).
    let assert = assert_ran(vex_in(tmp.path()).args(["usages", "payment_processor", "--why"]));
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let trace_line = stderr
        .lines()
        .rev()
        .find(|l| l.contains("VEX_WHY:"))
        .unwrap_or_else(|| panic!("no VEX_WHY trace on stderr; got: {stderr}"));
    let json_start = trace_line.find('{').expect("trace must contain JSON");
    let trace: serde_json::Value =
        serde_json::from_str(&trace_line[json_start..]).expect("trace JSON parse");

    assert_eq!(
        trace["def_site_dropped"].as_u64(),
        Some(1),
        "trace must report exactly 1 def-site dropped; got: {trace}"
    );
    assert_eq!(
        trace["docs_dropped"].as_u64(),
        Some(1),
        "trace must report exactly 1 docs row dropped; got: {trace}"
    );
}

#[test]
fn strict_mode_unaffected_by_include_flags() {
    // The scope-binder doesn't index doc files and excludes the
    // def-site by construction. Strict + the two new flags must
    // behave identically to strict alone — no panic, no row leak.
    let tmp = TempDir::new().unwrap();
    write_project_with_def_and_doc(tmp.path());

    let assert = assert_ran(vex_in(tmp.path()).args([
        "usages",
        "payment_processor",
        "--strict",
        "--include-self",
        "--include-docs",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    assert!(
        !stdout.contains("README.md"),
        "strict must never surface README.md regardless of --include-docs; got: {stdout}"
    );
    let has_body_line = stdout.contains("src/lib.rs:4") || stdout.contains("src\\lib.rs:4");
    assert!(
        has_body_line,
        "strict must still return the body-line call site; got: {stdout}"
    );
}
