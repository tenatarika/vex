use std::collections::{BTreeMap, BTreeSet, HashMap};
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

    /// v1.17 Phase 14.8 — ISO-date sentinel: `Some(<YYYY-MM-DD>)` when
    /// this index was built with the `index.git_history` sidecar
    /// present, `None` otherwise. Used by `vex update --history`'s
    /// sticky-rebuild logic and by `vex status` to surface "history
    /// indexed at X". Architect L3 (sticky-via-sentinel): no separate
    /// boolean — `history_indexed_at.is_some()` IS the predicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_indexed_at: Option<String>,

    /// v1.17 Phase 14.8 — full SHA of the commit the history section
    /// was indexed at (typically `HEAD` at build time). Required by
    /// Step 5+ incremental update for `git merge-base --is-ancestor
    /// <prior_tip> <new_tip>` force-push detection (architect H3) and
    /// for `<prior_tip>..<new_tip>` range walking. `None` on pre-14.8
    /// manifests or when `--no-history` was passed. Not surfaced in
    /// `vex status` — it's an internal-state field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_tip_sha: Option<String>,

    /// v1.17 Phase 14.8 — sticky cap from `--history-depth N`. Read
    /// by `vex update --history` so the user doesn't have to repeat
    /// the flag on every incremental rebuild. `None` = unbounded
    /// walk (or pre-14.8 manifest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_depth: Option<usize>,

    /// v1.17 Phase 14.8 — populated counts for the section, surfaced
    /// by `vex status` (text + JSON) so users + agents can see at a
    /// glance whether the section is non-trivial. Mirrors the build-
    /// time `HistorySection` shape. `None` on pre-14.8 manifests or
    /// when `--history` was not opted into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryStats>,

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

    /// Phase 11.1.10 (Q4-B) — reverse map of cross-file imports.
    /// `imported_by[target_file_path]` is the set of files that
    /// reference (via type-aware binder) at least one symbol defined
    /// in `target_file_path`. Used by `vex update` to cascade-invalidate
    /// importers when a target file changes — the cascade re-parses
    /// those files (rather than reconstructing their ref_edges via
    /// Q4-A) so refs targeting renamed/deleted symbols get fresh
    /// resolution against the new name table.
    ///
    /// Flat (not `Option`) per rust-reviewer Q7 must-fix: an empty map
    /// is the natural "no edges to cascade" state, identical to a
    /// pre-11.1.10 manifest deserialized with `#[serde(default)]`.
    /// Removing the `Option` flattens the call site to a single
    /// `manifest.imported_by` access without unwrap_or_default litter.
    ///
    /// BTreeMap + BTreeSet (not Hash equivalents) so JSON serialization
    /// is sorted → byte-identical manifests across runs given identical
    /// inputs. Useful when manifests are committed (rare) or diffed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub imported_by: BTreeMap<String, BTreeSet<String>>,

    /// Phase 11.1.10 (Q4-B) writer provenance flag. `Some(true)` when a
    /// vex ≥ 1.18 writer produced this manifest (regardless of whether
    /// `imported_by` ended up empty — a binder-less project or one with
    /// no cross-file refs legitimately writes an empty map). `None` on
    /// pre-11.1.10 manifests where the field didn't exist.
    ///
    /// Distinguishing "writer didn't run yet (pre-11.1.10)" from
    /// "writer ran and saw no edges" lets `vex update` skip the
    /// bootstrap warning on the steady-state empty case (Go-only repos,
    /// fresh projects, etc.). Without this sentinel we'd false-positive
    /// every update on those projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_by_built: Option<bool>,
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

    #[test]
    fn history_fields_round_trip() {
        let m = Manifest {
            history_indexed_at: Some("2026-06-08".to_string()),
            history_tip_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            history_depth: Some(500),
            history: Some(HistoryStats {
                commit_count: 378,
                blob_count: 1498,
                entry_count: 38_999,
                depth_capped: Some(false),
            }),
            ..Manifest::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"history_indexed_at\":\"2026-06-08\""));
        assert!(json.contains("\"history_tip_sha\""));
        assert!(json.contains("\"history_depth\":500"));
        assert!(json.contains("\"commit_count\":378"));
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.history_indexed_at.as_deref(), Some("2026-06-08"));
        assert_eq!(back.history_depth, Some(500));
        let stats = back.history.expect("history sub-object present");
        assert_eq!(stats.commit_count, 378);
        assert_eq!(stats.blob_count, 1498);
        assert_eq!(stats.entry_count, 38_999);
        assert_eq!(stats.depth_capped, Some(false));
    }

    #[test]
    fn history_fields_default_none_on_pre_v17_manifest() {
        // Pre-Phase 14.8 manifests have no `history_*` keys. All four
        // must deserialise as None.
        let pre_json = r#"{"files":{},"cpp_includes_processed":true}"#;
        let m: Manifest = serde_json::from_str(pre_json).unwrap();
        assert_eq!(m.history_indexed_at, None);
        assert_eq!(m.history_tip_sha, None);
        assert_eq!(m.history_depth, None);
        assert!(m.history.is_none());
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
    fn history_fields_omitted_when_none() {
        let m = Manifest::default();
        let json = serde_json::to_string(&m).unwrap();
        for key in [
            "history_indexed_at",
            "history_tip_sha",
            "history_depth",
            // `history` (the stats sub-object) is also omitted via
            // skip_serializing_if when None.
            "\"history\":",
        ] {
            assert!(
                !json.contains(key),
                "expected key {key} absent for default Manifest, got: {json}"
            );
        }
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
}
