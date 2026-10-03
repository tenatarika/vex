//! `vex show` exit-code contract (docs/EXIT-CODES.md): `1` when no symbol
//! resolves, `0` when at least one does, identically in every output
//! format. Text/compact used to exit `0` on a miss because the
//! "No symbol found" line counted as printed output.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn project() -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "pub fn present_symbol_fn() {}\n",
    )
    .unwrap();
    vex_in(tmp.path()).arg("index").assert().success();
    tmp
}

/// `(stdout, exit code)` of `vex show <symbols> --format <format>`.
fn show(dir: &Path, symbols: &[&str], format: &str) -> (String, i32) {
    let out = vex_in(dir)
        .arg("show")
        .args(symbols)
        .args(["--no-stale-check", "--format", format])
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap(),
    )
}

#[test]
fn missing_symbol_exits_one_in_text() {
    let tmp = project();
    let (out, code) = show(tmp.path(), &["absent_symbol_fn"], "text");
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("No symbol found: \"absent_symbol_fn\""),
        "{out}"
    );
}

#[test]
fn missing_symbol_exits_one_in_compact() {
    let tmp = project();
    let (out, code) = show(tmp.path(), &["absent_symbol_fn"], "compact");
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("No symbol found: \"absent_symbol_fn\""),
        "{out}"
    );
}

#[test]
fn missing_symbol_exits_one_in_json() {
    let tmp = project();
    let (out, code) = show(tmp.path(), &["absent_symbol_fn"], "json");
    assert_eq!(code, 1, "{out}");
    let env: serde_json::Value = serde_json::from_str(&out).expect("JSON envelope");
    assert_eq!(env["results"].as_array().map(Vec::len), Some(0), "{out}");
}

#[test]
fn found_symbol_exits_zero_in_every_format() {
    let tmp = project();
    for format in ["text", "compact", "json"] {
        let (out, code) = show(tmp.path(), &["present_symbol_fn"], format);
        assert_eq!(code, 0, "{format}: {out}");
        assert!(out.contains("present_symbol_fn"), "{format}: {out}");
    }
}

#[test]
fn partial_hit_exits_zero_and_still_reports_the_miss() {
    let tmp = project();
    for format in ["text", "compact"] {
        let (out, code) = show(
            tmp.path(),
            &["present_symbol_fn", "absent_symbol_fn"],
            format,
        );
        assert_eq!(code, 0, "{format}: {out}");
        assert!(
            out.contains("No symbol found: \"absent_symbol_fn\""),
            "{format}: {out}"
        );
    }
}
