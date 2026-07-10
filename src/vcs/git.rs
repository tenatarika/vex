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

use std::path::Path;
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
        VcsCapabilities { merge_base: true }
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
}
