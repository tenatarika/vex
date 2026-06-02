//! Phase 14.7 Step 4b — integration tests for blob-SHA cache pipeline wiring.
//!
//! These tests drive `vex::index::pipeline::run` against a real git repo and
//! observe that the on-disk blob cache is created on the first run, reused
//! (no rewrite) on a second run, and bypassed entirely for untracked files.
//!
//! ## Cache override strategy
//!
//! `set_cache_override` is process-global (OnceLock). All tests in this
//! binary share the same cache root, installed lazily via [`shared_cache_root`].
//! Each test still uses its own [`TempDir`] project so blob SHAs differ
//! between tests; the shared cache root is just a sink that catches all
//! cache writes and lets the test inspect the `<root>/blobs/` directory.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use tempfile::TempDir;
use vex::index::pipeline;

/// Process-global cache root kept alive for the lifetime of the test binary.
/// Installed exactly once via [`shared_cache_root`]; subsequent calls hand
/// back the same path so every test sees a consistent cache directory.
static CACHE_TMP: OnceLock<TempDir> = OnceLock::new();

/// Install a one-shot `set_cache_override` and return the cache root path.
///
/// The override is process-global because `config::CACHE_OVERRIDE` is a
/// `OnceLock`. Calling this from multiple tests just hands back the same
/// resolved cache root.
fn shared_cache_root() -> &'static Path {
    let tmp = CACHE_TMP.get_or_init(|| {
        let tmp = TempDir::new().expect("create cache TempDir");
        let root = tmp
            .path()
            .canonicalize()
            .expect("canonicalize cache TempDir");
        // `set_cache_override` is OnceLock — only the first caller wins.
        // That's fine; we want exactly one installation per test binary.
        vex::util::config::set_cache_override(root, false);
        tmp
    });
    tmp.path()
}

/// Initialize a git repo, configure user.email/name (CI sandboxes often
/// have neither set), and commit `files` into it. Returns the canonical
/// repository root.
fn init_git_repo(repo_root: &Path, files: &[(&str, &str)]) -> PathBuf {
    std::fs::create_dir_all(repo_root).unwrap();

    // `git init` — quiet because CI logs are noisy enough already.
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo_root)
        .status()
        .expect("git init");
    assert!(status.success(), "git init failed");

    // user.email/user.name are required for `git commit`. Use --local so
    // we never touch the developer's global git config.
    for (k, v) in [
        ("user.email", "ci@example.test"),
        ("user.name", "Vex CI"),
        // Default branch name — silences the per-repo hint, irrelevant
        // to the test but keeps the output tidy.
        ("init.defaultBranch", "main"),
    ] {
        let status = Command::new("git")
            .args(["config", "--local", k, v])
            .current_dir(repo_root)
            .status()
            .expect("git config");
        assert!(status.success(), "git config {k} failed");
    }

    for (rel, content) in files {
        let path = repo_root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }

    let status = Command::new("git")
        .args(["add", "-A"])
        .current_dir(repo_root)
        .status()
        .expect("git add");
    assert!(status.success(), "git add failed");

    let status = Command::new("git")
        .args(["commit", "-q", "-m", "initial"])
        .current_dir(repo_root)
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit failed");

    repo_root.canonicalize().unwrap()
}

