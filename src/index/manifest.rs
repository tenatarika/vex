use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Manifest tracks file hashes for incremental indexing.
/// Stored as JSON next to the index file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    /// Map of relative file path → content hash
    pub files: HashMap<String, u64>,

    /// Git HEAD commit hash at index time (None for non-git repos)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,

    /// Unix timestamp (seconds since epoch) when the index was written
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<u64>,

    /// Stable identifier of the embedder used to build the semantic index
    /// (e.g. `"minilm-l6-v2"`). `None` when the index has no embeddings, or
    /// for pre-9.1 manifests that did not record this field — readers
    /// interpret `None` as the default embedder for back-compat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedder_id: Option<String>,

    /// Whether this index was built with the persistent call-graph section.
    /// `Some(false)` means the user opted out via `--no-call-graph` or
    /// `.vex.toml`; `vex update` reads this to keep the opt-out sticky
    /// across incremental rebuilds. `None` on pre-10.3 manifests is treated
    /// as enabled (pre-flag behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_graph: Option<bool>,

    /// Whether this index was built with the BM25 channel. Same semantics
    /// as `call_graph` — opt-out is sticky across updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bm25: Option<bool>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path).context("read manifest")?;
        serde_json::from_str(&data).context("parse manifest")
    }

    /// Atomic write: write to .tmp, then rename to avoid corruption on crash.
    pub fn save(&self, path: &Path) -> Result<()> {
        let data = serde_json::to_string(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, data).context("write manifest tmp")?;
        std::fs::rename(&tmp, path).context("rename manifest")?;
        Ok(())
    }
}

/// Determine which files need re-indexing.
pub struct DiffResult {
    /// Files that are new or changed (need parsing)
    pub changed: Vec<String>,
    /// Files that were deleted (need removal)
    pub deleted: Vec<String>,
    /// Files that are unchanged (skip)
    pub unchanged: usize,
}

/// Compare current filesystem state against the stored manifest.
pub fn diff_files(
    current_files: &[(String, u64)], // (rel_path, content_hash)
    old_manifest: &Manifest,
) -> DiffResult {
    let mut changed = Vec::new();
    let mut current_set: HashMap<&str, u64> = HashMap::new();
    let mut unchanged = 0;

    for (path, hash) in current_files {
        current_set.insert(path.as_str(), *hash);
        match old_manifest.files.get(path) {
            Some(old_hash) if *old_hash == *hash => {
                unchanged += 1;
            }
            _ => {
                changed.push(path.clone());
            }
        }
    }

    let deleted: Vec<String> = old_manifest
        .files
        .keys()
        .filter(|p| !current_set.contains_key(p.as_str()))
        .cloned()
        .collect();

    DiffResult {
        changed,
        deleted,
        unchanged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_new_files() {
        let old = Manifest::default();
        let current = vec![("src/main.rs".to_string(), 123u64)];
        let diff = diff_files(&current, &old);
        assert_eq!(diff.changed, vec!["src/main.rs"]);
        assert!(diff.deleted.is_empty());
        assert_eq!(diff.unchanged, 0);
    }

    #[test]
    fn detects_changed_files() {
        let mut old = Manifest::default();
        old.files.insert("src/main.rs".to_string(), 100);
        let current = vec![("src/main.rs".to_string(), 200u64)];
        let diff = diff_files(&current, &old);
        assert_eq!(diff.changed, vec!["src/main.rs"]);
        assert_eq!(diff.unchanged, 0);
    }

    #[test]
    fn detects_deleted_files() {
        let mut old = Manifest::default();
        old.files.insert("src/old.rs".to_string(), 100);
        let current: Vec<(String, u64)> = vec![];
        let diff = diff_files(&current, &old);
        assert!(diff.changed.is_empty());
        assert_eq!(diff.deleted, vec!["src/old.rs"]);
    }

    #[test]
    fn unchanged_files_counted() {
        let mut old = Manifest::default();
        old.files.insert("src/main.rs".to_string(), 100);
        let current = vec![("src/main.rs".to_string(), 100u64)];
        let diff = diff_files(&current, &old);
        assert!(diff.changed.is_empty());
        assert!(diff.deleted.is_empty());
        assert_eq!(diff.unchanged, 1);
    }
}
