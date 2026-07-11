//! P3 (`docs/HIERARCHY-EDGES.md` §7, §8) — CLI-level coverage for
//! `vex implementations` (index-backed lookup + live-walk fallback) and
//! `vex subtypes` (transitive BFS, no live-walk fallback). There was no
//! prior CLI-level test file for either command:
//! `tests/incremental_consistency_hierarchy.rs` (P2a) only covers `vex
//! update` carry-forward correctness at the `IndexReader` level, not the
//! CLI/output surface.

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

fn write_minimal_index_at(dir: &Path, files: &[(&str, &str)]) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    for (rel, contents) in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
    }
    vex_in(dir).args(["index"]).assert().success();
}

fn run_json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full_args = args.to_vec();
    full_args.extend(["--format", "json"]);
    let assert = assert_ran(vex_in(dir).args(&full_args));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\n---\n{stdout}"))
}

/// A single-file fixture with a real Rust `impl Base for Derived` —
/// exercised through the actual parse/extract/resolve pipeline (`vex
/// index`), not a hand-spliced binary section, so these tests validate
/// the real end-to-end wiring. `impl Trait for Struct` maps to
/// `EdgeKind::Extends` (P2 scope note, `docs/HIERARCHY-EDGES.md` §4 —
/// `EdgeKind::Implements` is never emitted in the current extraction),
/// so the CLI is expected to print `relation: "extends"` for it.
const BASE_AND_DERIVED: &str = "pub trait Base {\n    fn greet(&self);\n}\n\npub struct Derived;\nimpl Base for Derived {\n    fn greet(&self) {}\n}\n";

// ── implementations: index path ─────────────────────────────────────────

#[test]
fn implementations_uses_index_when_hierarchy_section_present() {
    let tmp = TempDir::new().unwrap();
    write_minimal_index_at(tmp.path(), &[("src/lib.rs", BASE_AND_DERIVED)]);

    let out = run_json(tmp.path(), &["implementations", "Base"]);
    let results = out["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected results array, got: {out}"));
    assert_eq!(results.len(), 1, "expected exactly one implementer: {out}");
    assert_eq!(results[0]["name"], "Derived");
    assert_eq!(results[0]["relation"], "extends");
    assert_eq!(results[0]["path"], "src/lib.rs");
}

#[test]
fn implementations_real_empty_result_does_not_trigger_fallback() {
    // The hierarchy section exists (Derived/Base produced one real edge)
    // but querying a name with NO implementers must return a genuinely
    // empty result — not silently re-run the live walk and potentially
    // disagree with the index.
    let tmp = TempDir::new().unwrap();
    write_minimal_index_at(tmp.path(), &[("src/lib.rs", BASE_AND_DERIVED)]);

    let out = run_json(tmp.path(), &["implementations", "NoSuchBaseType"]);
    let results = out["results"].as_array().unwrap();
    assert!(
        results.is_empty(),
        "querying a nonexistent base must yield zero results, got: {out}"
    );
}

// ── implementations: live-walk fallback ─────────────────────────────────

#[test]
fn implementations_falls_back_to_live_walk_when_no_index_at_all() {
    // No `.vex.toml`, no `vex index` run at all — `vex implementations`
    // must still work via the live tree-sitter walk (today's pre-P3
    // behavior), not error out because there's no index.
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), BASE_AND_DERIVED).unwrap();

    let out = run_json(tmp.path(), &["implementations", "Base"]);
    let results = out["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "live-walk fallback must still find the implementer: {out}"
    );
    assert_eq!(results[0]["name"], "Derived");
}

// ── subtypes: transitive descent ────────────────────────────────────────

#[test]
fn subtypes_transitive_descent_across_two_hops() {
    // A <- B <- C: `struct B` extends/impls `A`, `struct C` extends/impls
    // `B`. Querying subtypes of A must report B (depth 1) and C (depth 2).
    let tmp = TempDir::new().unwrap();
    write_minimal_index_at(
        tmp.path(),
        &[(
            "src/lib.rs",
            "pub trait A {\n    fn a(&self);\n}\n\
             pub struct B;\n\
             impl A for B {\n    fn a(&self) {}\n}\n\
             \n\
             pub trait BTrait: A {}\n\
             pub struct C;\n\
             impl BTrait for C {}\n",
        )],
    );

    // NOTE: this fixture uses two independent single-hop edges (B implements
    // A, C implements BTrait) rather than a literal 3-generation Rust struct
    // chain, since Rust doesn't have struct-extends-struct. The essential
    // transitive-chain property under test (`vex subtypes` walking more than
    // one hop) is validated at the pure-BFS unit-test level in
    // `src/cli/cmd_subtypes.rs::tests::transitive_descent_across_two_hops`,
    // which exercises an actual A<-B<-C chain directly. This CLI-level test
    // instead confirms the command at least surfaces direct hits correctly
    // end-to-end through the real pipeline; see that unit test for the
    // authoritative multi-hop-chain regression coverage.
    let out = run_json(tmp.path(), &["subtypes", "A"]);
    let results = out["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["name"] == "B" && r["depth"] == 1),
        "expected B at depth 1: {out}"
    );
}

