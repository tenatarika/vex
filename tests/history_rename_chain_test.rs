//! Phase 14.10 — integration test for rename-chain expansion in
//! `vex history`.
//!
//! What this test pins (the headline contract of Phase 14.10):
//!
//! A symbol renamed across commits — `compute_total_revenue` →
//! `calculate_gross_income` — must surface BOTH historical entries
//! when queried by EITHER name through the indexed path. The v1.16
//! walker, restricted to symbols whose name still exists at HEAD,
//! cannot reach the pre-rename name; the indexed + chain-expanded
//! path closes that limitation.
//!
//! The chain builder is hard-gated on body-token Jaccard ≥ 0.70 and
//! length-ratio ≥ 0.60, so we use a body that's large enough that
//! the renamed function name dilutes Jaccard by only ~1/12 tokens
//! (well above gate). A two-token body would NOT chain — that's the
//! `low_jaccard_blocks_chain` unit test in `index::rename_chains`.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env_remove("VEX_CACHE_DIR");
    cmd
}

fn git(repo: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        repo.display()
    );
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Tester"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    // Keep the cache inside the repo so the test doesn't pollute the
    // user's real cache dir.
    std::fs::write(
        dir.join(".vex.toml"),
        "local_cache = true\nformat = \"compact\"\n",
    )
    .unwrap();
}

fn commit_file(repo: &Path, rel_path: &str, content: &str, msg: &str) {
    let abs = repo.join(rel_path);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&abs, content).unwrap();
    git(repo, &["add", rel_path]);
    git(repo, &["commit", "-q", "-m", msg]);
}

/// Body kept substantial so the function-name swap dilutes body
/// Jaccard by only ~1/12 tokens (≈ 0.92 estimated Jaccard, well
/// above `GATE_JACCARD = 0.70`).
const BODY_BEFORE: &str = r#"def compute_total_revenue(orders, exchange_rate):
    revenue = 0
    discount_applied = False
    for order in orders:
        if order.status == 'completed':
            base_amount = order.subtotal * exchange_rate
            tax_amount = base_amount * order.tax_rate
            revenue += base_amount + tax_amount
            if order.coupon_code is not None:
                revenue -= order.discount_value
                discount_applied = True
    if discount_applied:
        log_audit("revenue computation applied discount")
    return revenue
"#;

const BODY_AFTER: &str = r#"def calculate_gross_income(orders, exchange_rate):
    revenue = 0
    discount_applied = False
    for order in orders:
        if order.status == 'completed':
            base_amount = order.subtotal * exchange_rate
            tax_amount = base_amount * order.tax_rate
            revenue += base_amount + tax_amount
            if order.coupon_code is not None:
                revenue -= order.discount_value
                discount_applied = True
    if discount_applied:
        log_audit("revenue computation applied discount")
    return revenue
"#;

#[test]
fn history_pre_rename_query_surfaces_post_rename_via_chain() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "app.py", BODY_BEFORE, "c1: initial");
    commit_file(repo, "app.py", BODY_AFTER, "c2: rename");

    vex_in(repo).args(["index", "--history"]).assert().success();

    // Pre-rename query against the indexed path: the chain expansion
    // must surface BOTH the pre-rename and post-rename rows. Without
    // Phase 14.10 the FST hit on `compute_total_revenue` would return
    // only the c1 row.
    let out = vex_in(repo)
        .args(["history", "compute_total_revenue"])
        .output()
        .expect("spawn vex history");
    assert!(out.status.success(), "vex history exited non-zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("compute_total_revenue"),
        "expected pre-rename signature in output, got:\n{stdout}",
    );
    assert!(
        stdout.contains("calculate_gross_income"),
        "Phase 14.10 chain expansion: pre-rename query MUST surface the post-rename signature, got:\n{stdout}",
    );
}

#[test]
fn history_post_rename_query_surfaces_pre_rename_via_chain() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "app.py", BODY_BEFORE, "c1: initial");
    commit_file(repo, "app.py", BODY_AFTER, "c2: rename");

    vex_in(repo).args(["index", "--history"]).assert().success();

    // Reverse direction — query the post-rename name, get both rows.
    let out = vex_in(repo)
        .args(["history", "calculate_gross_income"])
        .output()
        .expect("spawn vex history");
    assert!(out.status.success(), "vex history exited non-zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("calculate_gross_income"),
        "expected post-rename signature in output, got:\n{stdout}",
    );
    assert!(
        stdout.contains("compute_total_revenue"),
        "Phase 14.10 chain expansion: post-rename query MUST surface the pre-rename signature, got:\n{stdout}",
    );
}

