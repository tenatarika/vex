//! CLI integration tests for `vex bundle` (Phase 13.2).
//!
//! Inc 1 locked the envelope skeleton. Inc 2 wires the `--mode symbol`
//! pipeline (body + callers + callees + similar). Inc 3 / 4 add
//! `pr-impact` / `project`.

use std::path::Path;
use std::process::Command as StdCommand;

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

fn run_git(root: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git invocation");
    assert!(status.success(), "git {args:?} failed");
}

fn init_git_repo(dir: &Path) {
    run_git(dir, &["init", "-q", "-b", "main"]);
    run_git(dir, &["config", "user.email", "t@t"]);
    run_git(dir, &["config", "user.name", "T"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
}

fn commit_all(root: &Path, msg: &str) {
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-q", "-m", msg]);
}

/// Seed a tiny Rust project with a known caller/callee graph and build
/// an index. Layout (function `target` is the symbol we resolve in the
/// happy-path test):
///
///     src/lib.rs:
///         pub fn caller_one() { target(); }
///         pub fn caller_two() { target(); }
///         pub fn target() { helper_one(); helper_two(); }
///         pub fn helper_one() {}
///         pub fn helper_two() {}
///         pub fn unrelated() {}
///
/// Index is built without `--semantic` — that's covered by a dedicated
/// test which asserts the symbol mode degrades gracefully when the
/// index has no vectors.
fn seed_indexed_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn caller_one() { target(); }\n\
         pub fn caller_two() { target(); }\n\
         pub fn target() { helper_one(); helper_two(); }\n\
         pub fn helper_one() {}\n\
         pub fn helper_two() {}\n\
         pub fn unrelated() {}\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

/// Run `vex bundle …` and parse stdout as JSON, panicking on a real
/// error or unparseable output. Captures stderr so test logs show
/// indexing diagnostics when this fails.
///
/// v1.12.0 S8.2 — `vex bundle` returns exit code 1 when the bundle
/// `items` array is empty (e.g. the `bundle_*_empty_*` tests below
/// intentionally probe degraded shapes). Accept exit 0 or 1 via
/// `assert_ran` so those tests still validate the JSON envelope.
fn run_bundle(dir: &Path, args: &[&str]) -> serde_json::Value {
    let assert = assert_ran(vex_in(dir).args(args));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("`vex bundle {args:?}` stdout is not valid JSON: {e}\n---\n{stdout}")
    })
}

// ---------------------------------------------------------------------------
// Envelope shape (Inc 1 — still applies)
// ---------------------------------------------------------------------------

#[test]
fn bundle_symbol_stub_emits_protocol_version_v1() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "symbol", "--symbol", "target"],
    );
    assert_eq!(out["protocol_version"].as_str(), Some("v1"));
    assert_eq!(out["results"]["mode"].as_str(), Some("symbol"));
}

#[test]
fn bundle_invalid_mode_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    vex_in(tmp.path())
        .args(["bundle", "--mode", "not-a-mode"])
        .assert()
        .failure();
}

#[test]
fn bundle_envelope_advertises_phase_13_2_modes_in_capabilities() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "symbol", "--symbol", "target"],
    );
    let modes: Vec<&str> = out["capabilities"]["bundle_modes"]
        .as_array()
        .unwrap_or_else(|| panic!("capabilities.bundle_modes missing: {out}"))
        .iter()
        .map(|v| v.as_str().expect("bundle_modes entry must be a string"))
        .collect();
    assert_eq!(modes, vec!["symbol", "pr-impact", "project"]);
}

// ---------------------------------------------------------------------------
// `--mode symbol` pipeline (Inc 2)
// ---------------------------------------------------------------------------

fn items(out: &serde_json::Value) -> &Vec<serde_json::Value> {
    out["results"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("results.items missing: {out}"))
}

