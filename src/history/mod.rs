//! v1.16 — `vex history <Symbol>`: query-time git-log walker that
//! finds every historical version of a named symbol.
//!
//! ## Design
//!
//! 1. Locate every file at the chosen tip (`HEAD` by default, or
//!    `--branch <X>`) that currently mentions the symbol name. `git
//!    grep -l --word-regexp` is the cheap probe — substring noise
//!    (mention in a comment / string literal) is filtered out later
//!    by parsing.
//! 2. For each file, walk `git log --follow` up to `depth` commits.
//! 3. For each `(commit, file)` pair fetch the blob SHA via `git
//!    ls-tree`, dedupe by blob SHA (same content across consecutive
//!    commits = one entry), and parse the blob with the same
//!    [`extract_symbols_and_imports`] vex uses at index time.
//! 4. Keep only symbols whose `name` matches the query — that's the
//!    final dedup boundary (a file mentioning the name in a comment
//!    or string literal yields zero matches, no false positives).
//!
//! No index reader dependency: the walker shells out to `git` and
//! parses blobs in memory. Works even when `vex index` has never
//! been run on the repo, at the cost of being slower per query than
//! it would be with an indexed history (Phase 14.7-built `git_history`
//! section; deferred to v1.18+).
//!
//! ## Limitations (v1)
//!
//! - **Only symbols whose name still appears at the chosen tip** are
//!   surfaced. If a function existed historically but its name no
//!   longer appears anywhere in the tree (deleted file, renamed
//!   symbol), the walker doesn't find it.
//! - **Substring-overlap matches** (function `parse` and another
//!   `parse_json` in the same file) are de-noised by the post-parse
//!   `name == query` exact filter — but the candidate-file set comes
//!   from `git grep --word-regexp`, which still picks up
//!   comments / string literals that happen to mention the name.
//! - **Rename-aware?** `git log --follow` handles file renames; symbol
//!   renames inside a file are not tracked (each rename surfaces as
//!   a removal of the old name + introduction of the new — query
//!   either separately).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::parse::extractor::extract_symbols_and_imports;
use crate::parse::language::Language;

pub mod diff;
pub mod filter;
pub mod presence;

pub use filter::{parse_iso_date, HistoryFilter};
pub use presence::{resolve as resolve_exact_presence, EntryPresence};

/// One historical occurrence of the requested symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalSymbol {
    /// Full commit SHA the entry was observed at.
    pub commit_sha: String,
    /// `%cs` — short ISO commit date (`YYYY-MM-DD`).
    pub commit_date: String,
    /// `%an` — author name.
    pub author: String,
    /// Repo-relative POSIX path of the file at that commit.
    pub file_path: String,
    /// SHA of the file blob at that commit. The walker dedupes on
    /// this — two consecutive commits with identical file content
    /// surface as one entry (commit = the newer one).
    pub blob_sha: String,
    /// 1-based line number of the symbol's definition in the blob.
    pub line: u32,
    /// Signature line (first line of the def). May be empty if the
    /// parser failed to extract one.
    pub signature: String,
    /// Symbol kind label — `function`, `struct`, etc. Lowercase
    /// for stable wire-format use.
    pub kind: String,
}

/// Options for [`find_symbol_history`]. Owned-by-borrow so callers
/// can pass references straight through.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryOpts<'a> {
    /// Stop walking after this many commits **per file**. `None`
    /// walks the entire history (slow on long-lived repos).
    pub depth: Option<usize>,
    /// Restrict log walk to this revision (`refs/heads/foo`,
    /// `origin/main`, a SHA). `None` uses the current `HEAD`.
    pub branch: Option<&'a str>,
    /// Cap the total result set. The walker stops as soon as the
    /// limit is hit (does not finish the file).
    pub limit: Option<usize>,
}

/// Walk every historical version of `symbol_name` reachable from
/// `opts.branch` (or `HEAD`). Results are ordered newest-first per
/// file, files in the order `git grep -l` returned them.
pub fn find_symbol_history(
    root: &Path,
    symbol_name: &str,
    opts: &HistoryOpts<'_>,
) -> Result<Vec<HistoricalSymbol>> {
    if symbol_name.is_empty() {
        bail!("symbol name must be non-empty");
    }

    ensure_git_worktree(root)?;
    let revision = opts.branch.unwrap_or("HEAD");

    let candidate_files = git_grep_files(root, revision, symbol_name)?;
    if candidate_files.is_empty() {
        return Ok(Vec::new());
    }

    let limit = opts.limit.unwrap_or(usize::MAX);
    let mut out: Vec<HistoricalSymbol> = Vec::new();
    let mut seen_blobs: HashSet<String> = HashSet::new();

    'files: for file in &candidate_files {
        // Language detection is path-based — files we can't parse get
        // skipped silently (the user gets one entry-per-language-vex-knows
        // rather than spurious errors).
        let lang = match path_language(file) {
            Some(l) => l,
            None => continue,
        };

        let commits = git_log_follow(root, revision, file, opts.depth)?;
        for commit in commits {
            let Some(blob_sha) = git_ls_tree_blob(root, &commit.sha, file)? else {
                continue;
            };
            if !seen_blobs.insert(blob_sha.clone()) {
                continue;
            }
            let content = match git_cat_file_blob(root, &blob_sha) {
                Ok(c) => c,
                Err(_) => continue, // unreadable blob (corrupt / submodule) — skip silently
            };

            let (symbols, _imports) = match extract_symbols_and_imports(&content, lang) {
                Ok(s) => s,
                Err(_) => continue, // parser fail on historical version — skip
            };

            for sym in symbols.iter().filter(|s| s.name == symbol_name) {
                out.push(HistoricalSymbol {
                    commit_sha: commit.sha.clone(),
                    commit_date: commit.date.clone(),
                    author: commit.author.clone(),
                    file_path: file.clone(),
                    blob_sha: blob_sha.clone(),
                    line: sym.line as u32,
                    signature: sym.signature.clone().unwrap_or_default(),
                    kind: format!("{:?}", sym.kind).to_lowercase(),
                });
                if out.len() >= limit {
                    break 'files;
                }
            }
        }
    }

    Ok(out)
}

