use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;
use regex::Regex;

pub mod trigram;

use crate::store::trigram as store_trigram;
use crate::store::trigram::TrigramRecord;
use crate::util::config;
use trigram::{Trigram, TrigramBloom};

/// A single grep match in a file.
#[derive(Debug, Clone)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Search file contents by regex pattern. Parallel scan.
///
/// When an `index.trigram` sidecar is present and the pattern yields a
/// required literal, files whose bloom provably can't contain that literal
/// are skipped before they're read (see [`TrigramSkip`]). Absent sidecar,
/// non-literal pattern, or a stale record → the file is read as before, so
/// the result set is identical to a full walk — the skip-index only trims
/// I/O, never matches.
pub fn search(
    root: &Path,
    pattern: &str,
    filter_path: Option<&str>,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<GrepMatch>> {
    let re = Regex::new(pattern).context("invalid regex pattern")?;
    let skip = TrigramSkip::build(root, pattern);
    let files = discover_files(root, filter_path, excludes, skip.as_ref())?;

    let matches: Vec<GrepMatch> = files
        .par_iter()
        .flat_map(|path| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(), // binary or unreadable — skip silently
            };

            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            let mut file_matches = Vec::new();
            for (line_num, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    file_matches.push(GrepMatch {
                        path: rel.clone(),
                        line: line_num + 1,
                        text: line.trim().to_string(),
                    });
                }
            }
            file_matches
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

/// The `index.trigram` skip-index paired with the current pattern's
/// required trigrams. `can_skip` decides — per file, from metadata the
/// walk already fetched — whether the file provably cannot match and can
/// be left unread.
///
/// **No false negatives.** A file is skipped ONLY when it has a sidecar
/// record whose `(len, mtime)` still matches the file on disk AND whose
/// bloom lacks one of the required trigrams. Any other case (no record,
/// stale record, un-keyable path, stat failure) reads the file. See
/// `docs/GREP-TRIGRAM.md`.
struct TrigramSkip {
    /// Trigrams the pattern's literal must contain (non-empty by
    /// construction — `required_trigrams` returns `None` for < 3 bytes).
    required: Vec<Trigram>,
    index: HashMap<String, TrigramRecord>,
}

impl TrigramSkip {
    /// Build the skip-index for `pattern`, or `None` when it can't help:
    /// the pattern has no ≥3-byte required literal, or the sidecar is
    /// absent / malformed (→ full walk, matching pre-index behaviour).
    fn build(root: &Path, pattern: &str) -> Option<Self> {
        let required = trigram::required_trigrams(pattern)?;
        if required.is_empty() {
            return None;
        }
        let records = store_trigram::load(&config::trigram_path(root)).ok()?;
        let index = records
            .into_iter()
            .map(|r| (r.rel_path.clone(), r))
            .collect();
        Some(TrigramSkip { required, index })
    }

    /// True iff `path` provably cannot match and may be left unread.
    /// `meta` is the stat the walk already performed for the size cap.
    fn can_skip(&self, path: &Path, root: &Path, meta: &std::fs::Metadata) -> bool {
        // Key must be derived exactly as the sidecar wrote it (POSIX rel),
        // else the lookup silently misses on Windows and every file reads.
        let Some(rel) = crate::util::paths::to_rel_posix(path, root) else {
            return false;
        };
        let Some(rec) = self.index.get(&rel) else {
            return false; // absent → read
        };
        // Staleness guard: grep runs without a reindex, so any drift in
        // (len, mtime) means the bloom may not reflect current content →
        // read, never skip.
        if rec.len != meta.len() {
            return false;
        }
        let Ok(mtime) = meta.modified() else {
            return false;
        };
        if (rec.mtime_secs, rec.mtime_nanos) != store_trigram::mtime_parts(mtime) {
            return false;
        }
        // Fresh + matching record: skip iff the bloom proves the required
        // literal cannot be present.
        !TrigramBloom::from_raw(rec.bloom).might_contain_all(&self.required)
    }
}

fn discover_files(
    root: &Path,
    filter_path: Option<&str>,
    excludes: &[String],
    skip: Option<&TrigramSkip>,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for entry in crate::util::walk::walk_builder(root, excludes)?.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();

        // Optional path filter
        if let Some(fp) = filter_path {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if !rel.to_string_lossy().contains(fp) {
                continue;
            }
        }

        // Single stat, reused for both the 1 MB cap and the trigram skip.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() > 1_048_576 {
            continue;
        }

        // Trigram skip-index: drop files that provably can't match before
        // they're ever read.
        if let Some(skip) = skip {
            if skip.can_skip(&path, root, &meta) {
                continue;
            }
        }

        files.push(path);
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.py"),
            "def hello():\n    print('world')\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("config.py"),
            "TIMEOUT = '40 MINUTE'\nDEBUG = True\n",
        )
        .unwrap();
        fs::create_dir(dir.path().join("api")).unwrap();
        fs::write(
            dir.path().join("api/routes.py"),
            "def get_user():\n    return user\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn grep_finds_string_in_content() {
        let dir = setup_test_dir();
        let matches = search(dir.path(), "40 MINUTE", None, 50, &[]).unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].path.contains("config.py"));
        assert_eq!(matches[0].line, 1);
    }

    #[test]
    fn grep_regex_pattern() {
        let dir = setup_test_dir();
        let matches = search(dir.path(), r"def \w+\(\)", None, 50, &[]).unwrap();
        assert_eq!(matches.len(), 2); // hello() and get_user()
    }

    #[test]
    fn grep_with_path_filter() {
        let dir = setup_test_dir();
        let matches = search(dir.path(), "def", Some("api"), 50, &[]).unwrap();
        assert_eq!(matches.len(), 1);
        // Path separators differ between Unix (`api/routes.py`) and
        // Windows (`api\routes.py`); assert on the directory name only.
        assert!(
            matches[0].path.contains("api"),
            "expected match path to contain `api`, got {:?}",
            matches[0].path
        );
    }

    #[test]
    fn grep_respects_limit() {
        let dir = setup_test_dir();
        let matches = search(dir.path(), ".", None, 2, &[]).unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn grep_invalid_regex_returns_error() {
        let dir = setup_test_dir();
        assert!(search(dir.path(), "[invalid", None, 50, &[]).is_err());
    }
}