#[test]
fn bundle_symbol_includes_body_caller_callee_in_items() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "symbol", "--symbol", "target"],
    );

    let roles: Vec<&str> = items(&out)
        .iter()
        .filter_map(|i| i["role"].as_str())
        .collect();

    // Body item is always present when the symbol resolves.
    assert!(
        roles.contains(&"body"),
        "expected role: body, roles seen: {roles:?}"
    );
    // We seeded two callers (`caller_one`, `caller_two`) and two callees
    // (`helper_one`, `helper_two`) — at least one of each must surface.
    assert!(
        roles.contains(&"caller"),
        "expected role: caller, roles seen: {roles:?}"
    );
    assert!(
        roles.contains(&"callee"),
        "expected role: callee, roles seen: {roles:?}"
    );

    // Body item exposes the source.
    let body_item = items(&out)
        .iter()
        .find(|i| i["role"] == "body")
        .expect("body item");
    let body_text = body_item["body"]
        .as_str()
        .expect("body field must be set on role=body items");
    assert!(
        body_text.contains("fn target"),
        "body should contain the function definition; got: {body_text:?}"
    );
}

#[test]
fn bundle_symbol_respects_callers_max() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &[
            "bundle",
            "--mode",
            "symbol",
            "--symbol",
            "target",
            "--callers-max",
            "1",
        ],
    );
    let caller_count = items(&out).iter().filter(|i| i["role"] == "caller").count();
    assert!(
        caller_count <= 1,
        "--callers-max 1 should cap callers; got {caller_count}"
    );
    // And the `mode_hints.callers_truncated` should flip true.
    assert_eq!(
        out["results"]["mode_hints"]["callers_truncated"].as_bool(),
        Some(true),
        "with `--callers-max 1` and 2 known callers, callers_truncated must be true"
    );
}

#[test]
fn bundle_symbol_works_without_semantic_index() {
    // seed_indexed_project does NOT pass `--semantic`, so the index has
    // no vectors. The symbol mode must degrade to empty similar[] —
    // not error out.
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "symbol", "--symbol", "target"],
    );

    let similar_count = items(&out)
        .iter()
        .filter(|i| i["role"] == "similar")
        .count();
    assert_eq!(similar_count, 0, "similar must be empty without vectors");
    assert_eq!(
        out["results"]["mode_hints"]["has_vectors"].as_bool(),
        Some(false),
        "mode_hints.has_vectors should surface the degraded state"
    );
    assert_eq!(
        out["results"]["mode_hints"]["similar_count"].as_u64(),
        Some(0)
    );
}

#[test]
fn bundle_symbol_unknown_symbol_returns_envelope_with_empty_items() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &[
            "bundle",
            "--mode",
            "symbol",
            "--symbol",
            "definitely_not_a_real_symbol_xyz",
        ],
    );
    // Exit 0 + empty items (architect-review A: agent-friendly empty
    // mode, not non-zero exit).
    assert!(
        items(&out).is_empty(),
        "unknown symbol must return empty items"
    );
    assert_eq!(
        out["results"]["mode_hints"]["empty_reason"].as_str(),
        Some("symbol_not_found"),
        "empty_reason must explain the empty list"
    );
}

#[test]
fn bundle_symbol_rank_percentile_monotonic_descending() {
    // Locks the architect-review A6 decision — rank_percentile stays
    // GLOBAL monotonic-descending across all items[], preserving the
    // search-envelope invariant from cli_signals_test.rs:163.
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "symbol", "--symbol", "target"],
    );
    let ranks: Vec<f32> = items(&out)
        .iter()
        .map(|i| i["rank_percentile"].as_f64().unwrap() as f32)
        .collect();
    assert!(
        !ranks.is_empty(),
        "bundle should produce at least the body item"
    );
    // First is 1.0; last is 0.0 (when N > 1) or 1.0 (when N == 1).
    assert!(
        (ranks.first().copied().unwrap() - 1.0).abs() < 1e-6,
        "first rank must be 1.0, got {:?}",
        ranks.first()
    );
    for win in ranks.windows(2) {
        assert!(
            win[0] >= win[1],
            "rank_percentile must be monotonic descending across items; got {ranks:?}"
        );
    }
}