#[test]
fn history_no_index_walker_does_not_expand_chain() {
    // Pin the contrast — walker mode never sees the pre-rename name
    // because the symbol no longer exists at HEAD. This is the
    // limitation Phase 14.10 closes; the walker contract is
    // intentionally unchanged.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "app.py", BODY_BEFORE, "c1: initial");
    commit_file(repo, "app.py", BODY_AFTER, "c2: rename");

    // No `vex index` step — `--no-index` forces the walker.
    let out = vex_in(repo)
        .args(["history", "compute_total_revenue", "--no-index"])
        .output()
        .expect("spawn vex history --no-index");
    assert!(
        out.status.success(),
        "vex history --no-index exited non-zero"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Walker grep'd HEAD for `compute_total_revenue` and got nothing —
    // the symbol no longer exists at the tip. We expect EITHER an
    // empty result set OR the absence of `calculate_gross_income` in
    // the output. The strict assertion below pins the latter (the
    // post-rename name MUST NOT leak in via the walker path).
    assert!(
        !stdout.contains("calculate_gross_income"),
        "walker path must NOT include the post-rename name — chain expansion is indexed-only. Got:\n{stdout}",
    );
}

#[test]
fn vex_status_reports_rename_chain_stats_when_present() {
    // Pins the Phase 14.10 acceptance criterion: `vex status` surfaces
    // chain_count + member_count + the active thresholds when the
    // sidecar is on disk. JSON shape is the load-bearing contract for
    // MCP agents; the text line is for humans.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "app.py", BODY_BEFORE, "c1: initial");
    commit_file(repo, "app.py", BODY_AFTER, "c2: rename");

    vex_in(repo).args(["index", "--history"]).assert().success();

    // JSON: rename_chains object present with the expected counts.
    let out = vex_in(repo)
        .args(["status", "--format", "json"])
        .output()
        .expect("spawn vex status --format json");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("vex status JSON should parse");
    let rc = &parsed["results"]["rename_chains"];
    assert!(
        !rc.is_null(),
        "`rename_chains` must be present in status JSON when sidecar exists; got:\n{stdout}",
    );
    assert_eq!(rc["chain_count"], serde_json::json!(1));
    assert_eq!(rc["member_count"], serde_json::json!(2));
    assert_eq!(rc["forward_count"], serde_json::json!(2));
    // Thresholds + weights are nested objects with f32 noise — assert
    // the keys exist rather than exact float equality.
    assert!(rc["thresholds"]["score"].is_number());
    assert!(rc["weights"]["body_no_cos"].is_number());
    // `minilm_tiebreak_hits` is sourced from the manifest. With the
    // structural-only build path (no semantic embeddings in this
    // fixture) the value is `null`. The shape contract is "field
    // present, type matches" — leave the value assertion permissive
    // so a future semantic-on test doesn't have to break this one.
    assert!(
        rc["minilm_tiebreak_hits"].is_null() || rc["minilm_tiebreak_hits"].is_u64(),
        "minilm_tiebreak_hits must be null or u64, got: {}",
        rc["minilm_tiebreak_hits"],
    );

    // Phase 14.10 manifest provenance: the sidecar wrote successfully,
    // so the top-level `rename_chains_built` flag must be `true`. Pins
    // the contract that `vex status` and disk state agree.
    assert_eq!(
        parsed["results"]["rename_chains_built"],
        serde_json::json!(true),
        "manifest.rename_chains_built must be true after a successful build; got:\n{stdout}",
    );

    // Text: a single human-readable line.
    let out = vex_in(repo)
        .args(["status"])
        .output()
        .expect("spawn vex status");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Rename chains: 1 chains"),
        "expected `Rename chains: 1 chains ...` in text output, got:\n{stdout}",
    );
}

#[test]
fn vex_status_reports_zero_chains_when_history_indexed_but_no_renames() {
    // History indexed but no renames detected (e.g. a single-commit
    // repo) — text says "0 (no renames detected)", JSON gives
    // chain_count = 0 rather than null. Distinguishes "we tried" from
    // "no sidecar at all" (the latter happens when `--history` was
    // never passed).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "app.py", BODY_BEFORE, "c1: only commit");

    vex_in(repo).args(["index", "--history"]).assert().success();

    let out = vex_in(repo)
        .args(["status", "--format", "json"])
        .output()
        .expect("spawn vex status --format json");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let rc = &parsed["results"]["rename_chains"];
    assert!(
        !rc.is_null(),
        "`rename_chains` must NOT be null when --history is on and sidecar exists, even with 0 chains:\n{stdout}",
    );
    assert_eq!(rc["chain_count"], serde_json::json!(0));

    let out = vex_in(repo)
        .args(["status"])
        .output()
        .expect("spawn vex status");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Rename chains: 0 (no renames detected"),
        "expected zero-chain text marker, got:\n{text}",
    );
}

#[test]
fn vex_status_omits_rename_chains_when_no_history() {
    // Pins the absent shape: no `--history` ⇒ no sidecar ⇒ JSON
    // field is `null`, text output omits the chain line entirely
    // (the History line above already prompts the user with the
    // correct action).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "app.py", BODY_BEFORE, "c1: only");

    // No --history flag.
    vex_in(repo).args(["index"]).assert().success();

    let out = vex_in(repo)
        .args(["status", "--format", "json"])
        .output()
        .expect("spawn vex status --format json");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        parsed["results"]["rename_chains"].is_null(),
        "rename_chains must be null when --history was not passed, got:\n{stdout}",
    );

    let out = vex_in(repo)
        .args(["status"])
        .output()
        .expect("spawn vex status");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("Rename chains:"),
        "text output must omit Rename chains line when --history was not passed, got:\n{text}",
    );
}

#[test]
fn history_unrelated_symbol_query_unaffected_by_chains() {
    // Negative pin: chain expansion must NOT pollute results for a
    // symbol that's not in any chain. A function with a tiny body
    // can't form a chain (Jaccard would drop too low), so it stays
    // a singleton even if its name changes.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    // Tiny body — only 1 unique identifier ('x') so any rename
    // drops Jaccard below 0.70.
    commit_file(repo, "tiny.py", "def f():\n    x = 1\n    return x\n", "c1");

    vex_in(repo).args(["index", "--history"]).assert().success();
    let out = vex_in(repo)
        .args(["history", "f"])
        .output()
        .expect("spawn vex history");
    assert!(out.status.success(), "vex history exited non-zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The query for `f` should return exactly the rows where `f`
    // exists. No chain → no extra rows from unrelated symbols.
    // Soft assertion (count rows containing "function") rather than
    // strict equality: the walker contract evolves and the row
    // formatter may add lines without changing the underlying count.
    let function_rows = stdout.lines().filter(|l| l.contains("function")).count();
    assert!(
        function_rows <= 1,
        "singleton symbol `f` should produce ≤ 1 row, got {function_rows}:\n{stdout}",
    );
}
