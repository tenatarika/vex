use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;
use regex::Regex;

pub mod trigram;

/// A single grep match in a file.
#[derive(Debug, Clone)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Search file contents by regex pattern. Parallel scan, no index needed.
pub fn search(
    root: &Path,
    pattern: &str,
    filter_path: Option<&str>,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<GrepMatch>> {
    let re = Regex::new(pattern).context("invalid regex pattern")?;
    let files = discover_files(root, filter_path, excludes)?;

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

fn discover_files(
    root: &Path,
    filter_path: Option<&str>,
    excludes: &[String],
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

        // Skip files > 1 MB
        if std::fs::metadata(&path).is_ok_and(|m| m.len() <= 1_048_576) {
            files.push(path);
        }
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