#[test]
fn bundle_symbol_role_rank_zero_indexed_within_role() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "symbol", "--symbol", "target"],
    );
    // For each role, the role_rank values must start at 0 and be a
    // contiguous-from-zero ordering — within the role bucket.
    use std::collections::HashMap;
    let mut by_role: HashMap<&str, Vec<u64>> = HashMap::new();
    for item in items(&out) {
        let role = item["role"].as_str().expect("role string");
        let rr = item["role_rank"].as_u64().expect("role_rank u64");
        by_role.entry(role).or_default().push(rr);
    }
    for (role, ranks) in &by_role {
        assert_eq!(
            ranks.first(),
            Some(&0),
            "role_rank for role={role} must start at 0; got {ranks:?}"
        );
        for win in ranks.windows(2) {
            assert!(
                win[1] == win[0] + 1,
                "role_rank for role={role} must be contiguous-ascending; got {ranks:?}"
            );
        }
    }
}

#[test]
fn bundle_symbol_emits_mode_hints_block() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "symbol", "--symbol", "target"],
    );
    let hints = &out["results"]["mode_hints"];
    // All five count/flag keys must be present (callers/callees/similar
    // counts + has_call_graph + has_vectors).
    for key in [
        "callers_count",
        "callees_count",
        "similar_count",
        "has_call_graph",
        "has_vectors",
        "callers_truncated",
        "callees_truncated",
        "similar_truncated",
    ] {
        assert!(
            !hints[key].is_null(),
            "mode_hints.{key} missing in: {hints}"
        );
    }
}

// ---------------------------------------------------------------------------
// `--mode pr-impact` pipeline (Inc 3)
// ---------------------------------------------------------------------------

/// Seed a git repo containing a chain `caller_a → caller_b → target`
/// and commit the initial state. Returns at HEAD with everything
/// committed and an index built. Tests modify files post-seed and run
/// `vex bundle --mode pr-impact --base HEAD` to trigger the diff path.
fn seed_pr_impact_repo(dir: &Path) {
    init_git_repo(dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn target() { /* before */ }\n\
         pub fn caller_b() { target(); }\n\
         pub fn caller_a() { caller_b(); }\n\
         pub fn unrelated() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tests").join("target_test.rs"),
        "fn it_calls_target() { super_target(); }\n\
         fn super_target() { target(); }\n",
    )
    .unwrap();
    commit_all(dir, "init");
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn bundle_pr_impact_lists_changed_symbols() {
    let tmp = TempDir::new().unwrap();
    seed_pr_impact_repo(tmp.path());

    // Modify `target` body — produces a BodyChanged change.
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn target() { let _ = 1; /* after */ }\n\
         pub fn caller_b() { target(); }\n\
         pub fn caller_a() { caller_b(); }\n\
         pub fn unrelated() {}\n",
    )
    .unwrap();

    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "pr-impact", "--base", "HEAD"],
    );
    assert_eq!(out["results"]["mode"].as_str(), Some("pr-impact"));
    let names: Vec<&str> = items(&out)
        .iter()
        .filter(|i| i["role"] == "changed")
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(
        names.contains(&"target"),
        "expected `target` as role=changed; got: {names:?}"
    );
    assert_eq!(
        out["results"]["mode_hints"]["changed_count"].as_u64(),
        Some(names.len() as u64)
    );
}

#[test]
fn bundle_pr_impact_depth_2_walks_two_hops() {
    let tmp = TempDir::new().unwrap();
    seed_pr_impact_repo(tmp.path());

    // Modify `target` so the BFS walks callers of `target`.
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn target() { let _ = 2; }\n\
         pub fn caller_b() { target(); }\n\
         pub fn caller_a() { caller_b(); }\n\
         pub fn unrelated() {}\n",
    )
    .unwrap();

    let out = run_bundle(
        tmp.path(),
        &[
            "bundle",
            "--mode",
            "pr-impact",
            "--base",
            "HEAD",
            "--depth",
            "2",
        ],
    );

    // With depth=2 we should reach `caller_b` (depth 1) AND `caller_a`
    // (depth 2). Anything else under role=transitive_caller is fine —
    // the test fixture's `super_target` lives in tests/ and is
    // classified as test, not transitive_caller.
    let transitive: Vec<&str> = items(&out)
        .iter()
        .filter(|i| i["role"] == "transitive_caller")
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(
        transitive.contains(&"caller_b"),
        "depth=2 must include caller_b (direct caller of target); got {transitive:?}"
    );
    assert!(
        transitive.contains(&"caller_a"),
        "depth=2 must include caller_a (two-hop caller of target); got {transitive:?}"
    );
}

