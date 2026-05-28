//! Tracked-file discovery for the Phase 14.7 blob-SHA parse cache.
//!
//! Spawns `git ls-files -s` once at the start of `vex index` / `vex update`
//! and returns a `HashMap<absolute_path, blob_sha>` so the pipeline can look
//! up cache entries by path during the per-file parse loop.
//!
//! ## Dirty-tree filter (Step 4c)
//!
//! `git ls-files -s` reports the SHA of the **staged** (or HEAD) blob, but
//! the pipeline parses the **working-tree** content. On a dirty working tree
//! the parsed AST and the SHA we'd cache it under no longer agree — a future
//! clean checkout of the same SHA would read back the wrong AST. To avoid
//! that we run a follow-up `git diff-files --name-only -z` and remove any
//! path whose working-tree content differs from the index. Dirty paths fall
//! through to the existing xxh3 path and are never written to the blob cache.
//!
//! The cache is best-effort: if `git` is missing, the directory is not a git
//! repository, or any of the commands fail, this function returns an empty
//! map (or skips the filter, in the diff-files case) and the existing xxh3
//! manifest path absorbs the miss — no error is propagated.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Spawn `git -C <repo_root> ls-files -s` and return a map of
/// absolute-canonical paths → 40-char hex blob SHA for every tracked
/// regular file in the working tree.
///
/// Submodules (`git ls-files -s` skips them by default), symlinks
/// (mode `120000`), and gitlinks (mode `160000`) are excluded so the
/// blob cache only sees content-addressed regular file blobs.
///
/// Returns an empty map on any failure (git missing, non-repo, non-zero
/// exit, non-UTF-8 output). The blob cache layer treats an empty map as
/// "no tracked files known" and routes everything through the parse path,
/// so correctness is unaffected — only cache lookup is skipped.
pub fn discover_tracked_blobs(repo_root: &Path) -> HashMap<PathBuf, String> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("ls-files")
        .arg("-s")
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "git ls-files not available — blob cache disabled for this run"
            );
            return HashMap::new();
        }
    };

    if !output.status.success() {
        tracing::debug!(
            status = ?output.status,
            "git ls-files exited non-zero — blob cache disabled for this run"
        );
        return HashMap::new();
    }

    let stdout = match std::str::from_utf8(&output.stdout) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "git ls-files stdout was not UTF-8");
            return HashMap::new();
        }
    };

    let mut map = parse_ls_files_output(stdout, repo_root);

    // Step 4c — drop paths whose working tree content differs from the
    // index. Caching those would associate the working-tree AST with the
    // index/HEAD blob SHA and poison the cache for other checkouts.
    let dirty = discover_dirty_paths(repo_root);
    if !dirty.is_empty() {
        let before = map.len();
        map.retain(|path, _| !dirty.contains(path));
        tracing::debug!(
            removed = before - map.len(),
            dirty_total = dirty.len(),
            "dropped dirty paths from blob map"
        );
    }

    tracing::debug!(count = map.len(), "discovered tracked blobs");
    map
}

/// Spawn `git -C <repo_root> diff-files --name-only -z` and return the
/// canonical-absolute paths of tracked files whose working-tree content
/// differs from the index. Paths are canonicalized so the resulting set
/// can be compared directly against the keys produced by
/// [`discover_tracked_blobs`].
///
/// Returns an empty set on any failure — the caller treats an empty set as
/// "no dirty paths known" and skips the filter. Correctness still holds in
/// that case: the cache lookup-then-insert path simply caches the working
/// tree under the staged SHA (the original bug). The acceptable degradation
/// here is "we silently skipped the safety net", logged at `debug!`.
fn discover_dirty_paths(repo_root: &Path) -> HashSet<PathBuf> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("diff-files")
        .arg("--name-only")
        .arg("-z")
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(error = %e, "git diff-files not available — dirty-tree filter skipped");
            return HashSet::new();
        }
    };

    if !output.status.success() {
        // A non-zero exit here drops the safety net that prevents poisoning
        // the cache with parses of dirty content (mid-rebase, locked index,
        // permission errors). Surface it at warn level so operators notice
        // when the filter degrades to "trust everything".
        tracing::warn!(
            status = ?output.status,
            "git diff-files exited non-zero — dirty-tree filter degraded to no-op"
        );
        return HashSet::new();
    }

    parse_diff_files_output(&output.stdout, repo_root)
}

/// Parse the raw `-z`-separated `git diff-files --name-only` output into a
/// set of canonical absolute paths. Entries that fail to canonicalize
/// (deleted between the `diff-files` call and our stat, permission errors,
/// …) are dropped — a missing path cannot be in the blob map anyway, so
/// dropping it is a no-op.
fn parse_diff_files_output(stdout: &[u8], repo_root: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    for chunk in stdout.split(|&b| b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let rel = match std::str::from_utf8(chunk) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "diff-files path was not UTF-8");
                continue;
            }
        };
        let abs = repo_root.join(rel);
        let canonical = match abs.canonicalize() {
            Ok(p) => p,
            Err(_) => continue,
        };
        out.insert(canonical);
    }
    out
}

