//! git backend — the default, and the only backend wired in Phase 1.
//!
//! The command logic here is a verbatim move of the pre-abstraction
//! `util::git_diff` helpers, so behavior (arg order, `-z`, `--no-renames`,
//! the `--` terminator, the merge-base candidate ladder, error strings) is
//! byte-identical. `ChangedPaths::resolve` is now a thin caller of this.
//!
//! Note: `src/history/mod.rs` keeps its OWN `ensure_git_worktree` — it is
//! deliberately NOT shared with [`GitVcs::ensure_repo`] because history
//! surfaces a distinct `vex history`-specific error string (the two-message
//! contract the VCS design's L1 preserves) and history is not yet routed
//! through the `Vcs` trait. Do not dedup them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::{DiffScope, Vcs, VcsCapabilities, VcsError, VcsKind, VcsResult};

/// git VCS backend. Fieldless — every operation shells out to `git` in
/// `root`, so a single value is trivially `Send + Sync`.
#[derive(Debug, Default, Clone, Copy)]
pub struct GitVcs;

impl Vcs for GitVcs {
    fn kind(&self) -> VcsKind {
        VcsKind::Git
    }

    fn capabilities(&self) -> VcsCapabilities {
        VcsCapabilities {
            merge_base: true,
            content_addressed: true,
        }
    }

    /// Verify `root` is inside a git worktree before invoking `git diff`.
    /// Uses `git rev-parse --is-inside-work-tree`, which exits non-zero
    /// outside any repo — unlike `git diff`, which falls back to no-index
    /// mode and prints help. Without this guard a `--since main` call in a
    /// non-repo directory would silently return an empty change set.
    fn ensure_repo(&self, root: &Path) -> VcsResult<()> {
        let output = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(root)
            .output()
            .context("invoke git rev-parse")
            .map_err(VcsError::Failed)?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.trim() == "true" {
                return Ok(());
            }
        }
        Err(VcsError::Failed(anyhow::anyhow!(
            "not a git repository at {}: --since/--since-branched/--changed-only require a git checkout",
            root.display()
        )))
    }

    fn changed_paths(&self, root: &Path, scope: DiffScope) -> VcsResult<Vec<String>> {
        git_changed_paths(root, scope).map_err(VcsError::Failed)
    }

    /// Blob SHAs for tracked regular files (Phase-14.7 parse cache). Any git
    /// failure yields an EMPTY map (logged, not propagated) — the pipeline
    /// falls through to the xxh3/mtime path, so this never returns `Err`. The
    /// `Result` exists only for the trait default (`Unsupported`); see the
    /// inverted-H2 note on [`Vcs::tracked_content_ids`].
    fn tracked_content_ids(&self, root: &Path) -> VcsResult<HashMap<PathBuf, String>> {
        Ok(discover_tracked_blobs(root))
    }
}

/// Scope → git command dispatch. Kept as an `anyhow::Result` inner so the
/// existing helpers (and their exact error strings) move unchanged; the trait
/// method wraps the outcome into `VcsError::Failed`.
fn git_changed_paths(root: &Path, scope: DiffScope) -> Result<Vec<String>> {
    match scope {
        DiffScope::Since(rev) => git_diff_name_only(root, &format!("{rev}..HEAD")),
        DiffScope::SinceBranched => {
            let base = resolve_merge_base(root)?;
            git_diff_name_only(root, &format!("{base}..HEAD"))
        }
        DiffScope::ChangedOnly => collect_working_tree_changes(root),
    }
}

/// `git diff --name-only -z <range>` parsed into a Vec.
///
/// `-z` gives us null-terminated output that survives pathological
/// filenames (newlines, quotes) — `lines()` would split mid-path otherwise.
fn git_diff_name_only(root: &Path, range: &str) -> Result<Vec<String>> {
    // Trailing `--`: terminate the revision list so any user-supplied
    // `--since` value that starts with `-` (e.g. `--no-renames`) doesn't
    // sneak in as a git flag. We can't put `--` BEFORE the range — git
    // would then read the range as a pathspec — so it sits at the end,
    // which is the documented form for `git diff [<options>] [<rev>] [--]`.
    let output = Command::new("git")
        .args(["diff", "--name-only", "--no-renames", "-z", range, "--"])
        .current_dir(root)
        .output()
        .context("invoke git diff --name-only")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_t = stderr.trim();
        if stderr_t.contains("not a git repository") {
            bail!(
                "not a git repository at {}: --since/--since-branched/--changed-only require a git checkout",
                root.display()
            );
        }
        bail!("git diff --name-only {range} failed: {stderr_t}");
    }
    Ok(split_nul(&output.stdout))
}