#[test]
fn bundle_pr_impact_separates_tests_from_code() {
    let tmp = TempDir::new().unwrap();
    seed_pr_impact_repo(tmp.path());

    // Modify `target` — transitive walk should include `super_target`
    // which lives under `tests/target_test.rs`. The classifier should
    // route it to role=test (path heuristic on `/tests/`).
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn target() { let _ = 3; }\n\
         pub fn caller_b() { target(); }\n\
         pub fn caller_a() { caller_b(); }\n\
         pub fn unrelated() {}\n",
    )
    .unwrap();

    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "pr-impact", "--base", "HEAD"],
    );

    let tests: Vec<&str> = items(&out)
        .iter()
        .filter(|i| i["role"] == "test")
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(
        tests.contains(&"super_target"),
        "super_target (under tests/) must be classified as role=test; got {tests:?}"
    );

    // And conversely, nothing under tests/ should leak into transitive_caller.
    let transitive_paths: Vec<&str> = items(&out)
        .iter()
        .filter(|i| i["role"] == "transitive_caller")
        .filter_map(|i| i["path"].as_str())
        .collect();
    for p in &transitive_paths {
        assert!(
            !p.contains("/tests/"),
            "test paths must not appear under transitive_caller: {p}"
        );
    }
}

#[test]
fn bundle_pr_impact_requires_base_flag() {
    // Mirrors the MCP-layer test `args_for_bundle_pr_impact_requires_base`
    // at the CLI level — confirms the assembler bails with a message
    // pointing at the missing flag (review fix H2).
    let tmp = TempDir::new().unwrap();
    seed_pr_impact_repo(tmp.path());
    let assert = vex_in(tmp.path())
        .args(["bundle", "--mode", "pr-impact"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.to_lowercase().contains("base"),
        "error must mention the missing --base flag; got: {stderr}"
    );
}

#[test]
fn bundle_pr_impact_errors_when_no_call_graph() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn target() {}\n",
    )
    .unwrap();
    commit_all(tmp.path(), "init");
    // Build the index without the call graph section.
    vex_in(tmp.path())
        .args(["index", "--no-call-graph"])
        .assert()
        .success();
    // Modify something so the diff is non-empty.
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn target() { let _ = 1; }\n",
    )
    .unwrap();

    let assert = vex_in(tmp.path())
        .args(["bundle", "--mode", "pr-impact", "--base", "HEAD"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("call graph"),
        "error message should mention `call graph`; got: {stderr}"
    );
}

#[test]
fn bundle_pr_impact_empty_diff_returns_envelope_with_empty_items() {
    let tmp = TempDir::new().unwrap();
    seed_pr_impact_repo(tmp.path());

    // No file modifications — `git diff HEAD` is empty.
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "pr-impact", "--base", "HEAD"],
    );

    assert!(items(&out).is_empty(), "no diff → empty items[]");
    assert_eq!(
        out["results"]["mode_hints"]["empty_reason"].as_str(),
        Some("no_changes"),
        "empty mode_hints.empty_reason should explain the empty list"
    );
}

#[test]
fn bundle_pr_impact_surfaces_budget_fields_when_within_cap() {
    // H9 regression (v1.10.1): the aggregate-node cap and its sentinel
    // fields must always appear in `mode_hints` so callers can branch
    // on `budget_exceeded` without a guard for "field present?". A
    // small PR stays well under the cap — `budget_exceeded` is `false`
    // and `max_pr_impact_nodes` reflects the configured ceiling.
    let tmp = TempDir::new().unwrap();
    seed_pr_impact_repo(tmp.path());
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn target() { let _ = 5; }\n\
         pub fn caller_b() { target(); }\n\
         pub fn caller_a() { caller_b(); }\n\
         pub fn unrelated() {}\n",
    )
    .unwrap();

    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "pr-impact", "--base", "HEAD"],
    );
    let hints = &out["results"]["mode_hints"];
    assert_eq!(
        hints["budget_exceeded"].as_bool(),
        Some(false),
        "small PR must not exceed budget; got mode_hints: {hints}"
    );
    let cap = hints["max_pr_impact_nodes"].as_u64();
    assert!(
        cap.is_some_and(|c| c >= 1_000),
        "max_pr_impact_nodes must be surfaced as a positive integer; got mode_hints: {hints}"
    );
}

