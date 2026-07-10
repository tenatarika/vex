use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Hard ceiling on manifest JSON size. 128 MiB comfortably covers monorepos
/// with hundreds of thousands of files plus a dense `imported_by` graph;
/// anything larger is almost certainly hostile (the parsed structure can
/// occupy 2-3× the on-disk size in heap). Without this cap a 50 MB attacker
/// JSON can drive the process to 1-2 GB RSS before serde fails.
const MAX_MANIFEST_BYTES: u64 = 128 * 1024 * 1024;

/// Manifest tracks file hashes for incremental indexing.
/// Stored as JSON next to the index file.
///
/// INVARIANT — never add `#[serde(deny_unknown_fields)]` to this struct.
/// Pre-v1.18 manifests carried the incremental-state fields (`imported_by`,
/// `history_*`, `cpp_includes_processed`, `body_tokens_persisted`) inline at
/// the top level. Those are now nested under `state` (sidecar-persisted), so
/// on a pre-v1.18 JSON they appear as *unknown* top-level keys. Silently
/// ignoring them is the migration contract: the loader leaves `state`
/// default and the next `vex update` re-derives it (see `state` field doc).
/// `deny_unknown_fields` would turn that graceful re-bootstrap into a hard
/// load failure.
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

    /// v1.24+ grep trigram skip-index — `Some(true)` when the
    /// `index.trigram` sidecar was written successfully this build,
    /// `Some(false)` when the save was attempted but failed. `None` on
    /// pre-trigram manifests. Lives on the JSON manifest (not the bincode
    /// `state` sidecar) so adding it doesn't invalidate every existing
    /// `index.state`. Gated on the actual save outcome so `vex status`
    /// provenance matches disk — if the sidecar isn't there, this agrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigram_persisted: Option<bool>,

    /// v1.17 Phase 14.10 — `Some(true)` when the `index.rename_chains`
    /// sidecar was successfully written during this build, `Some(false)`
    /// when the write was attempted but failed (builder error, disk
    /// full, rename race). `None` when chain detection wasn't run
    /// (history not indexed, or pre-14.10 manifest). Gated on the
    /// actual sidecar write outcome so `vex status` provenance stays
    /// honest — if the sidecar's not on disk, this flag must agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename_chains_built: Option<bool>,

    /// v1.17 Phase 14.10 — count of accepted rename links where the
    /// MiniLM cosine contribution was *strictly required* to clear
    /// `GATE_SCORE`. Surfaced by `vex status` so users can see how
    /// often the semantic tie-breaker actually changed an outcome
    /// (vs. just nudging an already-passing score). `None` when chain
    /// detection didn't run or the build ran without semantic
    /// embeddings; `Some(0)` is a meaningfully different signal
    /// (cosine path active but no decisions hinged on it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename_chains_minilm_tiebreak_hits: Option<u32>,

    /// Incremental-rebuild state persisted out-of-band in the
    /// `index.state` binary sidecar (`src/index/incremental_state.rs`),
    /// NOT in this JSON manifest. Holds the reverse-import cascade map
    /// (`imported_by`), the history-section provenance (`history_*`),
    /// and the writer sentinels (`cpp_includes_processed`,
    /// `body_tokens_persisted`, `imported_by_built`). These were
    /// flattened onto `Manifest` through v1.17; v1.18 moved them to the
    /// sidecar to keep `vex update`'s JSON parse off the O(cross-file-
    /// edges) `imported_by` map. v1.21 nested them here so the loader no
    /// longer hand-shuttles each field.
    ///
    /// `#[serde(default, skip_serializing)]`: never written to JSON
    /// (the sidecar is the sole store) and absent on load until
    /// [`Manifest::load`] overlays the sidecar. On a pre-v1.18 JSON the
    /// old inline keys are ignored (see struct-level invariant) and this
    /// stays default — the next `vex update` re-derives `imported_by`
    /// via a one-time full cross-file pass.
    #[serde(default, skip_serializing)]
    pub state: crate::index::incremental_state::IncrementalState,
}

