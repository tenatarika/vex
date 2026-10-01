//! CLI-level coverage for the v9 CSR format bump (`docs/V9-FORMAT.md`
//! §13 R20): "One CLI test covers the upgrade itself: v8 file → `vex
//! update` → v9."
//!
//! Drives the real `vex` binary via `assert_cmd` (separate process —
//! see `tests/cli_bootstrap_test.rs` for the established pattern) so
//! this exercises the production `update_can_skip` version gate
//! (`docs/V9-FORMAT.md` §13 R5) exactly as a user would hit it: an
//! untouched v8 index, with no file changes at all, must still convert
//! to v9 on the next `vex update` rather than skip forever.
//!
//! `local_cache = true` keeps the index at the fixed `<root>/.vex_cache/`
//! convention (`tests/cli_incremental_hnsw_test.rs` established this
//! pattern) — known statically, so no cache-resolver call is needed to
//! locate the file for the in-process
//! `legacy_v8::downgrade_v9_file_to_v8` step.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;
use vex::store::legacy_v8::downgrade_v9_file_to_v8;
use vex::store::reader::IndexReader;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    // `local_cache = true` in `.vex.toml` decides the cache location;
    // make sure an ambient VEX_CACHE_DIR in the dev/CI environment can't
    // override it.
    cmd.env_remove("VEX_CACHE_DIR");
    cmd
}

#[test]
fn v8_index_upgrades_to_v9_on_vex_update() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
    let index_path = root.join(".vex_cache").join("index.vex");
    std::fs::write(
        root.join("a.rs"),
        "pub fn helper_fn() -> i32 {\n    42\n}\npub fn caller_fn() -> i32 {\n    helper_fn()\n}\n",
    )
    .unwrap();

    // 1. Build a genuine v9 index via the real binary.
    vex_in(&root).args(["index"]).assert().success();

    let reader = IndexReader::open(&index_path).expect("open freshly built index");
    assert_eq!(
        reader.header().version,
        9,
        "today's `vex index` must produce a v9 file"
    );
    drop(reader);

    // 2. Downgrade it to v8 in place (in-process — pure byte rewrite,
    //    no cache resolution involved).
    downgrade_v9_file_to_v8(&index_path).expect("downgrade to v8");
    let reader = IndexReader::open(&index_path).expect("open downgraded v8 index");
    assert_eq!(reader.header().version, 8);
    drop(reader);

    // 3. `vex update` with NO file changes at all — the manifest's
    //    options already "cover" the request, so pre-R5 this would have
    //    skipped and left the file at v8 forever. R5 forces a rebuild
    //    anyway because the on-disk version (8) is older than this
    //    build's VERSION (9).
    vex_in(&root).args(["update"]).assert().success();

    let reader = IndexReader::open(&index_path).expect("open updated index");
    assert_eq!(
        reader.header().version,
        9,
        "`vex update` must converge a stale v8 index to v9 even with zero file changes"
    );

    // Golden behaviour survives the round trip: caller_fn still calls
    // helper_fn after the v8→v9 convergence.
    let matches = vex::store::call_graph::find_callees_fast(&reader, "caller_fn", 50);
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].name, "helper_fn");
}