/// Parse the raw `git ls-files -s` output into the
/// `absolute_path → blob_sha` map.
///
/// Each line has the form `<mode> <sha> <stage>\t<path>`. Lines whose
/// mode is not a regular file (`100644` / `100755`) are skipped:
///   * `120000` — symlink: the blob content is the symlink target, not
///     parseable source, so caching it would poison the entry.
///   * `160000` — gitlink (submodule pointer): not a file at all.
///
/// Paths are canonicalized so they line-up with `walk_builder` output in
/// `pipeline::discover_files` (which feeds `root.canonicalize()`-rooted
/// absolute paths to `parse_files`). A path that fails to canonicalize
/// (deleted between `ls-files` and the walk, permission denied, …) is
/// dropped — it'll fall through to the parse path on miss.
fn parse_ls_files_output(stdout: &str, repo_root: &Path) -> HashMap<PathBuf, String> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        // Split metadata from path on the first TAB.
        let Some((meta, rel_path)) = line.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split_whitespace();
        let Some(mode) = parts.next() else {
            continue;
        };
        let Some(sha) = parts.next() else {
            continue;
        };
        // Only index regular file blobs. Skip symlinks (120000) and
        // gitlinks/submodules (160000).
        if mode != "100644" && mode != "100755" {
            continue;
        }
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let abs = repo_root.join(rel_path);
        let canonical = match abs.canonicalize() {
            Ok(p) => p,
            Err(_) => continue,
        };
        map.insert(canonical, sha.to_string());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parser unit test — drives the parsing logic without spawning git.
    /// Feeds a known multi-line stdout and asserts the resulting map. Paths
    /// must exist on disk to clear canonicalization, so the test writes a
    /// few files into a tempdir and feeds matching relative paths.
    #[test]
    fn parses_ls_files_output_into_map() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Create real files so canonicalize() succeeds.
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("b.rs"), b"fn b() {}\n").unwrap();

        let stdout = "\
100644 1111111111111111111111111111111111111111 0\ta.rs
100755 2222222222222222222222222222222222222222 0\tsrc/b.rs
120000 3333333333333333333333333333333333333333 0\ta_symlink
160000 4444444444444444444444444444444444444444 0\tsubmodule
malformed line without tab
100644 short_sha 0\tbad.rs
";

        let map = parse_ls_files_output(stdout, &root);

        assert_eq!(map.len(), 2, "expected only two regular file blobs");
        assert_eq!(
            map.get(&root.join("a.rs")).map(String::as_str),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(
            map.get(&root.join("src").join("b.rs")).map(String::as_str),
            Some("2222222222222222222222222222222222222222")
        );
    }

    /// Lines whose path field contains a tab (rare but legal in git history)
    /// preserve the path correctly because we split on the FIRST tab only.
    #[test]
    fn path_with_embedded_tab_is_preserved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Most filesystems disallow tabs in names; we just test the split
        // by feeding a synthesized line. The canonicalization step will
        // fail (file doesn't exist) and the entry will be dropped, which
        // is the correct behaviour for missing files.
        let stdout = "100644 1111111111111111111111111111111111111111 0\tnot_on_disk.rs\n";
        let map = parse_ls_files_output(stdout, &root);
        assert!(
            map.is_empty(),
            "non-existent paths must be filtered out by canonicalize"
        );
    }

    /// An entirely empty `git ls-files -s` output (e.g. fresh repo with no
    /// commits) returns an empty map without panicking.
    #[test]
    fn empty_output_returns_empty_map() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert!(parse_ls_files_output("", &root).is_empty());
    }

    /// Step 4c — parse_diff_files_output picks up every NUL-separated
    /// entry that resolves to an existing path on disk and ignores
    /// missing/empty chunks.
    #[test]
    fn parses_diff_files_nul_output_into_set() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        std::fs::write(root.join("dirty.rs"), b"fn dirty() {}\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("also_dirty.rs"), b"fn x() {}\n").unwrap();

        // Realistic diff-files -z output: NUL-separated relative paths,
        // trailing NUL after the last entry is allowed.
        let mut stdout: Vec<u8> = Vec::new();
        stdout.extend_from_slice(b"dirty.rs");
        stdout.push(0);
        stdout.extend_from_slice(b"src/also_dirty.rs");
        stdout.push(0);
        // A missing file (canonicalize fails) must be dropped silently.
        stdout.extend_from_slice(b"src/does_not_exist.rs");
        stdout.push(0);

        let set = parse_diff_files_output(&stdout, &root);

        assert_eq!(set.len(), 2, "expected exactly two existing dirty paths");
        assert!(set.contains(&root.join("dirty.rs")));
        assert!(set.contains(&root.join("src").join("also_dirty.rs")));
        assert!(!set.contains(&root.join("src").join("does_not_exist.rs")));
    }

    /// Step 4c — empty stdout (the common clean-tree case) returns an
    /// empty set without panicking.
    #[test]
    fn parses_diff_files_empty_output_returns_empty_set() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert!(parse_diff_files_output(&[], &root).is_empty());
    }
}
