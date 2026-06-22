//! v1.20.0 F1 — `vex impact <Symbol>` blast-radius / delete-safety
//! command. Composes strict refs + FST refs + grep word-boundary +
//! call-graph callers into a single verdict.
//!
//! The verdict logic is unit-tested at the channel-counts level in
//! `src/cli/cmd_impact.rs::tests`. These integration tests pin the
//! end-to-end behaviour through real fixtures: that the CLI binary
//! emits the right verdict for representative `safe` / `unsafe` /
//! `uncertain` scenarios, and that the JSON envelope carries the
//! contracted shape.

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

fn run_impact_json(dir: &Path, symbol: &str) -> serde_json::Value {
    let assert = assert_ran(vex_in(dir).args(["impact", symbol, "--format", "json"]));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("impact stdout is not valid JSON: {e}\n---\n{stdout}"))
}

#[test]
fn impact_verdict_safe_when_symbol_does_not_exist() {
    // Project with a real call site so the indexer writes both the v5
    // reference_edges section AND the v4 call graph — both binder
    // channels report `available: true`. Then query a totally
    // nonexistent symbol: every channel reports 0 hits, and because
    // ≥1 binder channel ran, the verdict can honestly be `safe`.
    //
    // (If the fixture has no calls at all, the binder + call-graph
    // sections stay empty, both channels report `available: false`,
    // and the verdict correctly downgrades to `uncertain` — see the
    // dedicated unit tests in `cmd_impact.rs::tests` for that path.)
    let tmp = TempDir::new().unwrap();
    write_minimal_index_at(
        tmp.path(),
        &[(
            "src/lib.rs",
            "pub fn payment_processor() {}\n\
             \n\
             fn caller_fn() {\n\
             \x20\x20\x20\x20payment_processor();\n\
             }\n",
        )],
    );

    let out = run_impact_json(tmp.path(), "totally_nonexistent_symbol_xyz");
    let results = &out["results"];
    assert_eq!(
        results["verdict"].as_str(),
        Some("safe"),
        "nonexistent symbol must be safe to 'delete' when binder channels ran, got: {results}"
    );
    assert_eq!(
        results["channels"]["strict_refs"]["available"].as_bool(),
        Some(true),
        "fixture must produce a v5 ref_edges section (sanity check), got: {results}"
    );
    assert_eq!(
        results["channels"]["strict_refs"]["count"].as_u64(),
        Some(0),
        "got: {results}"
    );
    assert_eq!(
        results["channels"]["call_graph_callers"]["available"].as_bool(),
        Some(true),
        "fixture must produce a v4 call graph (sanity check), got: {results}"
    );
    assert_eq!(
        results["channels"]["call_graph_callers"]["count"].as_u64(),
        Some(0),
        "got: {results}"
    );
}

#[test]
fn impact_verdict_unsafe_when_real_callers_exist() {
    // payment_processor is called from caller_fn — strict_refs and
    // call_graph_callers both confirm. Verdict must be unsafe.
    let tmp = TempDir::new().unwrap();
    write_minimal_index_at(
        tmp.path(),
        &[(
            "src/lib.rs",
            "pub fn payment_processor() {}\n\
             \n\
             fn caller_fn() {\n\
             \x20\x20\x20\x20payment_processor();\n\
             }\n",
        )],
    );

    let out = run_impact_json(tmp.path(), "payment_processor");
    let results = &out["results"];
    assert_eq!(
        results["verdict"].as_str(),
        Some("unsafe"),
        "real caller must produce unsafe verdict, got: {results}"
    );
    let strict_count = results["channels"]["strict_refs"]["count"]
        .as_u64()
        .expect("strict_refs.count must be u64");
    assert!(
        strict_count >= 1,
        "strict_refs must report ≥1 hit (the call inside caller_fn), got: {results}"
    );
    let callers_count = results["channels"]["call_graph_callers"]["count"]
        .as_u64()
        .expect("call_graph_callers.count must be u64");
    assert!(
        callers_count >= 1,
        "call_graph_callers must report ≥1 hit, got: {results}"
    );
    let explanation = results["verdict_explanation"]
        .as_str()
        .expect("verdict_explanation must be a string");
    assert!(
        explanation.contains("strict_refs") || explanation.contains("call_graph_callers"),
        "explanation must cite which channel confirmed, got: {explanation}"
    );
}

#[test]
fn impact_verdict_uncertain_when_only_text_mentions_in_docs() {
    // payment_processor is defined but NEVER called in code — only
    // mentioned in CHANGELOG.md. The def-site filter strips the
    // declaration row from fst/grep, leaving only the CHANGELOG
    // mention. Strict + callers are both 0 → uncertain (text-only).
    let tmp = TempDir::new().unwrap();
    write_minimal_index_at(
        tmp.path(),
        &[
            ("src/lib.rs", "pub fn payment_processor() {}\n"),
            (
                "CHANGELOG.md",
                "## Notes\n\nMention of payment_processor here.\n",
            ),
        ],
    );

    let out = run_impact_json(tmp.path(), "payment_processor");
    let results = &out["results"];
    assert_eq!(
        results["verdict"].as_str(),
        Some("uncertain"),
        "text-only mention must produce uncertain verdict, got: {results}"
    );
    // Strict reports 0 (binder excludes def site; no other call sites).
    assert_eq!(
        results["channels"]["strict_refs"]["count"].as_u64(),
        Some(0),
        "got: {results}"
    );
    // Call graph reports 0 (no caller_fn).
    assert_eq!(
        results["channels"]["call_graph_callers"]["count"].as_u64(),
        Some(0),
        "got: {results}"
    );
    // grep_word_boundary OR fst_refs catch the CHANGELOG mention.
    let grep = results["channels"]["grep_word_boundary"]["count"]
        .as_u64()
        .expect("grep_word_boundary.count must be u64");
    let fst = results["channels"]["fst_refs"]["count"]
        .as_u64()
        .expect("fst_refs.count must be u64");
    assert!(
        grep + fst >= 1,
        "at least one text channel must catch the CHANGELOG mention, got: {results}"
    );
}

#[test]
fn impact_envelope_carries_full_contracted_shape() {
    // Pin the JSON envelope shape so MCP clients can rely on the
    // structure: protocol_version, capabilities, _meta, and
    // results = { symbol, verdict, verdict_explanation, channels:
    // { strict_refs, fst_refs, grep_word_boundary, call_graph_callers } }.
    let tmp = TempDir::new().unwrap();
    write_minimal_index_at(
        tmp.path(),
        &[("src/lib.rs", "pub fn used() {}\nfn caller() { used(); }\n")],
    );

    let out = run_impact_json(tmp.path(), "used");

    assert_eq!(out["protocol_version"].as_str(), Some("v1"));
    assert!(out["capabilities"].is_object());
    let results = &out["results"];
    assert!(results["symbol"].is_string());
    assert!(results["verdict"].is_string());
    assert!(results["verdict_explanation"].is_string());
    for ch in [
        "strict_refs",
        "fst_refs",
        "grep_word_boundary",
        "call_graph_callers",
    ] {
        let block = &results["channels"][ch];
        assert!(
            block.is_object(),
            "channels.{ch} must be an object, got: {results}"
        );
        assert!(
            block["available"].is_boolean(),
            "channels.{ch}.available must be a boolean, got: {block}"
        );
        assert!(
            block["count"].is_number(),
            "channels.{ch}.count must be a number, got: {block}"
        );
        assert!(
            block["sample"].is_array(),
            "channels.{ch}.sample must be an array, got: {block}"
        );
    }
}
