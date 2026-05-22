//! Phase 11.4 — fixture-based regression suite for `vex pattern`.
//!
//! Each fixture under `tests/pattern_fixtures/<name>/` carries:
//!   - `input.<ext>` — source code to scan
//!   - `spec.toml`   — pattern + lang + expected matches metadata
//!
//! The harness iterates every fixture, runs `vex pattern` with the
//! spec, and asserts each `expected.line` appears in the JSON output
//! with the expected captures.
//!
//! ## RED today
//!
//! `baseline_*` fixtures use today-syntax (`$NAME`, `$$$`) and MUST
//! PASS to validate the harness itself. Every other fixture exercises
//! a scope-B feature (`$$$BODY`, `$$ARGS`, `&&`, `||`) that hasn't
//! shipped — these tests fail until the corresponding increment lands.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde::Deserialize;
use tempfile::TempDir;

#[derive(Deserialize)]
struct FixtureSpec {
    lang: String,
    pattern: String,
    #[serde(default)]
    #[allow(dead_code)] // diagnostic metadata, surfaced via panic messages
    exercises: String,
    #[serde(default)]
    expected: Vec<ExpectedMatch>,
}

#[derive(Deserialize)]
struct ExpectedMatch {
    line: usize,
    #[serde(default)]
    captures: HashMap<String, String>,
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("pattern_fixtures")
}

fn read_spec(dir: &Path) -> FixtureSpec {
    let contents = std::fs::read_to_string(dir.join("spec.toml"))
        .unwrap_or_else(|e| panic!("read spec.toml in {dir:?}: {e}"));
    toml::from_str(&contents).unwrap_or_else(|e| panic!("parse spec.toml in {dir:?}: {e}"))
}

fn input_extension(lang: &str) -> &'static str {
    match lang {
        "rust" => "rs",
        "typescript" | "ts" => "ts",
        "python" | "py" => "py",
        "go" => "go",
        "cpp" | "c++" => "cpp",
        "csharp" | "cs" => "cs",
        "sql" => "sql",
        "markdown" | "md" => "md",
        "java" => "java",
        other => panic!("unsupported fixture lang: {other}"),
    }
}

fn run_pattern(dir: &Path, spec: &FixtureSpec) -> serde_json::Value {
    let input = dir.join(format!("input.{}", input_extension(&spec.lang)));
    assert!(
        input.exists(),
        "fixture input missing: {input:?} (lang={})",
        spec.lang
    );
    let cache = TempDir::new().unwrap();
    let assert = Command::cargo_bin("vex")
        .unwrap()
        .args([
            "pattern",
            &spec.pattern,
            "--lang",
            &spec.lang,
            "--path",
            dir.to_str().unwrap(),
            "--format",
            "json",
        ])
        .env("VEX_CACHE_DIR", cache.path())
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "fixture {:?}: pattern must emit valid JSON: {e}\nstdout:\n{stdout}",
            dir.file_name()
        )
    })
}

fn capture_value<'a>(captures_json: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    captures_json
        .as_array()?
        .iter()
        .find_map(|entry| entry.get(key).and_then(|v| v.as_str()))
}

fn run_fixture(name: &str) {
    let dir = fixture_root().join(name);
    let spec = read_spec(&dir);
    let json = run_pattern(&dir, &spec);
    let matches = json
        .as_array()
        .unwrap_or_else(|| panic!("fixture {name}: expected JSON array, got {json}"));

    let got_lines: Vec<usize> = matches
        .iter()
        .filter_map(|m| m["line"].as_u64().map(|l| l as usize))
        .collect();

    for exp in &spec.expected {
        let match_obj = matches
            .iter()
            .find(|m| m["line"].as_u64() == Some(exp.line as u64))
            .unwrap_or_else(|| {
                panic!(
                    "fixture {name} ({}): expected match on line {} not found.\n\
                     Got lines: {got_lines:?}\nFull JSON: {json}",
                    spec.exercises, exp.line
                )
            });

        for (key, expected_value) in &exp.captures {
            let captures_json = match_obj
                .get("captures")
                .cloned()
                .unwrap_or(serde_json::Value::Array(Vec::new()));
            let got = capture_value(&captures_json, key);
            assert_eq!(
                got,
                Some(expected_value.as_str()),
                "fixture {name} ({}): expected ${key} = {expected_value:?} on line {}, got {got:?}.\n\
                 Captures JSON: {captures_json}",
                spec.exercises,
                exp.line,
            );
        }
    }
}

// One #[test] per fixture so failures stay isolated and visible in
// `cargo test` output. Add a new function when adding a new fixture.

#[test]
fn baseline_rust_simple_fn() {
    run_fixture("baseline_rust");
}

#[test]
fn rust_multiline_result_body() {
    run_fixture("rust_multiline_result");
}

#[test]
fn rust_struct_and_impl_composition() {
    run_fixture("rust_struct_and_impl");
}

#[test]
fn typescript_multiline_class_constructor() {
    run_fixture("typescript_multiline_class");
}

#[test]
fn typescript_interface_or_class_composition() {
    run_fixture("typescript_interface_or_class");
}

#[test]
fn python_multiline_method_with_body() {
    run_fixture("python_multiline_method");
}

#[test]
fn baseline_go_simple_func() {
    run_fixture("baseline_go");
}

#[test]
fn go_method_decl_matches_receiver_shape() {
    run_fixture("go_method_receiver");
}

#[test]
fn go_struct_with_multiline_body() {
    run_fixture("go_struct_body");
}

#[test]
fn baseline_cpp_simple_function() {
    run_fixture("baseline_cpp");
}

#[test]
fn cpp_class_with_member_body() {
    run_fixture("cpp_class_body");
}

#[test]
fn cpp_template_function_signature() {
    run_fixture("cpp_template_function");
}

#[test]
fn baseline_csharp_simple_method() {
    run_fixture("baseline_csharp");
}

#[test]
fn csharp_class_with_property_body() {
    run_fixture("csharp_class_body");
}

#[test]
fn csharp_record_signature() {
    run_fixture("csharp_record");
}

#[test]
fn baseline_sql_create_table() {
    run_fixture("baseline_sql");
}

#[test]
fn sql_create_function_signature() {
    run_fixture("sql_create_function");
}

#[test]
fn baseline_markdown_atx_heading() {
    run_fixture("baseline_markdown");
}

#[test]
fn markdown_fenced_code_block_language() {
    run_fixture("markdown_fenced_code");
}

#[test]
fn baseline_java_simple_method() {
    run_fixture("baseline_java");
}

#[test]
fn java_class_with_method_body() {
    run_fixture("java_class_body");
}

#[test]
fn java_record_signature() {
    run_fixture("java_record");
}
