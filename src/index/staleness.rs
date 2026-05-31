use std::path::Path;

use super::hasher::content_hash;
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
        Some(ts) if deep => check_mtime(root, ts, manifest),
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
///
/// H11 — when a file's `mtime > indexed_at` we verify against the
/// manifest's recorded `content_hash` before counting it as changed.
/// `touch foo.rs`, `git checkout`, `rustfmt` no-op passes, `rsync --times`,
/// and `git rebase` all touch mtimes without changing content; pre-H11
/// the mtime trigger alone produced spurious `Stale` results that
/// triggered redundant auto-rebuilds on every `vex search`.
///
/// Falls back to the mtime signal when:
/// - the path has no manifest entry (genuinely new file), or
/// - reading / hashing the file fails (treat as changed, conservative).
fn check_mtime(root: &Path, indexed_at: u64, manifest: &Manifest) -> Freshness {
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
        let Ok(meta) = path.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime <= indexed_time {
            continue;
        }
        // H11: mtime trigger only — verify against manifest's content hash
        // before counting. Mismatch / unknown / read-fail = stale.
        let rel = match crate::util::paths::to_rel_posix(path, root) {
            Some(r) => r,
            None => {
                // Symlink / non-prefix path — conservative: stale.
                changed += 1;
                continue;
            }
        };
        let Some(recorded_hash) = manifest.files.get(&rel).copied() else {
            // New file not in the manifest — stale (the index doesn't
            // know about it yet).
            changed += 1;
            continue;
        };
        match std::fs::read(path) {
            Ok(bytes) if content_hash(&bytes) == recorded_hash => {
                // Hash matches: file was touched but content is unchanged.
                // Do not count as changed.
            }
            _ => {
                changed += 1;
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

    // ---- H11: content-hash verification of mtime-triggered stale signal ----

    use crate::index::hasher::content_hash;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    /// Build a tiny tempdir with one supported-extension file whose content
    /// hash is registered in the manifest. `mtime_offset_secs` is the delta
    /// applied to the file's mtime relative to the manifest `indexed_at`
    /// timestamp — positive = file looks newer than the manifest.
    fn write_fixture(content: &str, mtime_offset_secs: i64) -> (TempDir, Manifest) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("foo.rs");
        fs::write(&path, content).unwrap();

        let indexed_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Force mtime relative to `indexed_at` for the deterministic check.
        // `set_modified` on macOS / Linux accepts arbitrary SystemTime.
        let target = if mtime_offset_secs >= 0 {
            SystemTime::UNIX_EPOCH + Duration::from_secs(indexed_at + mtime_offset_secs as u64)
        } else {
            SystemTime::UNIX_EPOCH + Duration::from_secs(indexed_at - (-mtime_offset_secs) as u64)
        };
        fs::File::open(&path).unwrap().set_modified(target).unwrap();

        let mut manifest = Manifest {
            indexed_at: Some(indexed_at),
            ..Default::default()
        };
        manifest
            .files
            .insert("foo.rs".to_string(), content_hash(content.as_bytes()));
        (tmp, manifest)
    }

    #[test]
    fn h11_touch_without_content_change_is_fresh() {
        // The file's mtime is *newer* than `indexed_at` (simulates `touch`,
        // `git checkout`, `rustfmt` no-op pass), but the content hash
        // matches what's in the manifest. Pre-H11 this triggered a
        // spurious `Stale` result; post-H11 the hash verification rules
        // it out.
        let (tmp, manifest) = write_fixture("fn main() {}\n", 60);
        let result = check_mtime(tmp.path(), manifest.indexed_at.unwrap(), &manifest);
        assert!(
            matches!(result, Freshness::Fresh),
            "matching content hash + newer mtime must be Fresh, got: {result:?}"
        );
    }

    #[test]
    fn h11_real_content_change_is_stale() {
        // Set up the manifest hash for the ORIGINAL content, then mutate
        // the file. mtime is naturally newer; hash now differs → Stale.
        let (tmp, mut manifest) = write_fixture("fn main() {}\n", 60);
        // Overwrite manifest's recorded hash with the original-content
        // hash explicitly so the second write actually diverges.
        manifest
            .files
            .insert("foo.rs".to_string(), content_hash(b"fn main() {}\n"));
        fs::write(tmp.path().join("foo.rs"), "fn main() { println!(); }\n").unwrap();
        // After write the mtime is "now"; bump it past indexed_at by 60s
        // so the mtime trigger fires for sure.
        let target =
            SystemTime::UNIX_EPOCH + Duration::from_secs(manifest.indexed_at.unwrap() + 60);
        fs::File::open(tmp.path().join("foo.rs"))
            .unwrap()
            .set_modified(target)
            .unwrap();

        let result = check_mtime(tmp.path(), manifest.indexed_at.unwrap(), &manifest);
        assert!(
            matches!(result, Freshness::Stale { .. }),
            "diverged content hash must be Stale, got: {result:?}"
        );
    }

    #[test]
    fn h11_unknown_file_falls_back_to_mtime_signal() {
        // File present on disk and newer than indexed_at, but its path
        // is NOT in the manifest — this is a genuinely new file that
        // appeared after the last index. Stale is the correct answer
        // (the index doesn't know about it).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("new.rs");
        fs::write(&path, "fn new() {}\n").unwrap();
        let indexed_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let target = SystemTime::UNIX_EPOCH + Duration::from_secs(indexed_at + 60);
        fs::File::open(&path).unwrap().set_modified(target).unwrap();
        let manifest = Manifest {
            indexed_at: Some(indexed_at),
            ..Default::default()
        };

        let result = check_mtime(tmp.path(), indexed_at, &manifest);
        assert!(
            matches!(result, Freshness::Stale { .. }),
            "new file (not in manifest) must trigger Stale, got: {result:?}"
        );
    }
}