#[test]
fn bundle_pr_impact_populates_diff_filter_meta() {
    let tmp = TempDir::new().unwrap();
    seed_pr_impact_repo(tmp.path());
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn target() { let _ = 4; }\n\
         pub fn caller_b() { target(); }\n\
         pub fn caller_a() { caller_b(); }\n\
         pub fn unrelated() {}\n",
    )
    .unwrap();

    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "pr-impact", "--base", "HEAD"],
    );
    let diff = &out["_meta"]["vex.dev/diff_filter"];
    assert!(
        !diff.is_null(),
        "_meta.vex.dev/diff_filter must be present for pr-impact mode; got envelope: {out}"
    );
    let scope = diff["scope"].as_str().expect("scope str");
    assert!(
        scope.starts_with("pr-impact:"),
        "diff_filter.scope must be prefixed with `pr-impact:`; got: {scope}"
    );
    let paths = diff["changed_paths"]
        .as_array()
        .expect("changed_paths array");
    let path_strs: Vec<&str> = paths.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        path_strs.iter().any(|p| p.ends_with("lib.rs")),
        "changed_paths should include the modified file; got: {path_strs:?}"
    );
}

// ---------------------------------------------------------------------------
// `--mode project` pipeline (Inc 4) — top-N by reverse indegree
// ---------------------------------------------------------------------------

#[test]
fn bundle_project_returns_top_n() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "project", "--top-n", "10"],
    );
    assert_eq!(out["results"]["mode"].as_str(), Some("project"));

    let it = items(&out);
    assert!(
        !it.is_empty(),
        "project mode must yield items on a non-empty call graph"
    );
    // The seed fixture: target() called by caller_one + caller_two = indegree 2.
    // helper_one / helper_two each have indegree 1. So `target` MUST be the
    // top-ranked item.
    let first = &it[0];
    assert_eq!(
        first["name"].as_str(),
        Some("target"),
        "highest-indegree symbol must be `target` (2 callers); got: {first}"
    );
    assert_eq!(first["role"].as_str(), Some("top"));
    // Indegree surfaces via the dedicated `signals.indegree` field
    // (the locked Signals struct gained this in the review pass —
    // additive, `skip_serializing_if = Option::is_none`). NOT via
    // `bm25_rank`, which would be semantically wrong.
    assert_eq!(
        first["signals"]["indegree"].as_u64(),
        Some(2),
        "target should report indegree=2 in signals.indegree"
    );
    assert!(
        first["signals"]["bm25_rank"].is_null(),
        "indegree must NOT leak into the BM25 channel; got: {first}"
    );

    // mode_hints carries the scoring label + counts.
    let hints = &out["results"]["mode_hints"];
    assert_eq!(hints["scoring"].as_str(), Some("reverse_indegree"));
    assert_eq!(hints["top_n"].as_u64(), Some(10));
    assert!(hints["total_ranked_symbols"].as_u64().is_some());
    assert_eq!(hints["has_call_graph"].as_bool(), Some(true));
}

#[test]
fn bundle_project_caps_at_top_n() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(tmp.path(), &["bundle", "--mode", "project", "--top-n", "1"]);
    let it = items(&out);
    assert!(
        it.len() <= 1,
        "--top-n 1 must cap at one item; got {}",
        it.len()
    );
}

