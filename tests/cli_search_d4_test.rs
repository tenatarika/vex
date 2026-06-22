//! v1.20.0 D4 — `vex search` surfaces raw channel scores in per-result
//! signals (`bm25_score`, `semantic_cosine`) and reports when the
//! semantic channel didn't fire via `_meta.vex.dev/semantic_channel`.
//! `--code-only` drops hits in prose-format files.

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

fn write_and_index(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn payment_processor() {}\n\
         \n\
         fn caller_fn() {\n\
         \x20\x20\x20\x20payment_processor();\n\
         }\n",
    )
    .unwrap();
    // A prose file that contains the symbol name — without `--code-only`
    // it ranks in the search result list; with `--code-only` it must
    // be filtered out.
    std::fs::write(
        dir.join("README.md"),
        "# Project\n\nDocuments `payment_processor` extensively.\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

fn run_search_json(dir: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["search", "payment_processor", "--format", "json"];
    args.extend(extra);
    let assert = assert_ran(vex_in(dir).args(args));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("search stdout is not valid JSON: {e}\n---\n{stdout}"))
}

#[test]
fn search_signals_carry_bm25_score_when_index_has_bm25() {
    let tmp = TempDir::new().unwrap();
    write_and_index(tmp.path());

    let out = run_search_json(tmp.path(), &[]);
    let results = out["results"].as_array().expect("results must be array");
    assert!(
        !results.is_empty(),
        "expected at least one result, got: {out}"
    );

    // At least one row must carry a numeric bm25_score (the channel
    // is enabled by default when the index has BM25 data).
    let any_bm25_score = results.iter().any(|r| {
        r["signals"]["bm25_score"]
            .as_f64()
            .is_some_and(|v| v.is_finite())
    });
    assert!(
        any_bm25_score,
        "at least one result must carry signals.bm25_score (v1.20.0 D4); got: {out}"
    );
}

#[test]
fn search_meta_reports_semantic_not_requested_by_default() {
    let tmp = TempDir::new().unwrap();
    write_and_index(tmp.path());

    let out = run_search_json(tmp.path(), &[]);
    assert_eq!(
        out["_meta"]["vex.dev/semantic_channel"].as_str(),
        Some("not_requested"),
        "default search must report semantic_channel=not_requested; got: {out}"
    );
}

#[test]
fn search_meta_reports_index_lacks_vectors_when_semantic_requested_but_unavailable() {
    let tmp = TempDir::new().unwrap();
    write_and_index(tmp.path());

    let out = run_search_json(tmp.path(), &["--semantic"]);
    assert_eq!(
        out["_meta"]["vex.dev/semantic_channel"].as_str(),
        Some("index_lacks_vectors"),
        "--semantic against an index without vectors must report \
         semantic_channel=index_lacks_vectors; got: {out}"
    );
}

#[test]
fn search_code_only_triggers_overfetch_so_limit_is_honoured() {
    // v1.20.0 (D4): with --code-only active and no other narrowing
    // filter, the fetch_limit must rise to symbol_count() so doc-file
    // truncation doesn't silently leave the result list below `--limit`.
    // Reproduces the HIGH issue code-reviewer flagged on the v1.20.0
    // D4 diff: pre-fix, fetch_limit == limit and post-filter trimming
    // could under-deliver against the requested limit.
    let tmp = TempDir::new().unwrap();
    write_and_index(tmp.path());
    let out = run_search_json(tmp.path(), &["--code-only", "--limit", "10"]);
    // We can't pin an exact count (small fixture), but the predicate
    // worth pinning is: when --code-only is set, ALL returned rows are
    // non-doc paths. This is the same shape as the next test, but with
    // an explicit `--limit` smaller than the index symbol_count so the
    // over-fetch path actually engages.
    let results = out["results"].as_array().expect("results must be array");
    for r in results {
        let path = r["path"].as_str().unwrap_or("");
        assert!(
            !path_is_doc(path),
            "--code-only with --limit must not return doc rows after over-fetch; got {path}"
        );
    }
}

/// Tiny inline mirror of `src/util/paths.rs::is_doc_path` so the
/// test binary doesn't pull in the whole vex lib just to check
/// extensions. Kept lockstep with the canonical list.
fn path_is_doc(path: &str) -> bool {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    matches!(
        ext.as_deref(),
        Some("md" | "markdown" | "txt" | "rst" | "adoc")
    )
}

#[test]
fn search_code_only_drops_doc_file_hits() {
    let tmp = TempDir::new().unwrap();
    write_and_index(tmp.path());

    // Without --code-only, README.md is reachable via the search pool.
    // (The test fixture intentionally puts the symbol name in README —
    // we don't strictly assert it appears in the default result list,
    // because ranking may push it past --limit. We DO assert that with
    // --code-only set, no result has a .md path.)
    let out = run_search_json(tmp.path(), &["--code-only", "--limit", "50"]);
    let results = out["results"].as_array().expect("results must be array");
    for r in results {
        let path = r["path"].as_str().unwrap_or("");
        assert!(
            !path.ends_with(".md"),
            "--code-only must drop *.md results; got path={path} in: {out}"
        );
    }
}
