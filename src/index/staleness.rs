use std::path::Path;

use super::manifest::Manifest;

/// Result of a staleness check.
#[derive(Debug)]
#[must_use]
pub enum Freshness {
    /// Index is current.
    Fresh,
    /// Index is stale. `changed_count` is `None` when stale but count not computed
    /// (e.g. HEAD changed), `Some(n)` when n files are known dirty.
    Stale { changed_count: Option<usize> },
    /// Cannot determine (no manifest metadata).
    Unknown,
}

/// Cheap staleness check. Git shortcut first, mtime fallback second.
///
/// When `deep` is true, runs the expensive dirty-tree check (git diff-index +
/// ls-files) to count changed files. When false, only compares HEAD — a single
/// subprocess call.
pub fn check(root: &Path, manifest: &Manifest, deep: bool) -> Freshness {
    if let Some(ref saved_head) = manifest.git_head {
        if let Some(f) = check_git(root, saved_head, deep) {
            return f;
        }
    }
    match manifest.indexed_at {
        Some(ts) if deep => check_mtime(root, ts),
        Some(_) => {
            // Shallow check: skip mtime walk, trust git or report Unknown
            Freshness::Unknown
        }
        None => Freshness::Unknown,
    }
}

/// Read the current git HEAD commit hash, or None if not a git repo.
pub fn read_git_head(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if head.is_empty() {
        None
    } else {
        Some(head)
    }
}

/// Git-based staleness check. Returns None if git is inconclusive.
fn check_git(root: &Path, saved_head: &str, deep: bool) -> Option<Freshness> {
    let current_head = read_git_head(root)?;
    if current_head != saved_head {
        return Some(Freshness::Stale {
            changed_count: None,
        });
    }
    if !deep {
        // HEAD matches — without deep check, assume fresh (single subprocess)
        return Some(Freshness::Fresh);
    }
    // HEAD matches — run expensive dirty-tree check
    let dirty = git_dirty_count(root)?;
    if dirty == 0 {
        Some(Freshness::Fresh)
    } else {
        Some(Freshness::Stale {
            changed_count: Some(dirty),
        })
    }
}

/// Count dirty files (tracked modified + untracked non-ignored).
fn git_dirty_count(root: &Path) -> Option<usize> {
    let tracked = std::process::Command::new("git")
        .args(["diff-index", "--name-only", "HEAD"])
        .current_dir(root)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let tracked_count = String::from_utf8_lossy(&tracked.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .count();

    let untracked = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(root)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let untracked_count = String::from_utf8_lossy(&untracked.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .count();

    Some(tracked_count + untracked_count)
}

/// mtime-based staleness check for non-git repos.
fn check_mtime(root: &Path, indexed_at: u64) -> Freshness {
    let indexed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(indexed_at);
    let mut changed = 0usize;

    let walker = match crate::util::walk::walk_builder(root, &[]) {
        Ok(w) => w,
        Err(_) => return Freshness::Unknown,
    };

    for entry in walker.build().flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        // Only check files with supported extensions
        if path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(crate::parse::language::Language::from_extension)
            .is_none()
        {
            continue;
        }
        if let Ok(meta) = path.metadata() {
            if let Ok(mtime) = meta.modified() {
                if mtime > indexed_time {
                    changed += 1;
                }
            }
        }
    }

    if changed == 0 {
        Freshness::Fresh
    } else {
        Freshness::Stale {
            changed_count: Some(changed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_when_no_metadata() {
        let manifest = Manifest::default();
        assert!(matches!(
            check(Path::new("."), &manifest, false),
            Freshness::Unknown
        ));
    }

    #[test]
    fn read_git_head_returns_some_in_git_repo() {
        let head = read_git_head(Path::new("."));
        assert!(head.is_some(), "should read HEAD in a git repo");
        let h = head.unwrap();
        assert!(
            h.len() == 40 || h.len() == 64,
            "HEAD should be a 40 or 64-char hex SHA, got {}-char",
            h.len()
        );
    }

    #[test]
    fn stale_when_head_differs() {
        let manifest = Manifest {
            git_head: Some("0000000000000000000000000000000000000000".to_string()),
            indexed_at: None,
            ..Default::default()
        };
        let result = check(Path::new("."), &manifest, false);
        assert!(
            matches!(result, Freshness::Stale { .. }),
            "should be stale when HEAD differs"
        );
    }

    #[test]
    fn shallow_check_skips_dirty_count() {
        // With deep=false and matching HEAD, should return Fresh without spawning
        // the expensive diff-index/ls-files commands
        let head = read_git_head(Path::new("."));
        if head.is_none() {
            return;
        }
        let manifest = Manifest {
            git_head: head,
            indexed_at: None,
            ..Default::default()
        };
        let result = check(Path::new("."), &manifest, false);
        assert!(
            matches!(result, Freshness::Fresh),
            "shallow check with matching HEAD should be Fresh"
        );
    }

    #[test]
    fn deep_check_with_matching_head() {
        let head = read_git_head(Path::new("."));
        if head.is_none() {
            return;
        }
        let manifest = Manifest {
            git_head: head,
            indexed_at: None,
            ..Default::default()
        };
        // deep=true runs dirty check; result depends on working tree state
        let result = check(Path::new("."), &manifest, true);
        assert!(
            matches!(result, Freshness::Fresh | Freshness::Stale { .. }),
            "deep check should be Fresh or Stale, not Unknown"
        );
    }
}
