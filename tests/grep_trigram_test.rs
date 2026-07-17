//! grep trigram skip-index — P3 (STORAGE-RESEARCH §2).
//!
//! Proves `vex grep` consumes the `index.trigram` sidecar correctly:
//!
//!   * results are identical to a full walk (the skip-index only trims
//!     I/O — it must never drop a real match);
//!   * the staleness guard reads a file edited after indexing, even when
//!     the stale bloom lacks the pattern (no false negatives);
//!   * a skip actually fires when a record is fresh and its bloom lacks
//!     the literal — observed white-box by fooling the `(len, mtime)`
//!     guard with `filetime` so we can watch the skip drop a match that a
//!     full read would have found.
//!
//! No git repo needed: on a plain dir every file is "untracked", so the
//! blob cache is bypassed and `parse_files` builds each bloom on the read
//! path — the sidecar is still written in full.

use std::path::Path;
use std::sync::OnceLock;

use filetime::{set_file_times, FileTime};
use tempfile::TempDir;
use vex::grep;
use vex::index::pipeline;

static CACHE_TMP: OnceLock<TempDir> = OnceLock::new();

fn shared_cache_root() {
    CACHE_TMP.get_or_init(|| {
        let tmp = TempDir::new().expect("create cache TempDir");
        let root = tmp.path().canonicalize().expect("canonicalize cache dir");
        vex::util::config::set_cache_override(root, false);
        tmp
    });
}

/// Build an indexed project from `files` and return its canonical root.
/// Canonicalization matters: grep must read the sidecar from the same
/// cache subdir the index wrote it to (see cache-path writer/reader
/// symmetry).
fn indexed_project(name: &str, files: &[(&str, &str)]) -> (TempDir, std::path::PathBuf) {
    shared_cache_root();
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join(name);
    std::fs::create_dir_all(&project).unwrap();
    for (rel, content) in files {
        let path = project.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }
    let root = project.canonicalize().unwrap();
    pipeline::run(
        &root,
        pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .expect("pipeline::run");
    assert!(
        vex::util::config::trigram_path(&root).exists(),
        "index.trigram must exist after indexing"
    );
    (tmp, root)
}

fn grep(root: &Path, pattern: &str) -> Vec<grep::GrepMatch> {
    grep::search(root, pattern, None, 100, &[], false).unwrap()
}

// ── Correctness: skip-index active, results identical to a full walk ─────────

#[test]
fn grep_with_sidecar_returns_correct_matches() {
    let (_tmp, root) = indexed_project(
        "correct",
        &[
            ("src/a.rs", "pub fn f() { let alphaword = 1; }\n"),
            ("src/b.rs", "pub fn g() { let betaword = 2; }\n"),
        ],
    );

    // Literal present in exactly one file: found there, nowhere else. (b.rs
    // is bloom-skipped for this literal, but the result is the same as if it
    // had been read and not matched.)
    let hits = grep(&root, "alphaword");
    assert_eq!(hits.len(), 1, "expected one match for alphaword: {hits:?}");
    assert!(hits[0].path.contains("a.rs"));

    // Literal absent everywhere: no matches, no panic.
    assert!(
        grep(&root, "zzznotpresentzzz").is_empty(),
        "absent literal must yield no matches"
    );

    // The other file's literal still works (sidecar didn't corrupt lookups).
    let hits = grep(&root, "betaword");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].path.contains("b.rs"));
}

// ── Staleness guard: edit-then-grep WITHOUT reindex still finds the match ────

#[test]
fn grep_reads_file_edited_after_index() {
    let (_tmp, root) = indexed_project(
        "stale",
        &[("src/s.rs", "pub fn s() { let original = 1; }\n")],
    );

    // The stale bloom (from the original content) does not contain
    // "insertedword". Grepping for it before the edit correctly finds
    // nothing.
    assert!(grep(&root, "insertedword").is_empty());

    // Edit the file to introduce the literal, WITHOUT reindexing. The new
    // content is a different length, so the `(len, mtime)` guard trips and
    // the file must be read despite the stale bloom lacking the trigram.
    std::fs::write(
        root.join("src/s.rs"),
        "pub fn s() { let original = 1; let insertedword = 2; }\n",
    )
    .unwrap();

    let hits = grep(&root, "insertedword");
    assert_eq!(
        hits.len(),
        1,
        "staleness guard must read an edited file — a stale bloom must never \
         cause a false negative: {hits:?}"
    );
}

// ── White-box: a fresh record with a bloom-miss actually skips the read ──────

/// Overwrite an indexed file with same-length content that DOES contain a
/// literal the stale bloom lacks, then restore its mtime so the `(len,
/// mtime)` guard is satisfied. Now the only thing standing between grep and
/// a match is the skip decision — if the skip fires (as it must), grep
/// leaves the file unread and returns nothing. This deliberately induces
/// the false-negative the staleness guard normally prevents, purely to
/// observe that the skip path is taken.
#[test]
fn fresh_record_with_bloom_miss_is_skipped() {
    // 30 bytes; the bloom built from this has "presentword"'s trigrams but
    // not "laterword"'s (no `lat`/`ate`/`ter`/`erw`/`rwo`).
    let original = "fn a(){let x=\"presentword\";}\n";
    let (_tmp, root) = indexed_project("skip", &[("src/f.rs", original)]);

    let file = root.join("src/f.rs");
    let meta = std::fs::metadata(&file).unwrap();
    let atime = FileTime::from_last_access_time(&meta);
    let mtime = FileTime::from_last_modification_time(&meta);
    let indexed_len = meta.len();

    // Same-length replacement that contains "laterword" (which the stale
    // bloom lacks). Equal length keeps the len half of the guard satisfied.
    let replacement = "fn a(){let y=\"laterword_z\";}\n";
    assert_eq!(
        original.len(),
        replacement.len(),
        "test fixture must keep byte length identical"
    );
    std::fs::write(&file, replacement).unwrap();
    assert_eq!(std::fs::metadata(&file).unwrap().len(), indexed_len);

    // Restore mtime so the staleness guard is fully satisfied.
    set_file_times(&file, atime, mtime).unwrap();

    // "laterword" is in the file on disk, but the fresh-looking record's
    // stale bloom lacks it → the file is skipped and never read.
    let hits = grep(&root, "laterword");
    assert!(
        hits.is_empty(),
        "skip did not fire: a fresh record whose bloom lacks the literal must \
         leave the file unread (observed via the fooled staleness guard): {hits:?}"
    );

    // Sanity: a literal the stale bloom DOES contain still reads/matches...
    // actually "presentword" is no longer on disk, so grepping it now yields
    // nothing too — but for a DIFFERENT reason (not present). Instead prove
    // the skip is literal-specific: a short (<3 byte) pattern bypasses the
    // skip-index entirely (required_trigrams → None) and reads the file, so
    // it sees the new content.
    let hits = grep(&root, "fn");
    assert_eq!(
        hits.len(),
        1,
        "sub-trigram pattern must bypass the skip-index and read the file"
    );
}
