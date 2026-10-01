//! Shared types passed between stages of the index build.
//!
//! Lives outside `pipeline` and `store` so neither end has to reach
//! across module boundaries to define / consume the cross-stage
//! contract. Phase 11.1.9 (Q4-A) introduced `ReconstructedRef`; this
//! module also formalises [`IndexBuildArtefacts`] so a future Q4-C
//! addition lands as a named field instead of widening the writer
//! signature with another positional argument (architect audit C2).

use std::collections::{HashMap, HashSet};
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
    /// P4b (`docs/V9-FORMAT.md` §5, §13 R11-R13) — cluster carry-forward
    /// data extracted from the OLD index during `vex update`. `None` on a
    /// full rebuild AND on an update whose prior index had no COMPUTED
    /// cluster section (the R14 compute-once path applies instead — see
    /// `prior_clusters_opt_out`).
    pub cluster_carry: Option<ClusterCarryArtefacts>,
    /// P4b (§13 R14, Q2) — the prior manifest's `clusters_opt_out` value,
    /// read BEFORE the writer runs so `vex update` can honour a user's
    /// `vex index --no-clusters` instead of silently computing clusters
    /// they opted out of. `None` on a full rebuild (irrelevant there —
    /// `opts.with_clusters` is authoritative for `vex index`) and on a
    /// pre-P4b manifest.
    pub prior_clusters_opt_out: Option<bool>,
}

/// P4b carry-forward data for one `vex update`'s cluster section,
/// extracted from the OLD index's `ClusterSectionReader` + the new
/// symbol set being assembled. Threaded into `store::writer` so the
/// writer can freeze+carry (§5) instead of recomputing (`docs/V9-FORMAT.md`
/// §13 R11-R13).
#[derive(Debug, Clone, Default)]
pub(crate) struct ClusterCarryArtefacts {
    /// Per reconstructed (unchanged-file) symbol, in the SAME flattened
    /// order `reconstruct_unchanged` emits them — the OLD index's raw
    /// `assign` value for that symbol (sentinel or ordinal), carried
    /// verbatim (§5 rule 1). The pipeline extends this with one `None`
    /// per re-parsed/new symbol so the full `Vec` is 1:1 with the final
    /// symbol order the writer builds `records` in (`reconstruct_unchanged`
    /// itself only ever pushes `Some`).
    pub per_symbol_carried: Vec<Option<u32>>,
    /// OLD `sym_idx` for each `Some` entry in `per_symbol_carried` (same
    /// length, same indices) — grows `old_to_new` for the unchanged
    /// prefix without a second lookup pass.
    pub per_symbol_old_idx: Vec<Option<u32>>,
    /// Every OLD symbol belonging to a path in `changed_set` (re-parsed
    /// this update — NOT the unchanged slice, which never reaches here),
    /// grouped by path, as `(name, kind, old_sym_idx, old_assign)`. Used
    /// by the writer to key-match re-parsed symbols 1:1 by
    /// `(path, name, kind)` (§13 R12) and to carry cascade-only files
    /// positionally.
    pub old_symbols_by_path: HashMap<String, Vec<(String, u8, u32, u32)>>,
    /// Size `old_to_new` must be built at (one slot per OLD `sym_idx`,
    /// i.e. the OLD index's `symbol_count`).
    pub old_symbol_count: u32,
    /// Paths re-parsed this update solely because of Q4-B/Q4-C cascade
    /// invalidation (content hash UNCHANGED, re-parsed only to rebind
    /// refs) — eligible for positional (not key-match) carry when the
    /// old and new symbol counts for that path agree (§13 R12).
    pub cascade_unchanged_paths: HashSet<String>,
    /// OLD cluster table, one entry per ordinal, in original order —
    /// `rep_sym_idx`/`hubs` are OLD `sym_idx` values the writer remaps
    /// through the completed `old_to_new` map; `size`/weights/`label`
    /// are frozen build-time values (§5 rule 5).
    pub old_table: Vec<CarriedClusterRecord>,
    pub resolution: (u32, u32),
    pub algo_version: u16,
    pub levels: u16,
    /// Frozen at whatever the OLD header already recorded — never
    /// updated to the live/reconstructed symbol count (§13 R3's
    /// "build-time symbol count" bound must stay stable across however
    /// many consecutive carries have run since the last full `vex index`).
    pub build_symbol_count: u32,
    pub iter_cap_hit: bool,
}

/// One OLD `ClusterRecord`, decoded and ready for the writer's remap +
/// re-intern step. `rep_sym_idx`/`hubs` are `None` when the OLD record
/// already stored the "lost" sentinel (`u32::MAX`) — carried forward as
/// `None` (→ `u32::MAX` again) regardless of whether `old_to_new` would
/// otherwise resolve them, since a lost reference never comes back.
#[derive(Debug, Clone)]
pub(crate) struct CarriedClusterRecord {
    pub rep_sym_idx: Option<u32>,
    pub size: u32,
    pub internal_weight: u32,
    pub cut_weight: u32,
    pub label: String,
    pub hubs: [Option<u32>; 3],
}