/// Run `git hash-object <abs_path>` and return the blob SHA so the test
/// can find the exact `.bin` file produced by the cache.
fn blob_sha(repo_root: &Path, rel: &str) -> String {
    let out = Command::new("git")
        .args(["hash-object", rel])
        .current_dir(repo_root)
        .output()
        .expect("git hash-object");
    assert!(out.status.success(), "git hash-object failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Locate the on-disk cache file for a given blob SHA. Returns the path
/// whether or not the file exists; callers assert on `.exists()`.
///
/// Layout: `BlobCache` writes entries to `<root>/<sha[0..2]>/<sha>.bin`. The
/// root passed to `BlobCache::new` is `config::blob_cache_dir()`
/// (= `<cache_root>/blobs`), so the final on-disk path is
/// `<cache_root>/blobs/<sha[0..2]>/<sha>.bin` — one `blobs/` segment owned
/// by `blob_cache_dir`, no duplication.
fn cache_entry_path(sha: &str) -> PathBuf {
    vex::util::config::blob_cache_dir()
        .join(&sha[..2])
        .join(format!("{sha}.bin"))
}

// ── Test 1: tracked file → cache write on miss, cache hit on rerun ───────────

/// First `pipeline::run` writes the cache entry; second `pipeline::run`
/// on the same commit must skip the parse path entirely. Mtime stability
/// is the cleanest observable proof: `BlobCache::insert` always rewrites
/// the file via temp + atomic rename, so a fresh write moves mtime forward.
#[test]
fn pipeline_run_caches_tracked_files_and_skips_rewrite_on_rerun() {
    shared_cache_root();

    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");

    let repo_root = init_git_repo(
        &project,
        &[(
            "src/lib.rs",
            "pub fn cached_function() -> u32 { 42 }\n\
             pub fn helper() -> bool { true }\n",
        )],
    );

    let sha = blob_sha(&repo_root, "src/lib.rs");
    let entry_path = cache_entry_path(&sha);

    // First run: cache miss, file is written.
    let (count1, _) = pipeline::run(
        &repo_root,
        pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .expect("first pipeline::run failed");

    assert!(
        entry_path.exists(),
        "expected blob cache entry at {} after first run",
        entry_path.display()
    );

    let mtime_after_first = std::fs::metadata(&entry_path)
        .expect("stat entry path")
        .modified()
        .expect("read mtime");

    // Sleep just long enough that a fresh rename would produce a different
    // mtime on platforms with second-resolution timestamps (HFS+, some
    // network filesystems). 1.1s gives a comfortable margin on macOS.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Second run: cache hit, file must NOT be rewritten.
    let (count2, _) = pipeline::run(
        &repo_root,
        pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .expect("second pipeline::run failed");

    let mtime_after_second = std::fs::metadata(&entry_path)
        .expect("stat entry path after second run")
        .modified()
        .expect("read mtime after second run");

    assert_eq!(
        mtime_after_first, mtime_after_second,
        "blob cache entry was rewritten on second run — cache hit path did not short-circuit"
    );

    assert_eq!(
        count1, count2,
        "symbol count must be identical across cached and uncached runs"
    );
    assert!(count1 >= 2, "expected at least 2 symbols, got {count1}");
}

// ── Test 2: dirty tracked file → cache skipped (Step 4c correctness) ─────────

/// Step 4c: a tracked file with uncommitted edits in the working tree
/// must NOT be written to the blob cache under its HEAD blob SHA. The
/// cache key is the staged/HEAD SHA, but the parsed content is the
/// (different) working tree — caching it would poison the entry for any
/// other checkout of that same SHA. Pipeline correctness still holds:
/// the file is indexed via the existing xxh3 path.
#[test]
fn pipeline_run_skips_blob_cache_for_dirty_tracked_file() {
    shared_cache_root();

    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");

    let repo_root = init_git_repo(
        &project,
        &[("src/foo.rs", "pub fn clean_committed_fn() -> u32 { 1 }\n")],
    );

    // Capture the HEAD blob SHA of foo.rs BEFORE we dirty the working tree —
    // that's the SHA the cache would key under if Step 4c failed.
    let head_sha = blob_sha(&repo_root, "src/foo.rs");

    // Modify the working tree without staging. `git diff-files` will report
    // src/foo.rs as dirty, and Step 4c must drop it from the blob map.
    std::fs::write(
        repo_root.join("src").join("foo.rs"),
        "pub fn dirty_working_tree_fn() -> u32 { 99 }\n\
         pub fn extra_symbol() -> bool { true }\n",
    )
    .unwrap();

    let (count, _) = pipeline::run(
        &repo_root,
        pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .expect("pipeline::run failed");

    // Correctness: the dirty working-tree content must still be indexed via
    // the xxh3 fallback. Both symbols from the modified file should appear.
    assert!(
        count >= 2,
        "expected at least 2 symbols from the dirty working tree, got {count}"
    );

    // Safety: the HEAD SHA must NOT have a cache file. The dirty working
    // tree should never be cached under the staged/HEAD blob SHA.
    let entry_path = cache_entry_path(&head_sha);
    assert!(
        !entry_path.exists(),
        "blob cache wrote dirty working-tree content under HEAD SHA at {} \
         — Step 4c filter is not working",
        entry_path.display()
    );
}

// ── Test 3: untracked file → cache is bypassed ───────────────────────────────

/// An untracked file (not in `git ls-files`) must NOT produce a cache
/// entry, even though `pipeline::run` still indexes it via the parse path.
#[test]
fn pipeline_run_does_not_cache_untracked_files() {
    shared_cache_root();

    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");

    // Commit one tracked file so the repo has a HEAD; the test asserts on
    // a SECOND file that we deliberately do NOT `git add`.
    let repo_root = init_git_repo(&project, &[("src/tracked.rs", "pub fn tracked() {}\n")]);

    // Write the untracked file AFTER the initial commit. `git ls-files`
    // will not see it; the existing xxh3 path picks it up.
    std::fs::write(
        repo_root.join("src").join("untracked.rs"),
        "pub fn never_cached_in_blob_dir() -> u8 { 7 }\n",
    )
    .unwrap();

    let untracked_sha = {
        // `git hash-object` works on untracked content too — it just
        // computes the SHA without recording it. We use this to assert
        // the file is *missing* from the cache, not present.
        let out = Command::new("git")
            .args(["hash-object", "src/untracked.rs"])
            .current_dir(&repo_root)
            .output()
            .expect("git hash-object");
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };

    let (count, _) = pipeline::run(
        &repo_root,
        pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .expect("pipeline::run failed");

    // Untracked file must still be indexed via the xxh3 fallback.
    assert!(
        count >= 2,
        "expected both tracked + untracked symbols, got {count}"
    );

    let untracked_entry = cache_entry_path(&untracked_sha);
    assert!(
        !untracked_entry.exists(),
        "untracked file's blob hash ended up in the cache at {} — \
         pipeline must not insert for files missing from `git ls-files`",
        untracked_entry.display()
    );
}