#[test]
fn bundle_project_path_glob_scopes_results() {
    // Two-file fixture: src/lib.rs has the call graph, src/other.rs adds a
    // separate symbol set. With `--path-glob 'src/other.rs'` the lib.rs
    // symbols must be excluded.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src").join("lib.rs"),
        "pub fn caller_one() { target(); }\n\
         pub fn caller_two() { target(); }\n\
         pub fn target() {}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src").join("other.rs"),
        "pub fn lone_island() {}\n",
    )
    .unwrap();
    vex_in(tmp.path()).args(["index"]).assert().success();

    let out = run_bundle(
        tmp.path(),
        &[
            "bundle",
            "--mode",
            "project",
            "--top-n",
            "10",
            "--path-glob",
            "src/other.rs",
        ],
    );
    let names: Vec<&str> = items(&out)
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"target"),
        "path-glob src/other.rs must exclude target from lib.rs; got: {names:?}"
    );
    // total_ranked_symbols counts the unfiltered rank surface, so it
    // should still reflect lib.rs callees (target had inbound edges).
    assert!(
        out["results"]["mode_hints"]["total_ranked_symbols"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "total_ranked_symbols should count pre-filter rank: {out}"
    );
}

#[test]
fn bundle_project_emits_directory_tree_in_mode_hints() {
    // FU-6 (v1.10.1): `--mode project` ships a directory-symbol-density
    // tree alongside the indegree-ranked items. The seed fixture lives
    // entirely under `src/`, so `src` must show up as the top entry
    // with `recursive_symbol_count` matching the total symbol count.
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "project", "--top-n", "10"],
    );
    let tree = out["results"]["mode_hints"]["directory_tree"]
        .as_array()
        .expect("directory_tree must be an array");
    assert!(
        !tree.is_empty(),
        "non-empty index must surface at least one directory; got: {out}"
    );
    let mut saw_src = false;
    let mut saw_root = false;
    for entry in tree {
        let p = entry["path"].as_str().expect("path str");
        if p == "src" {
            saw_src = true;
            assert!(
                entry["recursive_symbol_count"]
                    .as_u64()
                    .is_some_and(|n| n >= 1),
                "src directory must report >=1 recursive symbol; got: {entry}"
            );
        }
        if p == "." {
            saw_root = true;
            assert!(
                entry["recursive_symbol_count"]
                    .as_u64()
                    .is_some_and(|n| n >= 1),
                "root `.` must roll up >=1 symbol; got: {entry}"
            );
        }
    }
    assert!(
        saw_src,
        "directory_tree should include `src`; got: {tree:?}"
    );
    assert!(
        saw_root,
        "directory_tree should include the root `.` rollup; got: {tree:?}"
    );

    // Sort invariant: recursive_symbol_count descending.
    let counts: Vec<u64> = tree
        .iter()
        .map(|e| e["recursive_symbol_count"].as_u64().unwrap_or(0))
        .collect();
    let mut sorted = counts.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        counts, sorted,
        "directory_tree must be sorted by recursive_symbol_count descending"
    );
}

#[test]
fn bundle_project_directory_tree_only_skips_indegree_items() {
    // FU-6 (v1.10.1): `--directory-tree-only` short-circuits the
    // indegree walk so callers that only need the directory density
    // payload don't pay for the project's call-graph traversal.
    // Verifies `items: []` and the tree still shows up in mode_hints.
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "project", "--directory-tree-only"],
    );
    assert!(
        items(&out).is_empty(),
        "items[] must be empty under --directory-tree-only; got: {out}"
    );
    let hints = &out["results"]["mode_hints"];
    assert_eq!(
        hints["directory_tree_only"].as_bool(),
        Some(true),
        "mode_hints.directory_tree_only must echo the flag; got: {hints}"
    );
    assert_eq!(
        hints["scoring"].as_str(),
        Some("directory_tree_only"),
        "scoring label must reflect the directory-only path; got: {hints}"
    );
    let tree = hints["directory_tree"]
        .as_array()
        .expect("directory_tree must still be populated");
    assert!(!tree.is_empty());
}

#[test]
fn bundle_project_directory_tree_top_caps_entry_count() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &[
            "bundle",
            "--mode",
            "project",
            "--directory-tree-only",
            "--directory-tree-top",
            "1",
        ],
    );
    let tree = out["results"]["mode_hints"]["directory_tree"]
        .as_array()
        .expect("directory_tree array");
    assert!(
        tree.len() <= 1,
        "--directory-tree-top 1 must cap entries; got {} ({tree:?})",
        tree.len()
    );
}

