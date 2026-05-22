//! Phase 11.4 Inc 4 — verify the pipeline persists pattern skeletons
//! when `--no-pattern-index` is not set, and writes an empty section
//! (still a v6 index) when the opt-out flag is passed.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    // No `VEX_CACHE_DIR`: the env var wins over `.vex.toml`'s `local_cache`
    // and would route the index through a hashed sub-directory, which
    // makes the file path harder to assert against.
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd
}

fn write_rust_project(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        dir.join("lib.rs"),
        "fn alpha() {}\nstruct Beta;\nimpl Beta { fn gamma(&self) {} }\n",
    )
    .unwrap();
}

fn open_index(dir: &Path) -> vex::store::reader::IndexReader {
    // `local_cache = true` puts the index at `<project>/.vex_cache/index.vex`.
    let path = dir.join(".vex_cache").join("index.vex");
    assert!(path.exists(), "index file missing at {path:?}");
    vex::store::reader::IndexReader::open(&path).expect("open v6 index")
}

#[test]
fn default_index_persists_pattern_skeletons() {
    let tmp = TempDir::new().unwrap();
    write_rust_project(tmp.path());
    vex_in(tmp.path()).args(["index"]).assert().success();

    let reader = open_index(tmp.path());
    assert!(
        reader.pattern_skeleton_header().is_some(),
        "v6 indexes must carry the PatternSkeletonHeader gate"
    );
    let skel = reader
        .pattern_skeleton_reader()
        .expect("pattern_skeleton_reader present on v6 index");
    assert!(
        !skel.is_empty(),
        "default `vex index` must persist skeletons for Rust T1 files"
    );
}

#[test]
fn no_pattern_index_flag_writes_empty_section() {
    let tmp = TempDir::new().unwrap();
    write_rust_project(tmp.path());
    vex_in(tmp.path())
        .args(["index", "--no-pattern-index"])
        .assert()
        .success();

    let reader = open_index(tmp.path());
    let skel = reader
        .pattern_skeleton_reader()
        .expect("section header is still present on opt-out, just empty");
    assert!(
        skel.is_empty(),
        "--no-pattern-index must leave the persisted section empty"
    );
}

#[test]
fn opt_out_is_sticky_across_update() {
    let tmp = TempDir::new().unwrap();
    write_rust_project(tmp.path());

    // First build with opt-out — manifest records pattern_index = Some(false).
    vex_in(tmp.path())
        .args(["index", "--no-pattern-index"])
        .assert()
        .success();

    // Mutate the source so `update` has changed files to re-parse.
    std::fs::write(
        tmp.path().join("lib.rs"),
        "fn alpha() {}\nstruct Beta;\nimpl Beta { fn gamma(&self) {} }\nfn delta() {}\n",
    )
    .unwrap();

    vex_in(tmp.path()).args(["update"]).assert().success();

    let reader = open_index(tmp.path());
    let skel = reader
        .pattern_skeleton_reader()
        .expect("section header present");
    assert!(
        skel.is_empty(),
        "`vex update` must honour the previous build's --no-pattern-index opt-out"
    );
}
