//! v1.15.0 B1.2 — integration coverage for the `index.bodytokens`
//! sidecar.
//!
//! Pins the contract that `vex index` writes the sidecar next to
//! `index.vex`, the file uses the `VEXT` magic, and `vex update` with
//! zero filesystem changes preserves the same body_tokens (proving
//! `parse_files::reconstruct_unchanged` reads the sidecar and feeds the
//! restored body_tokens back into the write path on the next save).
//! Sidecar presence is the prerequisite for B1.2 incremental HNSW
//! update — without it, reconstructed symbols produce body-less
//! `context_hash` values and the diff against the old `index.hashes`
//! sidecar treats every unchanged symbol as a remove+add.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env_remove("VEX_CACHE_DIR");
    cmd
}

fn make_indexed_project(dir: &Path) -> std::path::PathBuf {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        r#"pub fn payment_processor(amount: u64) -> u64 {
    let tax = amount / 10;
    amount + tax
}
pub fn billing_service(user: &str) -> String {
    format!("bill for {user}")
}
"#,
    )
    .unwrap();
    vex_in(dir).args(["index"]).assert().success();
    dir.join(".vex_cache")
}

#[test]
fn index_writes_body_tokens_sidecar_next_to_index() {
    let tmp = TempDir::new().unwrap();
    let cache = make_indexed_project(tmp.path());
    let sidecar = cache.join("index.bodytokens");
    assert!(
        sidecar.exists(),
        "expected body_tokens sidecar at {}, cache contents: {:?}",
        sidecar.display(),
        std::fs::read_dir(&cache)
            .map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
            .unwrap_or_default()
    );
    let bytes = std::fs::read(&sidecar).unwrap();
    // Header is magic (4) + version (4) + count (4) = 12 bytes minimum.
    // A 2-symbol fixture will have at least 12 + 2 × 4 = 20 bytes (each
    // record has at least the byte_len field).
    assert!(
        bytes.len() >= 20,
        "body_tokens file must contain at least header + 2 records, got {}",
        bytes.len()
    );
    assert_eq!(
        &bytes[0..4],
        b"VEXT",
        "body_tokens file must start with the VEXT magic"
    );
}

#[test]
fn vex_update_preserves_body_tokens_sidecar_when_no_changes() {
    // Round-trip the sidecar through an update cycle that has zero
    // filesystem changes. The reconstruct path must restore body_tokens
    // from disk, the write path must re-emit them, and the resulting
    // sidecar must be content-equal to the original. Pins the closure
    // of the v1.14.1 body-less hash drift: after this test, an
    // incremental HNSW path can rely on the sidecar staying stable
    // across no-op updates.
    let tmp = TempDir::new().unwrap();
    let cache = make_indexed_project(tmp.path());
    let sidecar = cache.join("index.bodytokens");
    let before = std::fs::read(&sidecar).expect("sidecar exists after initial index");

    vex_in(tmp.path()).args(["update"]).assert().success();

    let after = std::fs::read(&sidecar).expect("sidecar still exists after update");
    // Binary equality is a tight contract — it relies on the format
    // being fully deterministic (no timestamps, no salts, sym_idx order
    // preserved by the writer, body_tokens reconstructed bit-identically
    // from disk by `parse_files::reconstruct_unchanged`). A future
    // format addition that introduces non-determinism (e.g. a salt) MUST
    // update this assertion. Keeping it strict catches accidental drift.
    assert_eq!(
        before, after,
        "body_tokens sidecar must be content-equal across a no-change update",
    );
}

#[test]
fn vex_status_surfaces_body_tokens_marker_in_json() {
    let tmp = TempDir::new().unwrap();
    let _cache = make_indexed_project(tmp.path());
    let out = vex_in(tmp.path())
        .args(["status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --format json emits valid JSON");
    // Some output shapes wrap the payload in an envelope. Search the
    // tree for the literal key so we don't break on envelope changes.
    let surfaced = find_bool_field(&parsed, "body_tokens_persisted");
    assert_eq!(
        surfaced,
        Some(true),
        "expected body_tokens_persisted=true in JSON, got {parsed:?}"
    );
}

#[test]
fn vex_status_surfaces_body_tokens_marker_in_text() {
    let tmp = TempDir::new().unwrap();
    let _cache = make_indexed_project(tmp.path());
    let out = vex_in(tmp.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    assert!(
        stdout.contains("Body tokens: yes"),
        "expected 'Body tokens: yes' in status text, got:\n{stdout}"
    );
}

/// Walk a JSON value depth-first looking for a `bool` field at the
/// given key. Returns the first hit. Lets the test ignore envelope
/// wrappers (the project's `print_envelope` may add `_meta`,
/// `capabilities`, etc. around the payload).
fn find_bool_field(v: &serde_json::Value, key: &str) -> Option<bool> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Bool(b)) = map.get(key) {
                return Some(*b);
            }
            for (_, sub) in map {
                if let Some(found) = find_bool_field(sub, key) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(|sub| find_bool_field(sub, key)),
        _ => None,
    }
}
