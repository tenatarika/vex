//! Resolve the set of paths affected by a git ref-range or working-tree
//! state. Used by Phase 13.7-D3 to scope search-shaped commands to recent
//! changes — `--since <rev>`, `--since-branched`, and `--changed-only`.
//!
//! The module shells out to `git` via `std::process::Command` (no libgit2
//! dependency). Every invocation passes `-z` for null-terminated output so
//! filenames with embedded newlines or unicode oddities survive the round
//! trip intact.
//!
//! Resolved paths are repo-relative POSIX strings — the same shape vex's
//! indexer stores in `SearchResult::path`. Membership lookup is HashSet O(1)
//! per result so even a wide working-tree change set scales to whatever the
//! caller's `limit` is.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Selects which set of changed paths a search-shaped command should be
/// restricted to.
///
/// The variants are mutually exclusive at the CLI layer (clap
/// `conflicts_with_all`). We carry a borrowed `&str` for `--since` so the
/// caller's existing `String` can flow straight through without an extra
/// allocation.
#[derive(Debug, Clone, Copy)]
pub enum DiffScope<'a> {
    /// `--since <rev>` — files changed between `<rev>..HEAD`.
    Since(&'a str),
    /// `--since-branched` — files changed since branch diverged from
    /// main/master (origin first, then local).
    SinceBranched,
    /// `--changed-only` — working-tree dirty + untracked.
    ChangedOnly,
}

impl DiffScope<'_> {
    /// Human-readable label used by the `_meta.diff_filter.scope` JSON
    /// field. Stable string — wire-format consumers may key on it.
    pub fn label(&self) -> &'static str {
        match self {
            DiffScope::Since(_) => "since",
            DiffScope::SinceBranched => "since_branched",
            DiffScope::ChangedOnly => "changed_only",
        }
    }
}

/// Set of repo-relative paths a command's results must intersect.
///
/// Constructed once per invocation via [`ChangedPaths::resolve`]; reused as
/// a HashSet for the result-rerank filter. The set may be empty (valid:
/// "nothing changed in this range") — callers must distinguish that from
/// "filter disabled".
#[derive(Debug, Default)]
pub struct ChangedPaths {
    paths: HashSet<String>,
}

impl ChangedPaths {
    /// Run the appropriate `git` invocation for `scope`, collect the paths
    /// it returns, and normalize them to POSIX form for membership checks.
    ///
    /// Errors:
    ///   * Non-zero git exit → propagated with stderr in the context.
    ///   * `--since-branched` with no merge-base in any of
    ///     `origin/main`, `origin/master`, `main`, `master` → actionable
    ///     error message.
    ///   * Repo dir is not a git repo → first `git` invocation fails with
    ///     `not a git repository` in stderr; surfaced verbatim.
    pub fn resolve(repo_root: &Path, scope: DiffScope) -> Result<Self> {
        // Pre-flight: `git diff` outside a worktree exits 0 with help on
        // stderr (a fallback to `--no-index` mode), so a missing-repo
        // error would otherwise leak past the success check below and
        // silently return an empty set. Surface it here with a stable,
        // testable phrasing.
        ensure_git_worktree(repo_root)?;
        let raw = match scope {
            DiffScope::Since(rev) => git_diff_name_only(repo_root, &format!("{rev}..HEAD"))?,
            DiffScope::SinceBranched => {
                let base = resolve_merge_base(repo_root)?;
                git_diff_name_only(repo_root, &format!("{base}..HEAD"))?
            }
            DiffScope::ChangedOnly => collect_working_tree_changes(repo_root)?,
        };
        let paths: HashSet<String> = raw.into_iter().map(normalize_posix).collect();
        Ok(Self { paths })
    }

    /// True when `path` (in any separator flavour) is in the changed set.
    /// HashSet lookup — O(1) amortized.
    pub fn contains(&self, path: &str) -> bool {
        // Normalize on the lookup side too so Windows index paths
        // (`src\\foo.rs`) match git's POSIX output (`src/foo.rs`).
        if self.paths.contains(path) {
            return true;
        }
        if path.contains('\\') {
            self.paths.contains(&path.replace('\\', "/"))
        } else {
            false
        }
    }

    /// True when the change set is empty. Currently only used by tests
    /// and external callers; kept on the API for completeness so
    /// downstream consumers don't have to read `len() == 0`.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }
}

/// Verify `repo_root` is inside a git worktree before invoking `git diff`.
/// Uses `git rev-parse --is-inside-work-tree`, which exits non-zero outside
/// any repo — unlike `git diff`, which falls back to no-index mode and
/// prints help. Without this guard a `--since main` call in a non-repo
/// directory would silently return an empty change set.
fn ensure_git_worktree(repo_root: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(repo_root)
        .output()
        .context("invoke git rev-parse")?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim() == "true" {
            return Ok(());
        }
    }
    bail!(
        "not a git repository at {}: --since/--since-branched/--changed-only require a git checkout",
        repo_root.display()
    );
}

/// Convert a repo-relative path string (possibly with Windows-style
/// backslashes) to POSIX form. Git on every platform emits forward slashes,
/// but the index can store paths with either separator depending on the OS
/// of the indexing host — so we normalize on both sides of the membership
/// check.
fn normalize_posix(p: String) -> String {
    if p.contains('\\') {
        p.replace('\\', "/")
    } else {
        p
    }
}