/// Counts surfaced from the `git_history` section into the manifest
/// so `vex status` and `--history-depth` integration tests can
/// observe them without parsing the binary sidecar.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryStats {
    /// Number of `Commit` rows in the sidecar — also the count of
    /// distinct commits the walker visited under any `--history-depth`
    /// cap (architect M3 global cap).
    pub commit_count: u32,
    /// Number of `Blob` rows (unique blob SHAs reachable from `tip`).
    pub blob_count: u32,
    /// Number of `HistoryEntry` rows — one per (parsed symbol, blob,
    /// path) tuple.
    pub entry_count: u32,
    /// `Some(true)` when the walker hit the `--history-depth` cap
    /// before reaching the root commit. Surfaced so `vex status` can
    /// warn that the section is partial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_capped: Option<bool>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let meta = std::fs::metadata(path).context("stat manifest")?;
        if meta.len() > MAX_MANIFEST_BYTES {
            bail!(
                "manifest {} is {} bytes, exceeds {}-byte limit (refusing to load \
                 — file may be corrupt or crafted to exhaust memory)",
                path.display(),
                meta.len(),
                MAX_MANIFEST_BYTES,
            );
        }
        let data = std::fs::read_to_string(path).context("read manifest")?;
        let mut manifest: Manifest = serde_json::from_str(&data).context("parse manifest")?;

        // v1.18 audit C1 / v1.21: overlay the binary state sidecar onto
        // the JSON-loaded manifest. The sidecar is the sole store for
        // `manifest.state`; when present its contents win. When absent
        // (pre-v1.18 index, or first build) `state` stays default — the
        // next `vex update` re-derives it. Sidecar load failures degrade
        // to "treat as absent" + tracing warn rather than failing the
        // whole load. NOTE: pre-v1.18 JSON carried these fields inline;
        // those keys are now unknown and silently ignored (see the
        // struct-level no-`deny_unknown_fields` invariant), so a stale
        // index re-bootstraps rather than surfacing dead inline values.
        let state_path = state_path_for(path);
        if state_path.exists() {
            match crate::index::incremental_state::load(&state_path) {
                Ok(state) => manifest.state = state,
                Err(e) => tracing::warn!(
                    path = %state_path.display(),
                    error = %e,
                    "index.state sidecar load failed; incremental state resets to default \
                     (next `vex update` re-derives imported_by)"
                ),
            }
        }
        Ok(manifest)
    }

    /// Atomic write: write to .tmp, then rename to avoid corruption on
    /// crash. The incremental state (`self.state`) goes to the
    /// `index.state` sidecar — it is NOT in the JSON. JSON wins the
    /// rename first; a crash between the JSON rename and the sidecar
    /// write leaves the older (or absent) sidecar. Because the sidecar
    /// is now the *sole* store for `state` (no JSON inline fallback as
    /// of v1.21), that window means a clean loss of the new state →
    /// the loader sees default `state` and the next `vex update`
    /// re-derives `imported_by` via a one-time full cross-file pass.
    /// This graceful re-bootstrap covers a *crash* (process death) only:
    /// a runtime I/O failure from the sidecar write propagates via `?`
    /// and fails the whole save, exactly as the JSON write does (the
    /// JSON manifest is already committed by that point).
    pub fn save(&self, path: &Path) -> Result<()> {
        let data = serde_json::to_string(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, data).context("write manifest tmp")?;
        std::fs::rename(&tmp, path).context("rename manifest")?;
        let state_path = state_path_for(path);
        crate::index::incremental_state::save(&state_path, &self.state)
            .context("write index.state sidecar")?;
        Ok(())
    }
}

