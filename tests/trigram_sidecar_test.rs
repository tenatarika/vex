//! grep trigram skip-index — P2 pipeline integration (STORAGE-RESEARCH §2).
//!
//! Drives `vex::index::pipeline::{run,update}` against a real git repo and
//! asserts the `index.trigram` sidecar is written with one correct record
//! per code file. The load-bearing cases:
//!
//!   * a fresh `run` records a bloom + `(len, mtime)` for every code file;
//!   * the bloom survives a WARM blob-cache re-index (the common case —
//!     Phase 14.7's cache skips the byte read on a hit, so the bloom must
//!     ride inside the cache entry or the sidecar would come back empty);
//!   * `update` refreshes the changed file's record and carries the
//!     unchanged file's record forward verbatim.
//!
//! P3 wires `grep::search` to consume the sidecar; P2 only proves it is
//! produced correctly, so these tests inspect the sidecar directly via
//! `store::trigram::load` + `grep::trigram::{required_trigrams,TrigramBloom}`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use tempfile::TempDir;
use vex::grep::trigram::{required_trigrams, TrigramBloom};
use vex::index::pipeline;
use vex::store::trigram::{self, TrigramRecord};

static CACHE_TMP: OnceLock<TempDir> = OnceLock::new();

/// Install a process-global cache override so blob-cache writes land in a
/// tempdir (mirrors `parse_cache_pipeline_test`). Shared across this
/// binary's tests; per-test projects keep blob SHAs distinct.
fn shared_cache_root() {
    CACHE_TMP.get_or_init(|| {
        let tmp = TempDir::new().expect("create cache TempDir");
        let root = tmp.path().canonicalize().expect("canonicalize cache dir");
        vex::util::config::set_cache_override(root, false);
        tmp
    });
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// `git init` + identity + commit `files`. Returns the canonical root.
fn init_git_repo(repo_root: &Path, files: &[(&str, &str)]) -> PathBuf {
    std::fs::create_dir_all(repo_root).unwrap();
    git(repo_root, &["init", "-q"]);
    git(
        repo_root,
        &["config", "--local", "user.email", "ci@example.test"],
    );
    git(repo_root, &["config", "--local", "user.name", "Vex CI"]);
    git(
        repo_root,
        &["config", "--local", "init.defaultBranch", "main"],
    );
    write_files(repo_root, files);
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-q", "-m", "initial"]);
    repo_root.canonicalize().unwrap()
}

fn write_files(repo_root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let path = repo_root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }
}

/// Load the sidecar and index it by rel-path for convenient assertions.
fn load_sidecar(root: &Path) -> HashMap<String, TrigramRecord> {
    let path = vex::util::config::trigram_path(root);
    assert!(path.exists(), "index.trigram missing at {}", path.display());
    trigram::load(&path)
        .expect("load trigram sidecar")
        .into_iter()
        .map(|r| (r.rel_path.clone(), r))
        .collect()
}

/// Does this record's bloom admit `literal` as a possible match?
fn bloom_admits(rec: &TrigramRecord, literal: &str) -> bool {
    let trigrams = required_trigrams(literal)
        .unwrap_or_else(|| panic!("{literal:?} should yield required trigrams"));
    TrigramBloom::from_raw(rec.bloom).might_contain_all(&trigrams)
}

fn run(root: &Path) {
    pipeline::run(root, pipeline::IndexOptions::default(), "minilm-l6-v2", &[])
        .expect("pipeline::run");
}

// ── Test 1: a fresh index records one bloom + (len, mtime) per code file ─────

