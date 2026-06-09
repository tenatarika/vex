//! Phase 14.9 Tier B.7 — `--exact-presence`: resolve the exact set
//! of commits at which each historical entry's blob existed in the
//! file. Defeats the convex-hull lossy span representation
//! (LIMITATIONS §4c #4) by re-walking `git log` from HEAD,
//! batch-resolving each `<commit>:<file_path>` to its blob SHA via
//! `git cat-file --batch-check`, and intersecting against
//! `entry.blob_sha`.
//!
//! ## Walk cost
//!
//! For each unique `file_path` in the result set, runs:
//!
//! - one `git log --format=%H %cs -n N+1` (linear from HEAD, capped),
//! - one `git cat-file --batch-check` with the N commit:path lines
//!   piped on stdin.
//!
//! Two process spawns per unique file, regardless of result-set size
//! and regardless of how many entries share a file. The cap
//! `--exact-presence-max-commits N` (default 500) bounds worst-case
//! work — beyond the cap, the entry falls back to its convex-hull
//! `[first_commit_idx, last_commit_idx]` span with `truncated: true`
//! signalled to JSON consumers and an `eprintln!` notice in text mode.
//!
//! ## Limitations (v1)
//!
//! - **No rename tracking.** The walk does not pass `--follow`; if a
//!   file was renamed earlier in history, older commits report the
//!   blob at the new path as `missing` and presence stops at the
//!   rename boundary. The walker's `--follow` could be added in a
//!   later phase if rename-aware presence is requested.
//! - **Session-scoped cache, not persisted.** Re-running
//!   `--exact-presence` for the same `(file_path, blob_sha)` in a
//!   later process spawn re-walks. Persisting on disk re-introduces
//!   the canonicalize-symmetry hazard burned in Phase 14.8 Step 7
//!   (memory: `feedback_cache_path_writer_reader_symmetry`).

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::history::HistoricalSymbol;

/// One commit where a historical entry's blob exactly existed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PresenceCommit {
    pub sha: String,
    pub date: String,
}

/// Per-entry presence result. `commits` is the exact set; `walked`
/// is how deep the `git log` walked from HEAD; `truncated` is set
/// when the walk hit the `--exact-presence-max-commits` cap and the
/// caller should fall back to the convex-hull span.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EntryPresence {
    pub commits: Vec<PresenceCommit>,
    pub truncated: bool,
    pub walked: usize,
}