/// `git diff --name-only -z <range>` parsed into a Vec.
///
/// `-z` gives us null-terminated output that survives pathological
/// filenames (newlines, quotes) — `lines()` would split mid-path otherwise.
fn git_diff_name_only(root: &Path, range: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["diff", "--name-only", "--no-renames", "-z", range])
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
    let diff = Command::new("git")
        .args(["diff", "HEAD", "--name-only", "--no-renames", "-z"])
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
fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Resolve the merge-base for `--since-branched`, falling back through
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn run_git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git invocation");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        run_git(tmp.path(), &["config", "user.email", "t@t"]);
        run_git(tmp.path(), &["config", "user.name", "T"]);
        run_git(tmp.path(), &["config", "commit.gpgsign", "false"]);
        tmp
    }

    fn commit_all(root: &Path, msg: &str) {
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-q", "-m", msg]);
    }

    #[test]
    fn since_finds_files_modified_in_head() {
        let tmp = init_repo();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn b() {}\n").unwrap();
        commit_all(tmp.path(), "init");
        std::fs::write(tmp.path().join("b.rs"), "fn b() { 1 }\n").unwrap();
        commit_all(tmp.path(), "edit b");

        let cp = ChangedPaths::resolve(tmp.path(), DiffScope::Since("HEAD~1")).unwrap();
        assert!(cp.contains("b.rs"), "b.rs should be in change set");
        assert!(!cp.contains("a.rs"), "a.rs should NOT be in change set");
        assert_eq!(cp.len(), 1);
    }

    #[test]
    fn since_branched_uses_merge_base_with_main() {
        let tmp = init_repo();
        std::fs::write(tmp.path().join("trunk.rs"), "fn t() {}\n").unwrap();
        commit_all(tmp.path(), "trunk");
        run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(tmp.path().join("feature.rs"), "fn f() {}\n").unwrap();
        commit_all(tmp.path(), "feature");

        let cp = ChangedPaths::resolve(tmp.path(), DiffScope::SinceBranched).unwrap();
        assert!(cp.contains("feature.rs"));
        assert!(!cp.contains("trunk.rs"));
    }

    #[test]
    fn changed_only_includes_unstaged_and_untracked() {
        let tmp = init_repo();
        std::fs::write(tmp.path().join("committed.rs"), "fn c() {}\n").unwrap();
        commit_all(tmp.path(), "init");

        // Modify a tracked file (unstaged) + add an untracked file.
        std::fs::write(tmp.path().join("committed.rs"), "fn c() { 1 }\n").unwrap();
        std::fs::write(tmp.path().join("new.rs"), "fn n() {}\n").unwrap();

        let cp = ChangedPaths::resolve(tmp.path(), DiffScope::ChangedOnly).unwrap();
        assert!(cp.contains("committed.rs"), "tracked-dirty should match");
        assert!(cp.contains("new.rs"), "untracked should match");
    }

    #[test]
    fn changed_only_includes_staged() {
        let tmp = init_repo();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(tmp.path(), "init");

        std::fs::write(tmp.path().join("a.rs"), "fn a() { 1 }\n").unwrap();
        run_git(tmp.path(), &["add", "a.rs"]);

        let cp = ChangedPaths::resolve(tmp.path(), DiffScope::ChangedOnly).unwrap();
        assert!(cp.contains("a.rs"));
    }

    #[test]
    fn non_git_repo_errors_actionably() {
        let tmp = TempDir::new().unwrap();
        let err = ChangedPaths::resolve(tmp.path(), DiffScope::Since("HEAD~1")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a git repository"),
            "expected not-a-repo error, got: {msg}"
        );
    }

    #[test]
    fn empty_diff_yields_empty_set() {
        let tmp = init_repo();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(tmp.path(), "init");
        // No changes since HEAD~0 (HEAD vs HEAD).
        let cp = ChangedPaths::resolve(tmp.path(), DiffScope::Since("HEAD")).unwrap();
        assert!(cp.is_empty());
        assert_eq!(cp.len(), 0);
        assert!(!cp.contains("a.rs"));
    }

    #[test]
    fn path_with_space_is_handled() {
        let tmp = init_repo();
        let with_space = tmp.path().join("my dir");
        std::fs::create_dir(&with_space).unwrap();
        std::fs::write(with_space.join("file.rs"), "fn x() {}\n").unwrap();
        commit_all(tmp.path(), "init");
        std::fs::write(with_space.join("file.rs"), "fn x() { 1 }\n").unwrap();
        commit_all(tmp.path(), "edit");

        let cp = ChangedPaths::resolve(tmp.path(), DiffScope::Since("HEAD~1")).unwrap();
        assert!(cp.contains("my dir/file.rs"));
    }

    #[test]
    fn windows_separator_normalizes_on_lookup() {
        // Hand-craft a ChangedPaths that already holds POSIX paths (git's
        // output everywhere), and confirm a Windows-style query string
        // still finds it via the lookup-side normalization.
        let mut paths = HashSet::new();
        paths.insert("src/foo.rs".to_string());
        let cp = ChangedPaths { paths };
        assert!(cp.contains("src/foo.rs"));
        assert!(cp.contains("src\\foo.rs"));
        assert!(!cp.contains("src/bar.rs"));
    }

    #[test]
    fn scope_labels_are_stable() {
        // Wire-format guard: the JSON `_meta.diff_filter.scope` strings
        // are consumed by agents. Don't change these casually.
        assert_eq!(DiffScope::Since("anything").label(), "since");
        assert_eq!(DiffScope::SinceBranched.label(), "since_branched");
        assert_eq!(DiffScope::ChangedOnly.label(), "changed_only");
    }

    #[test]
    fn split_nul_drops_trailing_empty() {
        assert_eq!(split_nul(b"a\0b\0"), vec!["a", "b"]);
        assert_eq!(split_nul(b""), Vec::<String>::new());
        assert_eq!(split_nul(b"only\0"), vec!["only"]);
    }
}