#[test]
fn index_writes_one_trigram_record_per_code_file() {
    shared_cache_root();
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("proj1");
    let root = init_git_repo(
        &project,
        &[
            ("src/a.rs", "pub fn f() { let alphamarker = 1; }\n"),
            ("src/b.rs", "pub fn g() { let betamarker = 2; }\n"),
        ],
    );

    run(&root);
    let recs = load_sidecar(&root);

    assert_eq!(recs.len(), 2, "expected one record per code file: {recs:?}");

    let a = &recs["src/a.rs"];
    let b = &recs["src/b.rs"];

    // Blooms admit a literal that IS in the file...
    assert!(
        bloom_admits(a, "alphamarker"),
        "a.rs bloom must admit its own literal"
    );
    assert!(
        bloom_admits(b, "betamarker"),
        "b.rs bloom must admit its own literal"
    );
    // ...and reject one that is not (probabilistic, but a distinctive
    // long literal in a tiny file has a vanishing false-positive rate).
    assert!(
        !bloom_admits(a, "betamarker"),
        "a.rs bloom must NOT admit a literal unique to b.rs"
    );

    // Staleness guard: len matches the on-disk file; mtime is populated.
    let a_len = std::fs::metadata(root.join("src/a.rs")).unwrap().len();
    assert_eq!(a.len, a_len, "recorded len must match file byte length");
    assert!(
        a.mtime_secs != 0 || a.mtime_nanos != 0,
        "recorded mtime must be populated"
    );
}

// ── Test 2: bloom survives a WARM blob-cache re-index (the critical path) ────

/// Re-running a full `vex index` on unchanged, committed content hits the
/// blob cache for every file (Phase 14.7 skips the byte read). If blooms
/// were built only on the read path, the second run's sidecar would be
/// empty. This proves the bloom rides inside the cache entry and is
/// restored on a hit.
#[test]
fn trigram_bloom_survives_warm_blob_cache_reindex() {
    shared_cache_root();
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("proj2");
    let root = init_git_repo(
        &project,
        &[("src/warm.rs", "pub fn cached() { let warmmarker = 7; }\n")],
    );

    // First run populates the blob cache with v4 (bloom-carrying) entries.
    run(&root);
    let first = load_sidecar(&root);
    assert!(bloom_admits(&first["src/warm.rs"], "warmmarker"));

    // Second full run: every file is a blob-cache hit (no byte read). The
    // sidecar must STILL be complete with a valid bloom.
    run(&root);
    let second = load_sidecar(&root);
    assert_eq!(second.len(), 1, "warm re-index dropped the sidecar record");
    assert!(
        bloom_admits(&second["src/warm.rs"], "warmmarker"),
        "bloom was lost on the blob-cache-hit path — it must ride in the entry"
    );
}

// ── Test 3: update refreshes changed, carries unchanged forward verbatim ─────

#[test]
fn update_refreshes_changed_and_carries_unchanged_forward() {
    shared_cache_root();
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("proj3");
    let root = init_git_repo(
        &project,
        &[
            ("src/keep.rs", "pub fn k() { let keepmarker = 1; }\n"),
            ("src/edit.rs", "pub fn e() { let oldmarker = 2; }\n"),
        ],
    );

    run(&root);
    let before = load_sidecar(&root);
    let keep_before = before["src/keep.rs"].clone();

    // Change edit.rs content (working-tree edit; content hash changes so
    // the manifest diff marks it changed). Leave keep.rs untouched.
    write_files(
        &root,
        &[("src/edit.rs", "pub fn e() { let newmarker = 3; }\n")],
    );

    pipeline::update(
        &root,
        pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .expect("pipeline::update");

    let after = load_sidecar(&root);
    assert_eq!(after.len(), 2, "both files must remain in the sidecar");

    // Changed file: a fresh bloom that admits the NEW literal.
    let edit_after = &after["src/edit.rs"];
    assert!(
        bloom_admits(edit_after, "newmarker"),
        "changed file's bloom must be rebuilt from new content"
    );

    // Unchanged file: record carried forward byte-for-byte (same bloom,
    // same len/mtime — must-fix #2: verbatim carry-forward, never a
    // re-derived or stale bloom).
    let keep_after = &after["src/keep.rs"];
    assert_eq!(
        *keep_after, keep_before,
        "unchanged file's record must be carried forward verbatim"
    );
    assert!(bloom_admits(keep_after, "keepmarker"));
}