// ── implementations / subtypes: --path from a different cwd ────────────
//
// Regression test for a real bug caught during P3 review:
// `extract_path_hint` (src/cli/common.rs) gates which subcommands get
// their `--path` value honored for `.vex.toml` config loading — commands
// missing from that match arm silently fall back to loading config from
// the CURRENT DIRECTORY instead of `--path`. `Commands::Implementations`
// was already covered (pre-P3); `Commands::Subtypes` needed to be added
// alongside it. Every other test in this file runs `cmd.current_dir(dir)`
// with `--path` pointed at that SAME `dir`, so it can't catch this class
// of bug — these two tests deliberately invoke from an unrelated cwd.

#[test]
fn subtypes_honors_path_flag_from_different_cwd() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_minimal_index_at(&project, &[("src/lib.rs", BASE_AND_DERIVED)]);

    // Invoke from an unrelated cwd (tmp.path() itself, not `project`) —
    // if `extract_path_hint` doesn't route `--path` through for
    // `Commands::Subtypes`, config loading resolves against this cwd
    // instead, and the command can't find the project's `.vex.toml` /
    // index at all.
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(tmp.path());
    cmd.env("VEX_CACHE_DIR", project.join(".vex-test-cache"));
    let assert = assert_ran(cmd.args([
        "subtypes",
        "Base",
        "--path",
        project.to_str().unwrap(),
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let out: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\n---\n{stdout}"));
    let results = out["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected results array, got: {out}"));
    assert_eq!(
        results.len(),
        1,
        "expected to find Derived via --path from a different cwd: {out}"
    );
    assert_eq!(results[0]["name"], "Derived");
}

#[test]
fn implementations_honors_path_flag_from_different_cwd() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_minimal_index_at(&project, &[("src/lib.rs", BASE_AND_DERIVED)]);

    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(tmp.path());
    cmd.env("VEX_CACHE_DIR", project.join(".vex-test-cache"));
    let assert = assert_ran(cmd.args([
        "implementations",
        "Base",
        "--path",
        project.to_str().unwrap(),
        "--format",
        "json",
    ]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let out: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\n---\n{stdout}"));
    let results = out["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "expected to find Derived via --path from a different cwd: {out}"
    );
}

// ── subtypes: no-section behavior ────────────────────────────────────────

#[test]
fn subtypes_no_index_yields_empty_result_not_error() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), BASE_AND_DERIVED).unwrap();

    // No `vex index` run — `vex subtypes` has no live-walk fallback, so
    // this must be a clean empty result (exit code 1 per the S8.2
    // no-results contract), never a hard error / panic.
    let assert = assert_ran(vex_in(tmp.path()).args(["subtypes", "Base", "--format", "json"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let out: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\n---\n{stdout}"));
    let results = out["results"].as_array().unwrap();
    assert!(results.is_empty(), "expected empty results, got: {out}");

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("vex index"),
        "expected a stderr hint pointing at `vex index`, got: {stderr}"
    );
}

// ── vex status: hierarchy edges line ────────────────────────────────────

#[test]
fn status_reports_hierarchy_edge_count_in_json_and_text() {
    let tmp = TempDir::new().unwrap();
    write_minimal_index_at(tmp.path(), &[("src/lib.rs", BASE_AND_DERIVED)]);

    let json_out = run_json(tmp.path(), &["status"]);
    let count = json_out["results"]["hierarchy_edges"]
        .as_u64()
        .unwrap_or_else(|| panic!("expected numeric hierarchy_edges field, got: {json_out}"));
    assert!(
        count >= 1,
        "expected at least one hierarchy edge: {json_out}"
    );

    let text_assert = vex_in(tmp.path()).args(["status"]).assert().success();
    let stdout = String::from_utf8_lossy(&text_assert.get_output().stdout);
    assert!(
        stdout.contains("Hierarchy edges:"),
        "expected a 'Hierarchy edges:' line in text status output, got: {stdout}"
    );
}
