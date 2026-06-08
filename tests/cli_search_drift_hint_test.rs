//! v1.17 search-drift hint integration coverage.
//!
//! When the structural FST returns no symbol named the query AND
//! the query looks like a bare identifier (the typical
//! "imported-from-dependency / typo" case), `vex search` emits a
//! stderr hint pointing at `check` / `show` / `usages --strict` as
//! the precise-lookup tools. The hint is non-fatal and doesn't
//! appear in the JSON envelope on stdout.
//!
//! Pins:
//! 1. `vex search undefined_symbol` on a project where the name is
//!    only referenced (no local def) → stderr contains the hint.
//! 2. `vex search known_def` on a project where the name IS defined
//!    → stderr does NOT contain the hint (would be noise).
//! 3. `vex search "multi word phrase"` (not identifier-shaped) →
//!    no hint regardless of FST hits.
//! 4. JSON envelope on stdout is unchanged by the hint.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

const HINT_SUBSTRING: &str = "For exact-symbol lookup try `vex check";

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

/// Set up a project where `caller_fn` calls `undefined_symbol`
/// (which has NO local definition — it would be imported from a
/// dependency in a real project). Mirrors the user-reported feedback
/// scenario: `compile_query` imported from `chili_pg_utils`.
fn write_project_with_external_reference(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        r#"pub fn caller_fn() {
    undefined_symbol();
    undefined_symbol();
}
pub fn known_def() -> u8 { 7 }
"#,
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn hint_fires_when_identifier_has_no_local_definition() {
    let tmp = TempDir::new().unwrap();
    write_project_with_external_reference(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "undefined_symbol"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains(HINT_SUBSTRING),
        "stderr must carry the search-drift hint when an identifier-shaped \
         query has no structural FST match. Got stderr: {stderr}"
    );
    // Spot-check that all three suggested commands are named.
    for tool in ["vex check", "vex show", "vex usages"] {
        assert!(
            stderr.contains(tool),
            "hint should mention `{tool}`; got: {stderr}"
        );
    }
}

#[test]
fn hint_does_not_fire_for_defined_symbol() {
    let tmp = TempDir::new().unwrap();
    write_project_with_external_reference(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "known_def"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains(HINT_SUBSTRING),
        "hint must NOT fire when the symbol IS defined — would be noise. \
         Got stderr: {stderr}"
    );
}

#[test]
fn hint_does_not_fire_for_multi_word_query() {
    // Multi-word queries are clearly relevance queries (not exact
    // symbol lookups), so the hint would be a false positive.
    let tmp = TempDir::new().unwrap();
    write_project_with_external_reference(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "something nonexistent"])
        .assert();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains(HINT_SUBSTRING),
        "hint must NOT fire for multi-word queries (those are \
         clearly relevance queries, not exact-symbol lookups). \
         Got stderr: {stderr}"
    );
}

#[test]
fn hint_does_not_pollute_json_envelope() {
    // JSON output on stdout must remain a clean envelope — the
    // hint lives on stderr, so `vex search Foo --format json | jq`
    // still works.
    let tmp = TempDir::new().unwrap();
    write_project_with_external_reference(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "undefined_symbol", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    // Stdout must parse as JSON (proves no hint contamination).
    let envelope: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be valid JSON; got: {stdout}\nerror: {e}"));
    // Envelope must have the v1 shape.
    assert!(
        envelope.get("protocol_version").is_some(),
        "missing protocol_version"
    );
    assert!(envelope.get("results").is_some(), "missing results");
    // Stderr must STILL carry the hint (it just doesn't leak into stdout).
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains(HINT_SUBSTRING),
        "hint must STILL go to stderr in JSON mode. Got stderr: {stderr}"
    );
}
