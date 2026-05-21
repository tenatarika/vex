//! CLI-level integration tests for the v1.7 kind-aware reranking
//! changes (11.9): multi-value `--kind`, alias parsing, and the
//! defs-first default ordering for Markdown headings.
//!
//! Each test builds a tiny project mixing source code with a Markdown
//! file so the heading demote and the `--kind comment` alias have
//! something visible to act on.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write_mixed_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    // A function named `payment_processor` plus a Markdown heading with
    // the same word inside — without the heading demote default the
    // heading would frequently outrank the function for short queries.
    std::fs::write(
        dir.join("src").join("api.rs"),
        "pub fn payment_processor() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("README.md"),
        "# Payment Processor\n\nNotes on the module.\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn function_outranks_heading_by_default() {
    let tmp = TempDir::new().unwrap();
    write_mixed_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "payment_processor", "--format", "compact"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let first_line = stdout.lines().next().unwrap_or("");
    assert!(
        first_line.contains("src/api.rs"),
        "expected function to outrank heading by default, first line was: {first_line}"
    );
}

#[test]
fn comment_kind_alias_promotes_heading() {
    let tmp = TempDir::new().unwrap();
    write_mixed_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args([
            "search", "Payment", "--kind", "comment", "--format", "compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let first_line = stdout.lines().next().unwrap_or("");
    assert!(
        first_line.contains("README.md"),
        "expected heading first under --kind comment, first line was: {first_line}"
    );
}

#[test]
fn kind_accepts_repeated_and_comma_values() {
    let tmp = TempDir::new().unwrap();
    write_mixed_project(tmp.path());

    // `--kind fn --kind heading` and `--kind fn,heading` should be
    // equivalent: both forms must parse without error.
    for args in [
        &["search", "Payment", "--kind", "fn", "--kind", "heading"][..],
        &["search", "Payment", "--kind", "fn,heading"][..],
    ] {
        vex_in(tmp.path()).args(args).assert().success();
    }
}

#[test]
fn explicit_kind_overrides_query_shape_default() {
    // `payment_processor` is FunctionLike by query shape — without
    // --kind, the function-shaped boost would dominate. With
    // `--kind class`, the class result must rise to the top instead.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        tmp.path().join("lib.rs"),
        "pub fn payment_processor() {}\nstruct payment_processor;\n",
    )
    .unwrap();
    // Add a class via a different language to make sure the kind is
    // visibly distinct (Rust has no `class`, so add an explicit one
    // via a TS file).
    std::fs::write(tmp.path().join("svc.ts"), "class payment_processor {}\n").unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let assert = vex_in(tmp.path())
        .args([
            "search",
            "payment_processor",
            "--kind",
            "class",
            "--format",
            "compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let first_line = stdout.lines().next().unwrap_or("");
    assert!(
        first_line.contains("svc.ts"),
        "expected class match first with --kind class, got: {first_line}"
    );
}

#[test]
fn every_canonical_kind_alias_parses() {
    // Guards against typos in the SymbolKind::from_str alias table.
    // Each canonical name + its documented short alias must parse
    // without error.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(tmp.path().join("lib.rs"), "fn foo() {}\n").unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let canonical_and_aliases = [
        "function",
        "fn",
        "method",
        "struct",
        "class",
        "interface",
        "trait",
        "enum",
        "type_alias",
        "type",
        "impl",
        "constant",
        "const",
        "property",
        "prop",
        "package",
        "pkg",
        "heading",
        "h",
        // 11.9 meta-selectors
        "def",
        "definition",
        "comment",
        "test",
        "ref",
        "reference",
    ];
    for k in canonical_and_aliases {
        vex_in(tmp.path())
            .args(["search", "foo", "--kind", k])
            .assert()
            .success();
    }
}

#[test]
fn unknown_kind_value_returns_helpful_error() {
    let tmp = TempDir::new().unwrap();
    write_mixed_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "Foo", "--kind", "banana"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("banana") && stderr.contains("unknown --kind"),
        "expected helpful unknown-kind error naming `banana`, got: {stderr}"
    );
}