/// Derive the `index.state` path from the manifest path. Both live in
/// the same `index_dir`, so the sidecar path is the manifest's parent
/// directory joined with the canonical filename.
fn state_path_for(manifest_path: &Path) -> std::path::PathBuf {
    match manifest_path.parent() {
        Some(dir) => dir.join("index.state"),
        None => Path::new("index.state").to_path_buf(),
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
    use crate::index::incremental_state::IncrementalState;
    use std::collections::{BTreeMap, BTreeSet};

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
    fn cpp_includes_processed_round_trip_via_save_load() {
        // v1.18 audit C1: this field now lives in `index.state`, not
        // JSON. The end-to-end contract is `Manifest::save` then `load`
        // round-trips the value through the sidecar (JSON would silently
        // drop it on save).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        let m = Manifest {
            state: IncrementalState {
                cpp_includes_processed: Some(true),
                ..Default::default()
            },
            ..Manifest::default()
        };
        m.save(&path).unwrap();
        let back = Manifest::load(&path).unwrap();
        assert_eq!(back.state.cpp_includes_processed, Some(true));
    }

    #[test]
    fn cpp_includes_processed_defaults_none_on_pre_v1_14_manifest() {
        // A pre-1.14 manifest has no `cpp_includes_processed` key at all.
        // `#[serde(default)]` must deserialise it as `None` instead of
        // erroring — that's the back-compat contract every other
        // `Option<bool>` field already follows.
        let pre_v1_14_json = r#"{"files": {}}"#;
        let m: Manifest = serde_json::from_str(pre_v1_14_json).unwrap();
        assert_eq!(m.state.cpp_includes_processed, None);
    }

    #[test]
    fn body_tokens_persisted_round_trip_via_save_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        let m = Manifest {
            state: IncrementalState {
                body_tokens_persisted: Some(true),
                ..Default::default()
            },
            ..Manifest::default()
        };
        m.save(&path).unwrap();
        let back = Manifest::load(&path).unwrap();
        assert_eq!(back.state.body_tokens_persisted, Some(true));
    }

    #[test]
    fn body_tokens_persisted_defaults_none_on_pre_v1_15_manifest() {
        let pre_json = r#"{"files": {}}"#;
        let m: Manifest = serde_json::from_str(pre_json).unwrap();
        assert_eq!(m.state.body_tokens_persisted, None);
    }

    #[test]
    fn trigram_persisted_round_trip_via_save_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        let m = Manifest {
            trigram_persisted: Some(true),
            ..Manifest::default()
        };
        m.save(&path).unwrap();
        assert_eq!(Manifest::load(&path).unwrap().trigram_persisted, Some(true));
    }

    #[test]
    fn trigram_persisted_defaults_none_on_pre_trigram_manifest() {
        // A pre-trigram manifest has no key → `#[serde(default)]` yields
        // None (not a parse error), same back-compat contract as the
        // other Option flags.
        let pre_json = r#"{"files": {}}"#;
        let m: Manifest = serde_json::from_str(pre_json).unwrap();
        assert_eq!(m.trigram_persisted, None);
    }

    #[test]
    fn history_fields_round_trip_via_save_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        let m = Manifest {
            state: IncrementalState {
                history_indexed_at: Some("2026-06-08".to_string()),
                history_tip_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
                history_depth: Some(500),
                history: Some(HistoryStats {
                    commit_count: 378,
                    blob_count: 1498,
                    entry_count: 38_999,
                    depth_capped: Some(false),
                }),
                ..Default::default()
            },
            ..Manifest::default()
        };
        m.save(&path).unwrap();
        let back = Manifest::load(&path).unwrap();
        assert_eq!(back.state.history_indexed_at.as_deref(), Some("2026-06-08"));
        assert_eq!(back.state.history_depth, Some(500));
        let stats = back.state.history.expect("history sub-object present");
        assert_eq!(stats.commit_count, 378);
        assert_eq!(stats.blob_count, 1498);
        assert_eq!(stats.entry_count, 38_999);
        assert_eq!(stats.depth_capped, Some(false));
    }

    #[test]
    fn history_fields_default_none_on_pre_v17_manifest() {
        // Pre-Phase 14.8 manifests have no `history_*` keys. All four
        // must default to None under the nested `state`.
        let pre_json = r#"{"files":{}}"#;
        let m: Manifest = serde_json::from_str(pre_json).unwrap();
        assert_eq!(m.state.history_indexed_at, None);
        assert_eq!(m.state.history_tip_sha, None);
        assert_eq!(m.state.history_depth, None);
        assert!(m.state.history.is_none());
    }

    #[test]
    fn pre_v1_18_inline_state_fields_are_ignored_and_rebootstrap() {
        // v1.21 migration contract (was `..._load_via_json_fallback`):
        // a pre-v1.18 JSON manifest carries the state fields INLINE at
        // the top level and has no `index.state` sidecar. After nesting
        // those fields under `state`, the inline keys are now *unknown*
        // top-level keys. `Manifest::load` must ignore them (no
        // `deny_unknown_fields` — see struct invariant) and leave
        // `state` default. The dead inline values do NOT surface; the
        // next `vex update` re-derives `imported_by` from scratch.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        let legacy_json = r#"{
            "files": {"src/a.rs": 7},
            "cpp_includes_processed": true,
            "body_tokens_persisted": true,
            "imported_by": {"src/a.rs": ["src/b.rs"]},
            "imported_by_built": true,
            "history_indexed_at": "2026-06-08",
            "history_tip_sha": "abc123",
            "history_depth": 250,
            "history": {
                "commit_count": 10,
                "blob_count": 20,
                "entry_count": 30,
                "depth_capped": false
            }
        }"#;
        std::fs::write(&path, legacy_json).unwrap();
        let m = Manifest::load(&path).unwrap();
        // Non-state fields still load normally.
        assert_eq!(m.files.get("src/a.rs"), Some(&7));
        // Every moved field is back to default — re-bootstrap contract.
        assert_eq!(m.state.cpp_includes_processed, None);
        assert_eq!(m.state.body_tokens_persisted, None);
        assert_eq!(m.state.imported_by_built, None);
        assert_eq!(m.state.history_indexed_at, None);
        assert_eq!(m.state.history_depth, None);
        assert!(m.state.history.is_none());
        assert!(m.state.imported_by.is_empty());
    }

    #[test]
    fn save_omits_moved_state_fields_from_json() {
        // The JSON post-v1.18 must not carry the moved fields. Anyone
        // reading manifest.json directly (debugging, tests, external
        // tooling) sees only the truly-manifest concerns: file
        // fingerprints + sticky opt-outs + non-moved sentinels.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        let m = Manifest {
            state: IncrementalState {
                cpp_includes_processed: Some(true),
                body_tokens_persisted: Some(true),
                history_indexed_at: Some("2026-06-08".to_string()),
                imported_by_built: Some(true),
                ..Default::default()
            },
            ..Manifest::default()
        };
        m.save(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        for key in [
            "cpp_includes_processed",
            "body_tokens_persisted",
            "history_indexed_at",
            "history_tip_sha",
            "history_depth",
            "imported_by",
            "imported_by_built",
            "\"history\":",
        ] {
            assert!(
                !raw.contains(key),
                "post-v1.18 JSON must not carry {key}; got: {raw}"
            );
        }
    }

    #[test]
    fn rename_chains_minilm_tiebreak_hits_round_trip() {
        // Phase 14.10 — `Some(0)` is meaningful (cosine path active,
        // nothing decided by it) and must serialise distinctly from
        // `None` (no chain detection / pre-14.10). Pin both shapes.
        let active = Manifest {
            rename_chains_minilm_tiebreak_hits: Some(0),
            ..Manifest::default()
        };
        let json = serde_json::to_string(&active).unwrap();
        assert!(json.contains("\"rename_chains_minilm_tiebreak_hits\":0"));
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rename_chains_minilm_tiebreak_hits, Some(0));

        let absent = Manifest::default();
        let json_absent = serde_json::to_string(&absent).unwrap();
        assert!(
            !json_absent.contains("rename_chains_minilm_tiebreak_hits"),
            "expected key absent for None, got: {json_absent}"
        );
    }

    #[test]
    fn rename_chains_built_round_trip() {
        // v1.17 Phase 14.10: writer records the sidecar outcome as a
        // typed boolean. Serialise → parse must preserve `Some(true)`
        // verbatim so `vex status` provenance matches reality.
        let m = Manifest {
            rename_chains_built: Some(true),
            ..Manifest::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"rename_chains_built\":true"));
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rename_chains_built, Some(true));
    }

    #[test]
    fn rename_chains_built_records_failed_write_distinctly_from_pre_v17() {
        // A v1.17 build that attempted the sidecar but failed records
        // `Some(false)`. A pre-1.17 manifest carries no key at all and
        // deserialises as `None`. These must NOT collapse to the same
        // value — `vex status` distinguishes "tried and failed, look at
        // logs" from "not run (re-index to enable)".
        let failed = Manifest {
            rename_chains_built: Some(false),
            ..Manifest::default()
        };
        let json = serde_json::to_string(&failed).unwrap();
        assert!(json.contains("\"rename_chains_built\":false"));
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rename_chains_built, Some(false));

        let pre_json = r#"{"files":{}}"#;
        let pre: Manifest = serde_json::from_str(pre_json).unwrap();
        assert_eq!(pre.rename_chains_built, None);
    }

    #[test]
    fn rename_chains_built_none_is_omitted_from_serialised_form() {
        let m = Manifest::default();
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            !json.contains("rename_chains_built"),
            "expected key absent for None, got: {json}"
        );
    }

    #[test]
    fn load_round_trip_via_save_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        let mut original = Manifest::default();
        original.files.insert("src/lib.rs".to_string(), 42);
        original.save(&path).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.files.get("src/lib.rs"), Some(&42));
    }

    #[test]
    fn load_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = Manifest::load(&tmp.path().join("absent.json")).unwrap();
        assert!(loaded.files.is_empty());
    }

    #[test]
    fn load_rejects_oversized_manifest() {
        // Defense-in-depth: a hostile (or corrupted-mid-write) manifest
        // larger than `MAX_MANIFEST_BYTES` must be refused before we
        // allocate a `String` for it. Otherwise serde_json's parse can
        // run the process to multi-GB RSS before failing.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("huge.json");
        // Write `MAX + 1` bytes of garbage. JSON parse never runs.
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_MANIFEST_BYTES + 1).unwrap();
        drop(f);

        let err = Manifest::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exceeds") && msg.contains("limit"),
            "expected size-limit error, got: {msg}"
        );
    }

    #[test]
    fn load_accepts_manifest_at_cap_boundary() {
        // A 0-byte file is `<= MAX_MANIFEST_BYTES` — must reach the JSON
        // parser (which then fails on empty input). Guards against an
        // off-by-one in the size guard.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.json");
        std::fs::write(&path, b"").unwrap();
        let err = Manifest::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parse"),
            "expected JSON parse error past the size guard, got: {msg}"
        );
    }

    #[test]
    fn retained_json_fields_keep_flat_shape_and_omit_none() {
        // v1.21: the JSON-resident concerns stayed FLAT (the flatten-
        // grouping design was rejected — reviewer C1). So
        // `skip_serializing_if = "Option::is_none"` is honored verbatim:
        // a None opt-out emits NO key, not `"call_graph": null`. This
        // locks the byte shape flatten would have broken.
        let json = serde_json::to_string(&Manifest::default()).unwrap();
        for key in [
            "git_head",
            "embedder_id",
            "call_graph",
            "bm25",
            "pattern_index",
            "pattern_index_full",
            "vectors_normalized",
            "rename_chains_built",
            "rename_chains_minilm_tiebreak_hits",
            "indexed_at",
        ] {
            assert!(
                !json.contains(key),
                "None field {key} must be omitted (no null), got: {json}"
            );
        }
        // `state` never serialises regardless of contents.
        assert!(
            !json.contains("\"state\""),
            "state must never appear in JSON, got: {json}"
        );

        // Populated opt-outs appear flat at the top level (not nested).
        let populated = Manifest {
            call_graph: Some(false),
            bm25: Some(true),
            ..Manifest::default()
        };
        let json = serde_json::to_string(&populated).unwrap();
        assert!(json.contains("\"call_graph\":false"), "got: {json}");
        assert!(json.contains("\"bm25\":true"), "got: {json}");
    }

    #[test]
    fn sidecar_state_wins_over_default_on_load() {
        // The `index.state` sidecar is the sole store for `state`; the
        // JSON carries none of it, and `load` overlays the sidecar.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        let mut imported_by = BTreeMap::new();
        imported_by.insert(
            "src/a.rs".to_string(),
            BTreeSet::from(["src/b.rs".to_string()]),
        );
        let m = Manifest {
            state: IncrementalState {
                imported_by_built: Some(true),
                imported_by,
                ..Default::default()
            },
            ..Manifest::default()
        };
        m.save(&path).unwrap();
        // JSON alone carries no state.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("imported_by"), "got: {raw}");
        // load overlays the sidecar → state is restored.
        let back = Manifest::load(&path).unwrap();
        assert_eq!(back.state.imported_by_built, Some(true));
        assert!(back.state.imported_by.contains_key("src/a.rs"));
    }

    #[test]
    fn modern_json_without_sidecar_loads_default_state() {
        // Crash-window contract: if the process dies between the JSON
        // rename and the sidecar write, the next load sees a v1.21 JSON
        // (no state keys) and NO `index.state` on disk. `state` must
        // come back default so `vex update` re-bootstraps — no panic,
        // no stale carry-over.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        // A modern manifest serialises with state omitted; write it,
        // then delete the sidecar to simulate the interrupted write.
        Manifest {
            files: HashMap::from([("src/a.rs".to_string(), 7u64)]),
            ..Manifest::default()
        }
        .save(&path)
        .unwrap();
        std::fs::remove_file(state_path_for(&path)).unwrap();

        let back = Manifest::load(&path).unwrap();
        assert_eq!(back.files.get("src/a.rs"), Some(&7));
        assert!(back.state.imported_by.is_empty());
        assert_eq!(back.state.imported_by_built, None);
        assert_eq!(back.state.history_indexed_at, None);
    }
}