/// Working-tree change set: union of unstaged + staged + untracked.
///
/// `git diff HEAD` captures both staged and unstaged changes against the
/// HEAD tree in one shot — no need to also query the index separately.
/// `git ls-files --others --exclude-standard` then adds untracked files
/// while respecting `.gitignore`.
fn collect_working_tree_changes(root: &Path) -> Result<Vec<String>> {
    // Tracked changes vs. HEAD (staged + unstaged in one query).
    // Trailing `--` keeps any future caller-supplied pathspec from
    // colliding with `--` flags; the literal `HEAD` here is fixed so
    // there's no rev-injection risk today, but the form is identical
    // for consistency with `git_diff_name_only`.
    let diff = Command::new("git")
        .args(["diff", "HEAD", "--name-only", "--no-renames", "-z", "--"])
        .current_dir(root)
        .output()
        .context("invoke git diff HEAD --name-only")?;
    if !diff.status.success() {
        let stderr = String::from_utf8_lossy(&diff.stderr);
        let stderr_t = stderr.trim();
        if stderr_t.contains("not a git repository") {
            bail!(
                "not a git repository at {}: --changed-only requires a git checkout",
                root.display()
            );
        }
        // Empty repo (no HEAD yet): treat as "everything untracked" by
        // skipping the diff portion. `unknown revision or path 'HEAD'`
        // surfaces here.
        if !stderr_t.contains("unknown revision") && !stderr_t.contains("ambiguous argument") {
            bail!("git diff HEAD --name-only failed: {stderr_t}");
        }
    }
    let mut out = if diff.status.success() {
        split_nul(&diff.stdout)
    } else {
        Vec::new()
    };

    // Untracked files (still respecting .gitignore).
    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .current_dir(root)
        .output()
        .context("invoke git ls-files --others")?;
    if !untracked.status.success() {
        let stderr = String::from_utf8_lossy(&untracked.stderr);
        bail!(
            "git ls-files --others --exclude-standard failed: {}",
            stderr.trim()
        );
    }
    out.extend(split_nul(&untracked.stdout));
    Ok(out)
}

/// Split null-terminated git output into a clean `Vec<String>`. Tolerant of
/// the trailing `\0` git always emits, and of empty stdout.
pub(super) fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Resolve the merge-base for `SinceBranched`, falling back through
/// `origin/main` -> `origin/master` -> `main` -> `master`. Returns the
/// first ref that yields a merge-base. Errors with an actionable message
/// when none works — usually means the project has a non-default trunk
/// name (e.g. `develop`) and the user should use `--since develop` instead.
fn resolve_merge_base(root: &Path) -> Result<String> {
    const CANDIDATES: &[&str] = &["origin/main", "origin/master", "main", "master"];
    let mut tried = Vec::with_capacity(CANDIDATES.len());
    for cand in CANDIDATES {
        match try_merge_base(root, cand)? {
            Some(sha) => return Ok(sha),
            None => tried.push(*cand),
        }
    }
    bail!(
        "--since-branched: no merge-base found against any of {}. \
         Run `vex search ... --since <your-trunk>` instead, or push your branch \
         so `origin/main` exists.",
        tried.join(", ")
    );
}

/// Try `git merge-base HEAD <ref>`. Returns `Ok(Some(sha))` on success,
/// `Ok(None)` when the ref doesn't exist or no merge-base exists (both
/// are normal fall-through conditions for `resolve_merge_base`'s ladder),
/// and `Err` only for unexpected git failures (corrupt repo, etc.).
fn try_merge_base(root: &Path, reference: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["merge-base", "HEAD", reference])
        .current_dir(root)
        .output()
        .context("invoke git merge-base")?;
    if output.status.success() {
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if sha.is_empty() {
            return Ok(None);
        }
        return Ok(Some(sha));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_t = stderr.trim();
    if stderr_t.contains("not a git repository") {
        bail!(
            "not a git repository at {}: --since-branched requires a git checkout",
            root.display()
        );
    }
    // "Not a valid object name" / "unknown revision" / silent exit 1 with
    // no merge-base: treat as "this candidate didn't work, try the next".
    Ok(None)
}

