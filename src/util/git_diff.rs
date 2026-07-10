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

use anyhow::Result;

use crate::vcs::{GitVcs, Vcs, VcsError};

// `DiffScope` moved to `crate::vcs` (it is VCS-agnostic and part of the `Vcs`
// trait surface). Re-exported here so existing `crate::util::git_diff::DiffScope`
// import paths keep resolving unchanged.
pub use crate::vcs::DiffScope;

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
        // Phase 1 (docs/VCS-BACKENDS.md): git is the sole backend, constructed
        // directly. `ensure_repo` is the H3 pre-flight — `git diff` outside a
        // worktree exits 0 with help, so without it a missing-repo error would
        // leak past the success check and silently return an empty set. The
        // `Vcs` error is collapsed to `anyhow` verbatim so the existing
        // error-substring tests still hold.
        let vcs = GitVcs;
        vcs.ensure_repo(repo_root).map_err(VcsError::into_anyhow)?;
        let raw = vcs
            .changed_paths(repo_root, scope)
            .map_err(VcsError::into_anyhow)?;
        // Normalize at insertion time using the SAME function we'll use at
        // lookup time. The insertion side and lookup side must walk the
        // same normalization pipeline or the HashSet keys won't agree —
        // H7 review found the previous insert-side `normalize_posix` +
        // lookup-side bare-`contains` mismatch under-reported on Windows
        // (case-fold + UNC prefix divergence).
        let paths: HashSet<String> = raw.into_iter().map(normalize_for_lookup).collect();
        Ok(Self { paths })
    }

    /// True when `path` (in any separator flavour) is in the changed set.
    /// HashSet lookup — O(1) amortized.
    ///
    /// `path` is normalized identically to the insertion side via
    /// [`normalize_for_lookup`]: separator coalesce + (on Windows) UNC
    /// prefix strip + case-fold. NTFS is case-insensitive, so case-folding
    /// is required to match `Path::canonicalize` output against git's
    /// (always POSIX, always preserved-case) output.
    pub fn contains(&self, path: &str) -> bool {
        self.paths.contains(&normalize_for_lookup(path.to_string()))
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

/// Normalize a repo-relative (or canonicalized) path string into the
/// canonical form used as HashSet keys for membership checks.
///
/// The same function is applied to BOTH the insertion side (git output)
/// AND the lookup side (callers' index paths). Identical normalization
/// on both sides is the invariant the HashSet relies on.
///
/// Steps, in order:
/// 1. (Windows only) Strip a leading `\\?\` UNC prefix. `Path::canonicalize`
///    emits this on Windows for every absolute path; git output never
///    carries it. Without stripping, a canonicalized path will never
///    match a git-relative one.
/// 2. Convert all backslashes to forward slashes. Git output is already
///    POSIX-flavoured on every platform, but the index can store paths
///    in either form depending on the OS of the indexing host.
/// 3. (Windows only) Lowercase the entire string. NTFS is case-insensitive
///    but case-preserving; `Path::canonicalize` may flip case from the
///    user's input, while git output preserves whatever the user staged.
///    Case-folding both sides is the only way to keep the membership
///    check consistent.
///
/// On POSIX targets we only do step 2 — `/` is the native separator and
/// the filesystem is case-sensitive, so the historical behaviour
/// (just backslash → forward slash) is unchanged.
fn normalize_for_lookup(p: String) -> String {
    let stripped = strip_unc_prefix(p);
    let posix = if stripped.contains('\\') {
        stripped.replace('\\', "/")
    } else {
        stripped
    };
    case_fold(posix)
}

/// (Windows) Strip a leading `\\?\` extended-length / UNC path prefix
/// emitted by `Path::canonicalize`. On POSIX this is a no-op.
///
/// `Path::canonicalize` produces two distinct prefixes that both need to
/// go: `\\?\C:\path` for local drives and `\\?\UNC\server\share\path`
/// for network shares. The UNC variant must collapse back to `\\server\share\path`
/// (two leading slashes — the UNC root), not `server\share\path`, so callers
/// who normalize via `.replace('\\', "/")` end up with `//server/share/path`,
/// which is the POSIX-style spelling git uses internally for UNC inputs.
#[cfg(windows)]
fn strip_unc_prefix(p: String) -> String {
    const UNC_NET: &str = r"\\?\UNC\";
    const UNC_LOCAL: &str = r"\\?\";
    if let Some(rest) = p.strip_prefix(UNC_NET) {
        format!(r"\\{rest}")
    } else if let Some(rest) = p.strip_prefix(UNC_LOCAL) {
        rest.to_string()
    } else {
        p
    }
}

#[cfg(not(windows))]
#[inline]
fn strip_unc_prefix(p: String) -> String {
    p
}

/// (Windows) Lowercase ASCII letters in `p`. NTFS is case-insensitive,
/// so an inserted `src/foo.rs` must match a lookup of `SRC\Foo.rs`.
/// Stays ASCII-only to keep the comparison stable for typical source
/// trees (paths with non-ASCII identifiers will still work — they just
/// won't be case-folded across non-ASCII letters).
#[cfg(windows)]
fn case_fold(p: String) -> String {
    p.to_ascii_lowercase()
}

#[cfg(not(windows))]
#[inline]
fn case_fold(p: String) -> String {
    p
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

    /// Build a `ChangedPaths` from the listed raw paths by running each
    /// through the same insertion-side normalization that
    /// `ChangedPaths::resolve` applies. Used by the normalization tests
    /// so the insert side and lookup side go through identical pipelines.
    fn changed_paths_from(raw: impl IntoIterator<Item = &'static str>) -> ChangedPaths {
        let paths: HashSet<String> = raw
            .into_iter()
            .map(|s| normalize_for_lookup(s.to_string()))
            .collect();
        ChangedPaths { paths }
    }

    #[test]
    fn windows_separator_normalizes_on_lookup() {
        // Insert a POSIX path (git's output everywhere) and confirm a
        // Windows-style query string still finds it via the lookup-side
        // normalization. Both sides flow through `normalize_for_lookup`.
        let cp = changed_paths_from(["src/foo.rs"]);
        assert!(cp.contains("src/foo.rs"));
        assert!(cp.contains("src\\foo.rs"));
        assert!(!cp.contains("src/bar.rs"));
    }

    // ---------------------------------------------------------------
    // H7 — Windows path normalization regression guards.
    // ---------------------------------------------------------------

    #[test]
    fn posix_lookup_is_separator_and_case_preserving() {
        // POSIX behaviour is unchanged by the H7 fix: filesystem is
        // case-sensitive, so `src/Foo.rs` must NOT match a lookup of
        // `src/foo.rs` on Linux/macOS. (Windows-specific case-fold
        // covered by the dedicated `#[cfg(windows)]` test below.)
        let cp = changed_paths_from(["src/foo.rs"]);
        assert!(cp.contains("src/foo.rs"), "exact POSIX match must hit");

        #[cfg(not(windows))]
        {
            assert!(
                !cp.contains("src/Foo.rs"),
                "POSIX is case-sensitive — `Foo.rs` must NOT match `foo.rs`"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_unc_prefix_and_case_fold_match_posix_key() {
        // Simulate what `Path::canonicalize` emits on Windows: a UNC-style
        // `\\?\C:\proj\src\Foo.rs` with mixed case. After H7 normalization
        // (UNC strip + backslash → forward + lowercase), a git-side
        // insertion of `src/foo.rs` must match this lookup.
        //
        // Note this is a unit-level guard; the real canonicalize call
        // would prepend `\\?\` automatically — we inject the literal
        // prefix so the test runs on any Windows CI runner regardless
        // of the temp-dir layout.
        let cp = changed_paths_from(["c:/proj/src/foo.rs"]);
        let unc_input = r"\\?\C:\proj\src\Foo.rs";
        assert!(
            cp.contains(unc_input),
            "UNC + mixed-case lookup must match the POSIX-normalized insert key"
        );
    }

    #[test]
    fn scope_labels_are_stable() {
        // Wire-format guard: the JSON `_meta["vex.dev/diff_filter"].scope`
        // strings are consumed by agents. Don't change these casually.
        assert_eq!(DiffScope::Since("anything").label(), "since");
        assert_eq!(DiffScope::SinceBranched.label(), "since_branched");
        assert_eq!(DiffScope::ChangedOnly.label(), "changed_only");
    }
}
