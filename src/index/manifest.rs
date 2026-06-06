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

    /// Whether this index was built with the v6 pattern-skeleton
    /// section (11.4 Inc 4). Same sticky-opt-out semantics as
    /// `call_graph` / `bm25`. `None` on pre-11.4 manifests is treated
    /// as enabled (the section's empty state is harmless if absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern_index: Option<bool>,

    /// `true` when this manifest was written by a full rebuild
    /// (`vex index`), `false` after `vex update` because incremental
    /// updates leave skeletons empty for unchanged files (matches the
    /// `bound_refs` / `call_edges` convention). 11.4 Inc 5's indexed
    /// `vex pattern` prefilter degrades to live-scan when this is
    /// `Some(false)` to avoid silently under-reporting matches. `None`
    /// on pre-11.4 manifests is treated as `false` (conservative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern_index_full: Option<bool>,

    /// v1.13 P5: `true` when the on-disk vectors are L2-normalized
    /// (unit length). `vex similar` / `vex duplicates` / `vex search
    /// --semantic` switch to a dot-product fast path that skips the
    /// per-call norm + sqrt. `None` / `Some(false)` on pre-1.13
    /// manifests is treated as un-normalized and the cosine-similarity
    /// path runs — guaranteed-correct, just slower. The next
    /// `vex update` (or `vex index`) normalizes everything and flips
    /// this to `Some(true)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vectors_normalized: Option<bool>,

    /// v1.14: `Some(true)` when this index was built with Pass-2 C++
    /// `#include "..."` resolution (`src/store/include_resolver.rs` BFS).
    /// `None` on pre-1.14 manifests means strict C++ cross-file refs were
    /// not produced — `vex usages --strict <symbol>` will silently
    /// under-report for C++ codebases until the next `vex index`. The
    /// flag is purely a version marker; it does not encode whether the
    /// project has any C++ files (a pure-Rust project still gets
    /// `Some(true)` because the resolver ran trivially over an empty
    /// C++ set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpp_includes_processed: Option<bool>,

    /// v1.15.0 B1.2: `Some(true)` when this index was built with the
    /// `index.bodytokens` sidecar persisted. Required so the next
    /// `vex update`'s `reconstruct_unchanged` can restore body_tokens
    /// for unchanged symbols, which keeps `context_hash` stable across
    /// fresh-parse / reconstruct boundary and enables the B1.2
    /// incremental HNSW path. `None` on pre-1.15 manifests means the
    /// sidecar isn't present — `vex update` falls back to body-less
    /// hashes and full HNSW rebuild. The flag is a version marker; a
    /// pure non-semantic index still gets `Some(true)` because the
    /// sidecar write is unconditional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_tokens_persisted: Option<bool>,
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

    #[test]
    fn cpp_includes_processed_round_trip() {
        // Writers set `Some(true)` from v1.14; serialise → parse must
        // preserve the value verbatim. Guards against an accidental
        // `skip_serializing_if = "Option::is_none"` regression that
        // would drop a populated field.
        let m = Manifest {
            cpp_includes_processed: Some(true),
            ..Manifest::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"cpp_includes_processed\":true"));
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cpp_includes_processed, Some(true));
    }

    #[test]
    fn cpp_includes_processed_defaults_none_on_pre_v1_14_manifest() {
        // A pre-1.14 manifest has no `cpp_includes_processed` key at all.
        // `#[serde(default)]` must deserialise it as `None` instead of
        // erroring — that's the back-compat contract every other
        // `Option<bool>` field already follows.
        let pre_v1_14_json = r#"{"files": {}}"#;
        let m: Manifest = serde_json::from_str(pre_v1_14_json).unwrap();
        assert_eq!(m.cpp_includes_processed, None);
    }

    #[test]
    fn cpp_includes_processed_none_is_omitted_from_serialised_form() {
        // Symmetric to the load case: when the field is `None`, the
        // serialised JSON must not contain the key. Keeps old readers
        // (pre-1.14 self-update users running on a fresh v1.14 binary
        // that decided to opt out) from seeing unfamiliar keys.
        let m = Manifest::default();
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            !json.contains("cpp_includes_processed"),
            "expected key absent for None, got: {json}"
        );
    }

    #[test]
    fn body_tokens_persisted_round_trip() {
        let m = Manifest {
            body_tokens_persisted: Some(true),
            ..Manifest::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"body_tokens_persisted\":true"));
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.body_tokens_persisted, Some(true));
    }

    #[test]
    fn body_tokens_persisted_defaults_none_on_pre_v1_15_manifest() {
        let pre_json = r#"{"files": {}}"#;
        let m: Manifest = serde_json::from_str(pre_json).unwrap();
        assert_eq!(m.body_tokens_persisted, None);
    }

    #[test]
    fn body_tokens_persisted_none_is_omitted_from_serialised_form() {
        let m = Manifest::default();
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            !json.contains("body_tokens_persisted"),
            "expected key absent for None, got: {json}"
        );
    }
}
