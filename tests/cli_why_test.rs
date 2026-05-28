//! CLI integration test for `vex search --why` (11.10).
//!
//! `--why` appends a JSON trace to stderr so structured-output
//! consumers piping stdout into `jq` keep working. The trace records
//! per-channel hit counts plus a `filter_applied` snapshot so a user
//! can quickly answer "what did vex actually search?".

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write_tiny_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("api.rs"),
        "pub fn payment_processor() {}\n",
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn why_flag_emits_json_trace_on_stderr() {
    let tmp = TempDir::new().unwrap();
    write_tiny_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args([
            "search",
            "payment_processor",
            "--why",
            "--filter",
            "src/",
            "--include",
            "src/**",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    // stdout must remain a clean JSON array so consumers can `| jq`.
    let _parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout is valid JSON array");

    // Locate the trace line. v1.10.1 tags it `VEX_WHY: { … }` (review
    // S8.1); legacy "first `{`-line" shape is honoured as a fallback so
    // this test stays useful if the prefix ever has to ship behind a
    // capability flag.
    const PREFIX: &str = "VEX_WHY:";
    let trace: serde_json::Value = if let Some(rest) = stderr
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(PREFIX))
    {
        serde_json::from_str(rest.trim())
            .unwrap_or_else(|e| panic!("VEX_WHY trace did not parse as JSON ({e}): {stderr}"))
    } else {
        let trace_line = stderr
            .lines()
            .find(|l| {
                l.trim_start().starts_with('{')
                    && serde_json::from_str::<serde_json::Value>(l).is_ok()
            })
            .unwrap_or_else(|| panic!("expected JSON trace line on stderr, got: {stderr}"));
        serde_json::from_str(trace_line).unwrap()
    };

    assert_eq!(
        trace["normalized_query"].as_str().unwrap(),
        "payment_processor"
    );
    let channels = trace["channels"].as_array().expect("channels array");
    let names: Vec<&str> = channels
        .iter()
        .map(|c| c["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["fst", "bm25", "semantic"]);
    // Hit counts are integers; not asserting exact values because they
    // depend on index defaults.
    for c in channels {
        assert!(c["hits"].is_u64() || c["hits"].is_i64());
    }

    let filter = &trace["filter_applied"];
    assert_eq!(filter["filter"].as_str(), Some("src/"));
    assert_eq!(
        filter["include"].as_array().unwrap()[0].as_str(),
        Some("src/**")
    );
}

#[test]
fn no_why_leaves_stderr_quiet() {
    let tmp = TempDir::new().unwrap();
    write_tiny_project(tmp.path());

    let assert = vex_in(tmp.path())
        .args(["search", "payment_processor", "--format", "json"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    // No `VEX_WHY:`-tagged line should appear without --why. Warnings
    // are allowed, but they don't carry the tag (review S8.1, v1.10.1).
    const PREFIX: &str = "VEX_WHY:";
    let tagged: Vec<&str> = stderr
        .lines()
        .filter(|l| l.trim_start().starts_with(PREFIX))
        .collect();
    assert!(
        tagged.is_empty(),
        "expected no VEX_WHY trace without --why, got: {tagged:?}"
    );
    // Belt-and-suspenders: no bare JSON object line either.
    let trace_lines: Vec<&str> = stderr
        .lines()
        .filter(|l| {
            l.trim_start().starts_with('{') && serde_json::from_str::<serde_json::Value>(l).is_ok()
        })
        .collect();
    assert!(
        trace_lines.is_empty(),
        "expected no JSON trace without --why, got: {trace_lines:?}"
    );
}
