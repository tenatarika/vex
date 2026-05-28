//! C1 regression: tracing output must go to stderr, never to stdout.
//!
//! The MCP server parses the CLI's stdout as a JSON envelope; any
//! `tracing::warn!`/`debug!` written to stdout would prepend bytes to
//! the JSON and break `serde_json::from_str` for every frame. We
//! verify this by running `vex search` with `RUST_LOG=debug` and
//! asserting that stdout still parses as JSON.

use assert_cmd::Command;
use tempfile::TempDir;

fn write_project(dir: &std::path::Path, vex_toml: &str, source_name: &str, source: &str) {
    std::fs::write(dir.join(".vex.toml"), vex_toml).unwrap();
    std::fs::write(dir.join(source_name), source).unwrap();
}

#[test]
fn debug_tracing_does_not_corrupt_stdout_json() {
    let tmp = TempDir::new().unwrap();
    write_project(
        tmp.path(),
        "auto_update = true\nlocal_cache = true\n",
        "lib.rs",
        "fn known_symbol() {}\n",
    );

    let assert = Command::cargo_bin("vex")
        .unwrap()
        .current_dir(tmp.path())
        // Cache-isolation belt-and-braces: even though local_cache = true
        // is set, force the env override too so test isolation never
        // depends on .vex.toml parsing succeeding.
        .env("VEX_CACHE_DIR", tmp.path().join(".vex-test-cache"))
        // Crank tracing all the way up. With C1 fixed, every log line
        // must route through stderr.
        .env("RUST_LOG", "debug")
        .args(["search", "known_symbol", "--format", "json"])
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Stdout must parse as JSON. If tracing leaked to stdout this fails
    // with a parse error pointing at the log prefix on line 1.
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "stdout did not parse as JSON (C1 regression — tracing leaking to stdout?). \
             Error: {e}\nstdout: {stdout}\nstderr: {stderr}"
        )
    });

    // Sanity: it really is the envelope, not some other JSON we got
    // lucky parsing.
    assert!(
        parsed.get("protocol_version").is_some(),
        "expected response envelope in stdout, got: {parsed}"
    );

    // Belt: stderr should contain *some* tracing output at RUST_LOG=debug.
    // We don't pin the exact line because the bootstrap path varies; the
    // bootstrap banner alone is enough to prove tracing is wired up.
    assert!(
        !stderr.is_empty(),
        "RUST_LOG=debug should have produced some stderr output"
    );
}