#[test]
fn bundle_project_works_without_call_graph() {
    // Index built `--no-call-graph` → soft-degrade (NOT hard error,
    // unlike pr-impact). Empty items + empty_reason: "no_call_graph".
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src").join("lib.rs"), "pub fn alone() {}\n").unwrap();
    vex_in(tmp.path())
        .args(["index", "--no-call-graph"])
        .assert()
        .success();

    let out = run_bundle(tmp.path(), &["bundle", "--mode", "project"]);
    assert_eq!(out["results"]["mode"].as_str(), Some("project"));
    assert!(items(&out).is_empty());
    assert_eq!(
        out["results"]["mode_hints"]["empty_reason"].as_str(),
        Some("no_call_graph"),
    );
    assert_eq!(
        out["results"]["mode_hints"]["has_call_graph"].as_bool(),
        Some(false),
    );
}

#[test]
fn bundle_project_scoring_label_is_reverse_indegree() {
    // Locks architect-review A5 — `--mode project` is reverse indegree,
    // not PageRank. The mode_hints.scoring label is the authoritative
    // contract any future refactor that re-introduces PageRank MUST
    // update. We don't grep the full JSON blob (review nit — that
    // would false-fire on a user symbol named `*_pagerank_*`).
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "project", "--top-n", "10"],
    );
    let scoring = out["results"]["mode_hints"]["scoring"]
        .as_str()
        .expect("mode_hints.scoring must be present");
    assert_eq!(scoring, "reverse_indegree");
    for forbidden in ["pagerank", "damping", "weighted_score"] {
        assert_ne!(
            scoring, forbidden,
            "scoring label must remain `reverse_indegree`; got `{forbidden}`"
        );
    }
}

// ---------------------------------------------------------------------------
// H5 (partial) — envelope-contract uniformity across bundle modes.
// Phase 13 envelope must surface `protocol_version: "v1"` from every
// bundle arm (symbol / pr-impact / project), not just `symbol`. Regression
// guard for the review finding: "envelope contract only honored by
// `search` (and partly `bundle`)".
// ---------------------------------------------------------------------------

#[test]
fn bundle_project_mode_emits_protocol_version_v1() {
    let tmp = TempDir::new().unwrap();
    seed_indexed_project(tmp.path());
    let out = run_bundle(tmp.path(), &["bundle", "--mode", "project", "--top-n", "5"]);
    assert_eq!(
        out["protocol_version"].as_str(),
        Some("v1"),
        "project mode must surface protocol_version=v1 (H5 envelope contract)"
    );
    // `results` is the bundle payload — assert shape so a future refactor
    // that accidentally returns the bare payload (no envelope) trips here.
    assert_eq!(out["results"]["mode"].as_str(), Some("project"));
    assert!(
        out["capabilities"].is_object(),
        "envelope must carry the capabilities block"
    );
}

#[test]
fn bundle_pr_impact_mode_emits_protocol_version_v1() {
    // Seed a tiny git repo + commit + change so pr-impact has something
    // to diff. Mirrors the `assemble_pr_impact_*` happy path above.
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    seed_indexed_project(tmp.path());
    commit_all(tmp.path(), "init");
    // Touch the file so the diff against HEAD~0 yields at least the
    // working-tree-modified path; pr-impact mode tolerates an empty
    // diff (it just produces an empty items list) so even a no-op call
    // still emits the envelope.
    let out = run_bundle(
        tmp.path(),
        &["bundle", "--mode", "pr-impact", "--base", "HEAD"],
    );
    assert_eq!(
        out["protocol_version"].as_str(),
        Some("v1"),
        "pr-impact mode must surface protocol_version=v1 (H5 envelope contract)"
    );
    assert_eq!(out["results"]["mode"].as_str(), Some("pr-impact"));
    assert!(
        out["capabilities"].is_object(),
        "envelope must carry the capabilities block"
    );
}