/// Resolve exact presence for every entry in `rows`. The returned
/// vector is parallel to `rows`. On any per-file git failure the
/// affected entries return `EntryPresence::default()` (empty +
/// truncated=false + walked=0) so a partial failure doesn't bubble
/// up and kill the whole query.
pub fn resolve(
    root: &Path,
    rows: &[HistoricalSymbol],
    max_commits: usize,
) -> Result<Vec<EntryPresence>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // Group row indices by file_path so we walk each file once.
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, r) in rows.iter().enumerate() {
        by_file.entry(r.file_path.as_str()).or_default().push(idx);
    }

    let mut out: Vec<EntryPresence> = vec![EntryPresence::default(); rows.len()];

    // 1. List commits + dates from HEAD ONCE, cap+1 so we can detect
    //    overflow without a second probe. `git log` here takes no
    //    path filter — the commit list is shared across every
    //    file_path in the result set (round-2 review fix; previously
    //    this ran inside the loop and spawned N identical processes).
    let (commits, truncated) = match git_log_with_dates(root, max_commits) {
        Ok(v) => v,
        Err(_) => return Ok(out), // partial failure — leave defaults everywhere
    };
    let walked = commits.len();

    if truncated {
        for ep in &mut out {
            ep.truncated = true;
            ep.walked = walked;
        }
        return Ok(out);
    }

    for (file_path, row_indices) in &by_file {
        // 2. Batched cat-file --batch-check to resolve each commit's
        //    blob at file_path. Missing entries (file deleted/renamed)
        //    drop out of the map.
        let blob_by_commit = match git_cat_file_batch_check(root, file_path, &commits) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // 3. Per entry, filter commits whose resolved blob equals
        //    entry.blob_sha.
        for &i in row_indices {
            let entry_blob = rows[i].blob_sha.as_str();
            let presence: Vec<PresenceCommit> = commits
                .iter()
                .filter_map(|(sha, date)| {
                    let resolved = blob_by_commit.get(sha.as_str())?;
                    if resolved == entry_blob {
                        Some(PresenceCommit {
                            sha: sha.clone(),
                            date: date.clone(),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            out[i].commits = presence;
            out[i].walked = walked;
            out[i].truncated = false;
        }
    }

    Ok(out)
}

fn git_log_with_dates(root: &Path, max: usize) -> Result<(Vec<(String, String)>, bool)> {
    let cap_arg = format!("-n{}", max + 1);
    let out = Command::new("git")
        .args(["log", "--format=%H %cs", &cap_arg])
        .current_dir(root)
        .output()
        .context("git log for --exact-presence")?;
    if !out.status.success() {
        bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let lines: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            let sha = parts.next()?.to_string();
            let date = parts.next()?.to_string();
            if sha.is_empty() {
                return None;
            }
            Some((sha, date))
        })
        .collect();
    let truncated = lines.len() > max;
    let commits = lines.into_iter().take(max).collect();
    Ok((commits, truncated))
}

fn git_cat_file_batch_check(
    root: &Path,
    file_path: &str,
    commits: &[(String, String)],
) -> Result<HashMap<String, String>> {
    let mut child = Command::new("git")
        .args(["cat-file", "--batch-check"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn git cat-file --batch-check")?;

    {
        let stdin = child.stdin.as_mut().context("git cat-file stdin")?;
        for (sha, _) in commits {
            writeln!(stdin, "{sha}:{file_path}").context("write cat-file stdin")?;
        }
    }

    let out = child.wait_with_output().context("wait git cat-file")?;
    if !out.status.success() {
        bail!("git cat-file --batch-check exited non-zero");
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut by_commit = HashMap::new();
    for (line, (sha, _)) in stdout.lines().zip(commits.iter()) {
        // Output per input line is either:
        //   "<blob_sha> blob <size>"  on hit
        //   "<input> missing"         on hit-but-not-a-blob / missing
        let mut parts = line.split_whitespace();
        let first = parts.next().unwrap_or("");
        let second = parts.next().unwrap_or("");
        if second == "blob" {
            by_commit.insert(sha.clone(), first.to_string());
        }
    }
    Ok(by_commit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as Cmd;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        for args in [
            ["init", "-q", "-b", "main"].as_slice(),
            ["config", "user.email", "t@example.com"].as_slice(),
            ["config", "user.name", "T"].as_slice(),
            ["config", "commit.gpgsign", "false"].as_slice(),
        ] {
            Cmd::new("git")
                .args(args)
                .current_dir(tmp.path())
                .status()
                .unwrap();
        }
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

    fn blob_sha(repo: &Path, commit_sha: &str, file: &str) -> String {
        let out = Cmd::new("git")
            .args(["rev-parse", &format!("{commit_sha}:{file}")])
            .current_dir(repo)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn head_sha(repo: &Path) -> String {
        let out = Cmd::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn sym(commit: &str, blob: &str, file: &str) -> HistoricalSymbol {
        HistoricalSymbol {
            commit_sha: commit.into(),
            commit_date: "2026-06-09".into(),
            author: "T".into(),
            file_path: file.into(),
            blob_sha: blob.into(),
            line: 1,
            signature: "fn f()".into(),
            kind: "function".into(),
        }
    }

    #[test]
    fn revert_pattern_excludes_middle_commit() {
        // C1: introduce content A → blob_A
        // C2: change to content B → blob_B
        // C3: revert to content A → blob_A again
        // For row referencing blob_A, exact-presence should report
        // {C1, C3} but NOT C2.
        let repo = init_repo();
        commit(repo.path(), "lib.rs", "fn f() -> u8 { 1 }\n", "v1 = A");
        let c1 = head_sha(repo.path());
        let blob_a = blob_sha(repo.path(), &c1, "lib.rs");

        commit(repo.path(), "lib.rs", "fn f() -> u8 { 2 }\n", "v2 = B");
        let _c2 = head_sha(repo.path());

        commit(
            repo.path(),
            "lib.rs",
            "fn f() -> u8 { 1 }\n",
            "v3 = revert to A",
        );
        let c3 = head_sha(repo.path());

        let row_a = sym(&c3, &blob_a, "lib.rs");

        let presence = resolve(repo.path(), &[row_a], 100).unwrap();
        assert_eq!(presence.len(), 1);
        let p = &presence[0];
        assert!(!p.truncated);
        assert_eq!(p.walked, 3);

        let shas: Vec<&str> = p.commits.iter().map(|c| c.sha.as_str()).collect();
        assert!(shas.contains(&c1.as_str()), "C1 should be present");
        assert!(shas.contains(&c3.as_str()), "C3 (revert) should be present");
        assert_eq!(p.commits.len(), 2, "C2 must NOT appear: got {p:?}");
    }

    #[test]
    fn distinct_blobs_isolate_presence_sets() {
        let repo = init_repo();
        commit(repo.path(), "lib.rs", "fn f() -> u8 { 1 }\n", "v1");
        let c1 = head_sha(repo.path());
        let blob_a = blob_sha(repo.path(), &c1, "lib.rs");
        commit(repo.path(), "lib.rs", "fn f() -> u8 { 2 }\n", "v2");
        let c2 = head_sha(repo.path());
        let blob_b = blob_sha(repo.path(), &c2, "lib.rs");

        let rows = vec![sym(&c2, &blob_b, "lib.rs"), sym(&c1, &blob_a, "lib.rs")];
        let presence = resolve(repo.path(), &rows, 100).unwrap();
        assert_eq!(presence[0].commits.len(), 1);
        assert_eq!(presence[0].commits[0].sha, c2);
        assert_eq!(presence[1].commits.len(), 1);
        assert_eq!(presence[1].commits[0].sha, c1);
    }

    #[test]
    fn cap_triggers_truncation_signal() {
        let repo = init_repo();
        for i in 0..5 {
            commit(
                repo.path(),
                "lib.rs",
                &format!("fn f() -> u8 {{ {i} }}\n"),
                &format!("v{i}"),
            );
        }
        let c4 = head_sha(repo.path());
        let blob_4 = blob_sha(repo.path(), &c4, "lib.rs");

        let row = sym(&c4, &blob_4, "lib.rs");
        // Cap below the actual depth → truncation fires.
        let presence = resolve(repo.path(), &[row], 2).unwrap();
        assert!(
            presence[0].truncated,
            "should signal truncation at cap < depth"
        );
        assert!(presence[0].commits.is_empty());
        // `walked` reflects the post-cap commit count (`max` = 2), not the
        // raw `max+1` overflow probe. Truncation is the load-bearing signal.
        assert_eq!(presence[0].walked, 2);
    }

    #[test]
    fn empty_rows_returns_empty() {
        let repo = init_repo();
        let presence = resolve(repo.path(), &[], 100).unwrap();
        assert!(presence.is_empty());
    }
}