#[derive(Debug, Clone)]
struct CommitMeta {
    sha: String,
    date: String,
    author: String,
}

/// `git rev-parse --is-inside-work-tree` precheck — mirrors the
/// pattern in [`crate::vcs::GitVcs::ensure_repo`]. We don't reuse that
/// (Phase-1 diff-scope-only) backend method because history is not yet
/// routed through the `Vcs` trait and this surfaces a `vex history`-specific
/// error message (the two distinct strings the VCS design's L1 preserves).
///
/// Promoted to `pub(crate)` so the v1.17 Phase 14.8
/// `history_builder` module can reuse it without duplicating the
/// shellout (rust-reviewer SHOULD-FIX #11).
pub(crate) fn ensure_git_worktree(repo_root: &Path) -> Result<()> {
    let out = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(repo_root)
        .output()
        .context("invoke git rev-parse")?;
    if !out.status.success() || String::from_utf8_lossy(&out.stdout).trim() != "true" {
        bail!(
            "not a git repository at {} — `vex history` requires a git checkout",
            repo_root.display()
        );
    }
    Ok(())
}

/// `git grep -l --word-regexp -E <name> <revision>` — list files at
/// the given revision whose contents include the symbol name as a
/// whole word. Returns repo-relative POSIX paths.
fn git_grep_files(root: &Path, revision: &str, symbol_name: &str) -> Result<Vec<String>> {
    // Pass the symbol name as a literal pattern via `-F` and use
    // `--word-regexp` to require token boundaries — `parse` won't match
    // `parse_json`.
    let out = Command::new("git")
        .args([
            "grep",
            "--name-only",
            "--word-regexp",
            "--fixed-strings",
            "-z",
            symbol_name,
            revision,
            "--",
        ])
        .current_dir(root)
        .output()
        .context("invoke git grep")?;

    if !out.status.success() {
        // Exit code 1 means "no matches" — that's a clean empty result,
        // not an error. Any other non-zero is a real failure (bad
        // revision, permissions, ...).
        if out.status.code() == Some(1) && out.stderr.is_empty() {
            return Ok(Vec::new());
        }
        bail!(
            "git grep failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    // -z output: NUL-separated records. With `<revision>` arg the
    // format is `<revision>:<path>\0` — strip the `<revision>:` prefix.
    let prefix = format!("{revision}:");
    let mut files = Vec::new();
    for record in out.stdout.split(|&b| b == 0) {
        if record.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(record);
        let path = s.strip_prefix(&prefix).unwrap_or(s.as_ref());
        files.push(path.to_string());
    }
    Ok(files)
}

/// `git log --follow --pretty=format:%H|%cs|%an -n <depth> <revision> -- <file>`.
fn git_log_follow(
    root: &Path,
    revision: &str,
    file: &str,
    depth: Option<usize>,
) -> Result<Vec<CommitMeta>> {
    let depth_arg = depth.map(|d| format!("-n{d}"));
    let mut args: Vec<&str> = vec!["log", "--follow", "--pretty=format:%H|%cs|%an"];
    if let Some(ref d) = depth_arg {
        args.push(d);
    }
    args.push(revision);
    args.push("--");
    args.push(file);

    let out = Command::new("git")
        .args(&args)
        .current_dir(root)
        .output()
        .context("invoke git log")?;
    if !out.status.success() {
        bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut commits = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.splitn(3, '|');
        let sha = parts.next().unwrap_or("").to_string();
        let date = parts.next().unwrap_or("").to_string();
        let author = parts.next().unwrap_or("").to_string();
        if sha.is_empty() {
            continue;
        }
        commits.push(CommitMeta { sha, date, author });
    }
    Ok(commits)
}

/// `git ls-tree <commit> -- <file>` → blob SHA, or `None` if the
/// file didn't exist at that commit (deleted before reintroduction).
fn git_ls_tree_blob(root: &Path, commit: &str, file: &str) -> Result<Option<String>> {
    let out = Command::new("git")
        .args(["ls-tree", commit, "--", file])
        .current_dir(root)
        .output()
        .context("invoke git ls-tree")?;
    if !out.status.success() {
        return Ok(None);
    }
    // Format: <mode> blob <sha>\t<path>
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let _mode = parts.next();
    let kind = parts.next();
    let sha = parts.next();
    if kind == Some("blob") {
        Ok(sha.map(|s| s.to_string()))
    } else {
        Ok(None)
    }
}

/// `git cat-file blob <sha>` → file content as UTF-8 string. Binary
/// blobs surface as a `read content from git` error — caller skips.
fn git_cat_file_blob(root: &Path, blob_sha: &str) -> Result<String> {
    let out = Command::new("git")
        .args(["cat-file", "blob", blob_sha])
        .current_dir(root)
        .output()
        .context("invoke git cat-file")?;
    if !out.status.success() {
        bail!(
            "git cat-file failed for {}: {}",
            blob_sha,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn path_language(path: &str) -> Option<Language> {
    let ext = PathBuf::from(path)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())?;
    Language::from_extension(&ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as Cmd;
    use tempfile::TempDir;

    /// Build a synthetic git repo: init, configure user, return path.
    fn init_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        Cmd::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.name", "Tester"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        tmp
    }

    fn commit(repo: &Path, file: &str, content: &str, msg: &str) {
        std::fs::write(repo.join(file), content).unwrap();
        Cmd::new("git")
            .args(["add", file])
            .current_dir(repo)
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["commit", "-q", "-m", msg])
            .current_dir(repo)
            .status()
            .unwrap();
    }

    #[test]
    fn finds_two_versions_of_a_function_across_commits() {
        let repo = init_repo();
        commit(
            repo.path(),
            "lib.rs",
            "pub fn parse_payment(input: &str) -> i32 { 0 }\n",
            "v1 of parse_payment",
        );
        commit(
            repo.path(),
            "lib.rs",
            "pub fn parse_payment(input: &str) -> Result<i32, ()> { Ok(0) }\n",
            "v2 of parse_payment",
        );

        let opts = HistoryOpts::default();
        let history = find_symbol_history(repo.path(), "parse_payment", &opts).unwrap();
        assert_eq!(
            history.len(),
            2,
            "must find both versions of parse_payment, got {history:?}"
        );
        // Newest commit comes first per `git log` default order.
        assert!(history[0].signature.contains("Result"));
        assert!(history[1].signature.contains("i32"));
        assert!(history[0].blob_sha != history[1].blob_sha);
    }

    #[test]
    fn deduplicates_consecutive_commits_with_identical_blob() {
        let repo = init_repo();
        // Commit the same file content twice (touched but unchanged) —
        // the second commit must not produce a second HistoricalSymbol.
        commit(
            repo.path(),
            "lib.rs",
            "pub fn stable() -> u8 { 7 }\n",
            "initial",
        );
        // Force a no-op commit by adding an unrelated file (the blob
        // SHA of lib.rs stays the same).
        commit(repo.path(), "README.md", "hello\n", "unrelated");

        let history = find_symbol_history(repo.path(), "stable", &HistoryOpts::default()).unwrap();
        assert_eq!(
            history.len(),
            1,
            "blob-SHA dedup must collapse identical content across commits, got {history:?}"
        );
    }

    #[test]
    fn returns_empty_when_symbol_never_existed() {
        let repo = init_repo();
        commit(
            repo.path(),
            "lib.rs",
            "pub fn unrelated() -> u8 { 0 }\n",
            "no parse_payment here",
        );
        let history =
            find_symbol_history(repo.path(), "parse_payment", &HistoryOpts::default()).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn limit_cuts_results_short() {
        let repo = init_repo();
        for i in 0..5 {
            commit(
                repo.path(),
                "lib.rs",
                &format!("pub fn foo() -> u8 {{ {i} }}\n"),
                &format!("v{i}"),
            );
        }
        let opts = HistoryOpts {
            limit: Some(2),
            ..Default::default()
        };
        let history = find_symbol_history(repo.path(), "foo", &opts).unwrap();
        assert_eq!(history.len(), 2, "limit must cap the result set");
    }

    #[test]
    fn errors_on_non_git_dir() {
        let tmp = TempDir::new().unwrap();
        let err = find_symbol_history(tmp.path(), "anything", &HistoryOpts::default()).unwrap_err();
        assert!(err.to_string().contains("not a git repository"));
    }

    #[test]
    fn empty_symbol_name_rejected() {
        let repo = init_repo();
        commit(repo.path(), "lib.rs", "fn foo() {}\n", "init");
        let err = find_symbol_history(repo.path(), "", &HistoryOpts::default()).unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn word_regexp_filters_substring_overlap() {
        // `parse_json` mentions `parse` but only `parse_payment` is
        // the target — must not be falsely listed.
        let repo = init_repo();
        commit(
            repo.path(),
            "lib.rs",
            "pub fn parse_payment() -> u8 { 0 }\npub fn parse_json() -> u8 { 0 }\n",
            "two funcs",
        );
        let history =
            find_symbol_history(repo.path(), "parse_payment", &HistoryOpts::default()).unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].signature.contains("parse_payment"));
    }
}
