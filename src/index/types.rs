//! Shared types passed between stages of the index build.
//!
//! Lives outside `pipeline` and `store` so neither end has to reach
//! across module boundaries to define / consume the cross-stage
//! contract. Phase 11.1.9 (Q4-A) introduced `ReconstructedRef`; this
//! module also formalises [`IndexBuildArtefacts`] so a future Q4-C
//! addition lands as a named field instead of widening the writer
//! signature with another positional argument (architect audit C2).

use std::sync::Arc;

use crate::parse::scope::RefKind;

/// Reconstructed ref-edge fed from `reconstruct_unchanged` into the
/// writer's second-pass resolution. Phase 11.1.9 (Q4-A).
///
/// `target_name` / `target_path` are `Arc<str>` interned across edges so
/// a 50M-edge re-emission doesn't allocate ~8 GB of redundant String
/// copies (architect-H1 / rust-reviewer-#2 must-fix). At typical repo
/// shapes (10k distinct target paths, 5k distinct target names) the
/// interners stay sub-megabyte.
#[derive(Debug, Clone)]
pub(crate) struct ReconstructedRef {
    /// `file_id` of the unchanged source file in the OLD index's file
    /// table. Resolved to a path in the writer via the `old_file_paths`
    /// slice and then mapped to the new index's `file_ids`.
    pub from_file_id: u32,
    pub target_name: Arc<str>,
    /// OLD-index path of the target's defining file — disambiguates
    /// `name_to_global` candidates when `target_name` has multiple
    /// definitions across the project.
    pub target_path: Arc<str>,
    pub line: u32,
    pub col: u32,
    pub kind: RefKind,
}

/// Reconstructed unresolved-by-name ref fed from `reconstruct_unchanged`
/// into the writer (multi-repo Phase 6). Simpler than [`ReconstructedRef`]:
/// the FST key IS the name, so there is no target to re-resolve and no
/// `target_path` tiebreak — the writer just carries the name forward into
/// the v7 unresolved-refs section. Without this, every `vex update` drops
/// every unchanged file's unresolved refs, silently breaking cross-repo
/// strict usages after one routine update.
#[derive(Debug, Clone)]
pub(crate) struct ReconstructedUnresolvedRef {
    /// `file_id` of the unchanged source file in the OLD index's file table.
    pub from_file_id: u32,
    /// The referenced (unresolved) name, interned across edges.
    pub name: Arc<str>,
    pub line: u32,
    pub col: u32,
    pub kind: RefKind,
}

/// Cross-stage handoff for the incremental-update path. Bundles the
/// Q4-A reconstruction outputs that flow from `pipeline::update` into
/// `store::writer` together so the writer signature stays a single
/// `&IndexBuildArtefacts` parameter instead of two parallel slices
/// (architect audit C2 — last-clean-phase boundary before Q4-C).
///
/// A full `vex index` rebuild passes [`IndexBuildArtefacts::default()`]
/// — both vectors empty — and the writer treats the second pass as a
/// no-op (no edges to re-resolve, no old paths to map).
#[derive(Debug, Default)]
pub(crate) struct IndexBuildArtefacts {
    /// Reconstructed ref-edges from unchanged files during `vex update`.
    /// Empty on a full rebuild.
    pub reconstructed_refs: Vec<ReconstructedRef>,
    /// Old-index file_paths table — the writer maps
    /// `ReconstructedRef.from_file_id` back to a path here, then to the
    /// NEW index's file_id via its own file_ids map. Empty on a full
    /// rebuild.
    pub old_file_paths: Vec<String>,
    /// Reconstructed unresolved-by-name refs from unchanged files during
    /// `vex update` (multi-repo Phase 6). Empty on a full rebuild.
    pub reconstructed_unresolved_refs: Vec<ReconstructedUnresolvedRef>,
}