// ---- Phase-14.7 blob-SHA parse cache: tracked-file discovery ----
//
// Moved verbatim from `index::parse_cache::git_blobs` when the content cache
// was routed through the `Vcs` trait (VCS-BACKENDS Phase 5). `git ls-files -s`
// gives the SHA of the *staged/HEAD* blob; the follow-up `git diff-files`
// drops dirty paths so we never cache a working-tree AST under the index SHA
// (a future clean checkout of that SHA would read back the wrong AST). Best
// effort: any git failure returns an empty map / skips the filter — the
// pipeline's xxh3 path absorbs the miss, no error propagated.

/// Spawn `git -C <repo_root> ls-files -s` and return a map of
/// absolute-canonical paths → 40-char hex blob SHA for every tracked regular
/// file. Symlinks (`120000`) and gitlinks (`160000`) are excluded. Returns an
/// empty map on any failure (git missing, non-repo, non-zero exit, non-UTF-8).
fn discover_tracked_blobs(repo_root: &Path) -> HashMap<PathBuf, String> {
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

    // Drop paths whose working tree content differs from the index. Caching
    // those would associate the working-tree AST with the index/HEAD blob SHA
    // and poison the cache for other checkouts.
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
/// differs from the index. Returns an empty set on any failure — the caller
/// treats an empty set as "no dirty paths known" and skips the filter.
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

/// Parse the raw `-z`-separated `git diff-files --name-only` output into a set
/// of canonical absolute paths. Entries that fail to canonicalize (deleted,
/// permission errors, …) are dropped — a missing path cannot be in the blob
/// map anyway.
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

/// Parse the raw `git ls-files -s` output into the `absolute_path → blob_sha`
/// map. Each line is `<mode> <sha> <stage>\t<path>`; non-regular modes
/// (`120000` symlink, `160000` gitlink) are skipped. Paths are canonicalized
/// to line up with `pipeline::discover_files`; a path that fails to
/// canonicalize is dropped (falls through to the parse path on miss).
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
    use tempfile::TempDir;

    #[test]
    fn split_nul_drops_trailing_empty() {
        assert_eq!(split_nul(b"a\0b\0"), vec!["a", "b"]);
        assert_eq!(split_nul(b""), Vec::<String>::new());
        assert_eq!(split_nul(b"only\0"), vec!["only"]);
    }

    fn run_git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git invocation");
        assert!(status.success(), "git {args:?} failed");
    }

    /// `SinceBranched` on a repo whose branch is not main/master and has no
    /// origin exercises `resolve_merge_base`'s full candidate-ladder miss and
    /// its actionable error. Fills a pre-existing coverage gap (code-review
    /// LOW) — the ladder's failure path had no direct test before Phase 1.
    #[test]
    fn since_branched_errors_actionably_when_no_trunk_ref_exists() {
        let tmp = TempDir::new().unwrap();
        run_git(tmp.path(), &["init", "-q", "-b", "develop"]);
        run_git(tmp.path(), &["config", "user.email", "t@t"]);
        run_git(tmp.path(), &["config", "user.name", "T"]);
        run_git(tmp.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        run_git(tmp.path(), &["add", "-A"]);
        run_git(tmp.path(), &["commit", "-q", "-m", "init"]);

        let err = GitVcs
            .changed_paths(tmp.path(), DiffScope::SinceBranched)
            .expect_err("no origin/main|master|main|master ref → merge-base miss");
        let msg = format!("{}", err.into_anyhow());
        assert!(
            msg.contains("no merge-base found"),
            "expected the actionable merge-base ladder error, got: {msg}"
        );
    }

    // ---- blob-cache tracked-file parsing (moved from git_blobs.rs) ----

    #[test]
    fn parses_ls_files_output_into_map() {
        let tmp = TempDir::new().unwrap();
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

    #[test]
    fn path_with_embedded_tab_is_preserved() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Split-on-first-tab: the canonicalization step drops the non-existent
        // path, which is the correct behaviour for missing files.
        let stdout = "100644 1111111111111111111111111111111111111111 0\tnot_on_disk.rs\n";
        let map = parse_ls_files_output(stdout, &root);
        assert!(
            map.is_empty(),
            "non-existent paths must be filtered out by canonicalize"
        );
    }

    #[test]
    fn empty_ls_files_output_returns_empty_map() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert!(parse_ls_files_output("", &root).is_empty());
    }

    #[test]
    fn parses_diff_files_nul_output_into_set() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        std::fs::write(root.join("dirty.rs"), b"fn dirty() {}\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("also_dirty.rs"), b"fn x() {}\n").unwrap();

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

    #[test]
    fn parses_diff_files_empty_output_returns_empty_set() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert!(parse_diff_files_output(&[], &root).is_empty());
    }
}
