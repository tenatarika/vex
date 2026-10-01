//! Deterministic symbol clustering (`docs/V9-FORMAT.md` §3.2–§3.4, §13).
//!
//! Phase A, P3: **pure, unwired** code. Nothing here is called from
//! `vex index` / `vex update` yet (that is P4a/P4b) — this module only
//! projects the already-resolved call/ref/hierarchy edges the writer
//! collects during Pass-2 into an undirected weighted graph
//! ([`projection`]) and runs deterministic Leiden-CPM over it
//! ([`leiden`]) to produce cluster assignments. [`cluster`] ties the two
//! together and performs the §3.3 step 6 "Finalize" (singletons →
//! UNCLUSTERED, dense ordinals by min canonical key, per-cluster
//! records, labels and hubs) — no on-disk serialisation (that is P4a).
//!
//! The boundary with the writer is [`projection::ProjectionInput`]: a
//! small, self-contained set of plain records (no writer-internal types
//! leak in here), so P4 only needs to translate its own in-memory
//! builders into this shape.

// P3: pure, unwired code — nothing in `src/index/pipeline` or
// `src/store/writer.rs` calls into this module yet (that is P4a/P4b), so
// its public API is only exercised by this subtree's own tests today.
// Remove this crate-wide allow once P4 wires `cluster::cluster` into
// `write_index_to`.
#![allow(dead_code)]

pub mod leiden;
pub mod projection;

use std::collections::HashMap;

use projection::{ProjectedGraph, ProjectionInput};

/// On-disk assignment sentinels (`docs/V9-FORMAT.md` §2.4). `0 .. k-1`
/// (any value `< NEW`) is a real cluster ordinal.
pub const NOT_ELIGIBLE: u32 = 0xFFFF_FFFF;
pub const UNCLUSTERED: u32 = 0xFFFF_FFFE;
pub const NEW: u32 = 0xFFFF_FFFD;

/// One finalized cluster record (§2.4 `ClusterRecord`, minus the
/// `label_offset`/string-pool indirection — P4 interns `label` into the
/// strings pool and keeps everything else as-is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterRecordOut {
    /// Member `sym_idx` with the minimum canonical key (R6).
    pub rep_sym_idx: u32,
    pub size: u32,
    /// Sum of intra-cluster pair weights (saturating).
    pub internal_weight: u32,
    /// Sum of weights to other clusters, UNCLUSTERED counted (saturating).
    pub cut_weight: u32,
    /// Deepest directory prefix holding ≥60% of members, or
    /// `"(mixed) <top dir>/"` (§4.2).
    pub label: String,
    /// Top-3 members by intra-cluster weighted degree, `sym_idx`,
    /// descending degree then ascending `sym_idx`. Fewer than 3 if the
    /// cluster is smaller.
    pub hubs: Vec<u32>,
}

/// Full clustering result over the entire symbol space (§2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterOutput {
    /// Length == `symbol_count`. Values are either a dense cluster
    /// ordinal (`< clusters.len()`) or one of [`NOT_ELIGIBLE`] /
    /// [`UNCLUSTERED`] (this module never emits [`NEW`] — that sentinel
    /// is a P4b `vex update` carry concept only).
    pub assign: Vec<u32>,
    /// `clusters[i]` is the record for ordinal `i` (ordinals ascend by
    /// min canonical key, §13 R6).
    pub clusters: Vec<ClusterRecordOut>,
    /// Aggregation levels run (diagnostic, §2.4 `levels`).
    pub levels: u16,
    /// Set when either the outer `MAX_ITERATIONS` cap or the
    /// `MAX_LEVELS` cap was hit without convergence (§3.3 step 5, R17).
    pub iter_cap_hit: bool,
}

/// Run the full Phase-A pipeline: project `input` into an undirected
/// weighted graph over eligible symbols, run deterministic Leiden-CPM at
/// resolution `resolution = (num, den)` (γ = num/den), and finalize into
/// the on-disk shape (minus serialisation).
///
/// Pure: no I/O, no global state, single-threaded. Byte-for-byte
/// deterministic for a fixed `input` and `resolution` (§3.3).
pub fn cluster(input: &ProjectionInput<'_>, resolution: (u32, u32)) -> ClusterOutput {
    let projected = projection::project(input);
    let res = leiden::Resolution {
        num: resolution.0,
        den: resolution.1,
    };
    let leiden_result = leiden::run(&projected.graph, res);
    finalize(&projected, &leiden_result, input)
}

/// §3.3 step 6 + §4.2: flatten Leiden's dense node-id assignment back to
/// `sym_idx` space, demote singletons to UNCLUSTERED, and compute each
/// surviving cluster's record (size/weights/label/hubs).
fn finalize(
    projected: &ProjectedGraph,
    leiden_result: &leiden::LeidenResult,
    input: &ProjectionInput<'_>,
) -> ClusterOutput {
    let symbol_count = projected.symbol_count as usize;
    let mut assign = vec![NOT_ELIGIBLE; symbol_count];
    let sym_index: HashMap<u32, &projection::ProjectionSymbol> =
        input.symbols.iter().map(|s| (s.sym_idx, s)).collect();

    // First pass: how many level-0 (eligible) nodes does each Leiden
    // ordinal have? A cluster of size 1 is demoted to UNCLUSTERED
    // (§3.3 step 6) — it never gets a ClusterRecordOut.
    let num_ordinals = leiden_result
        .assignment
        .iter()
        .copied()
        .max()
        .map(|m| m as usize + 1)
        .unwrap_or(0);
    let mut members_of: Vec<Vec<u32>> = vec![Vec::new(); num_ordinals]; // ordinal -> [sym_idx], ascending
    for (node_id, &ordinal) in leiden_result.assignment.iter().enumerate() {
        let sym_idx = projected.node_sym_idx[node_id];
        members_of[ordinal as usize].push(sym_idx);
    }

    // Real (non-singleton) clusters keep their Leiden ordinal order,
    // which is already "ascending by min member node id" == "ascending
    // by min canonical key" (R6, since node ids are canonical-order
    // positions) == "ascending by min sym_idx" is NOT generally true
    // (canonical order sorts by path/line/kind/name first, sym_idx only
    // breaks ties) — but §13 redefines "min sym_idx" to mean "min
    // canonical key", so the Leiden ordinal order already *is* the
    // correct cluster id order. Singletons are simply dropped, and the
    // remaining ordinals are re-densified (no gaps).
    let mut clusters: Vec<ClusterRecordOut> = Vec::new();
    let mut ordinal_remap: Vec<Option<u32>> = vec![None; num_ordinals];
    for (ordinal, members) in members_of.iter().enumerate() {
        if members.len() < 2 {
            continue; // singleton or empty slot -> UNCLUSTERED below
        }
        ordinal_remap[ordinal] = Some(clusters.len() as u32);
        clusters.push(build_record(members, &sym_index));
    }

    for (node_id, &ordinal) in leiden_result.assignment.iter().enumerate() {
        let sym_idx = projected.node_sym_idx[node_id] as usize;
        assign[sym_idx] = match ordinal_remap[ordinal as usize] {
            Some(new_ordinal) => new_ordinal,
            None => UNCLUSTERED,
        };
    }

    // cut_weight needs the final assign array (to know which pairs
    // cross a cluster boundary vs. land on an UNCLUSTERED/ineligible
    // endpoint), so it is computed in a second pass over the *original*
    // pair list, not during `build_record`.
    accumulate_cut_weight(&mut clusters, projected, &assign);

    ClusterOutput {
        assign,
        clusters,
        levels: leiden_result.levels,
        iter_cap_hit: leiden_result.iter_cap_hit,
    }
}

fn build_record(
    members: &[u32],
    sym_index: &HashMap<u32, &projection::ProjectionSymbol>,
) -> ClusterRecordOut {
    // `members` order is node-id (canonical) order — R6 wants
    // `rep_sym_idx` to be the member with the smallest canonical key,
    // which is simply `members[0]`.
    let rep_sym_idx = members[0];
    let size = members.len() as u32;

    ClusterRecordOut {
        rep_sym_idx,
        size,
        internal_weight: 0, // filled in by accumulate_cut_weight's sibling pass below
        cut_weight: 0,
        label: compute_label(members, sym_index),
        hubs: Vec::new(), // filled in below once we have internal degree
    }
}

/// Second pass over the projected graph's pair list: for every pair
/// `(a, b, w)`, if both endpoints land in the same real cluster, the
/// weight is internal; otherwise it is cut (charged to every cluster
/// endpoint involved — an UNCLUSTERED/ineligible endpoint still counts
/// against the clustered side's cut weight, per §2.4 "cut_weight ...
/// UNCLUSTERED counted"). Also computes per-member weighted intra-
/// cluster degree for the top-3 hubs.
fn accumulate_cut_weight(
    clusters: &mut [ClusterRecordOut],
    projected: &ProjectedGraph,
    assign: &[u32],
) {
    let num_clusters = clusters.len();
    let mut internal: Vec<u64> = vec![0; num_clusters];
    let mut cut: Vec<u64> = vec![0; num_clusters];
    // sym_idx -> weighted intra-cluster degree, only meaningful for
    // clustered symbols; sized by symbol_count for direct indexing.
    let mut intra_degree: HashMap<u32, u64> = HashMap::new();

    for &(u, v, w) in &projected.pairs {
        let su = projected.node_sym_idx[u as usize];
        let sv = projected.node_sym_idx[v as usize];
        let cu = assign[su as usize];
        let cv = assign[sv as usize];
        let wu64 = u64::from(w);
        if cu < num_clusters as u32 && cu == cv {
            internal[cu as usize] = internal[cu as usize].saturating_add(wu64);
            *intra_degree.entry(su).or_insert(0) += wu64;
            *intra_degree.entry(sv).or_insert(0) += wu64;
        } else {
            if cu < num_clusters as u32 {
                cut[cu as usize] = cut[cu as usize].saturating_add(wu64);
            }
            if cv < num_clusters as u32 {
                cut[cv as usize] = cut[cv as usize].saturating_add(wu64);
            }
        }
    }

    for (i, rec) in clusters.iter_mut().enumerate() {
        rec.internal_weight = u32::try_from(internal[i]).unwrap_or(u32::MAX);
        rec.cut_weight = u32::try_from(cut[i]).unwrap_or(u32::MAX);
    }

    // Hubs: recompute per-cluster membership from `assign` (cheap,
    // O(symbol_count)) rather than threading member lists through —
    // keeps this function the single owner of intra_degree.
    let mut members_of: Vec<Vec<u32>> = vec![Vec::new(); num_clusters];
    for (sym_idx, &c) in assign.iter().enumerate() {
        if (c as usize) < num_clusters {
            members_of[c as usize].push(sym_idx as u32);
        }
    }
    for (i, rec) in clusters.iter_mut().enumerate() {
        let mut ranked: Vec<(u64, u32)> = members_of[i]
            .iter()
            .map(|&s| (*intra_degree.get(&s).unwrap_or(&0), s))
            .collect();
        // Descending degree, ties ascending sym_idx (§4.2).
        ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        rec.hubs = ranked.into_iter().take(3).map(|(_, s)| s).collect();
    }
}

/// §4.2: the deepest directory prefix holding ≥60% of the cluster's
/// members' files, else `"(mixed) <most common top dir>/"`.
fn compute_label(
    members: &[u32],
    sym_index: &HashMap<u32, &projection::ProjectionSymbol>,
) -> String {
    let paths: Vec<&str> = members
        .iter()
        .filter_map(|s| sym_index.get(s).map(|sym| sym.path.as_str()))
        .collect();
    if paths.is_empty() {
        return "(mixed) /".to_string();
    }
    let total = paths.len();
    // Try every distinct prefix depth, deepest first ("the deepest
    // directory prefix that contains >= 60% of the members' files");
    // first depth that reaches the share wins.
    let max_depth = paths
        .iter()
        .map(|p| p.split('/').count())
        .max()
        .unwrap_or(1);
    for depth in (1..=max_depth).rev() {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let prefixes: Vec<&str> = paths
            .iter()
            .map(|p| {
                let take = depth.min(p.split('/').count());
                nth_prefix(p, take)
            })
            .collect();
        for p in &prefixes {
            *counts.entry(p).or_insert(0) += 1;
        }
        if let Some((best_prefix, count)) = counts_max(&counts) {
            // count/total >= 0.6  <=>  5*count >= 3*total (integer-safe).
            if 5 * count >= 3 * total {
                let mut label = best_prefix.to_string();
                if !label.ends_with('/') {
                    label.push('/');
                }
                return label;
            }
        }
    }
    // No prefix of any depth reached 60% — "(mixed) <most common top dir>/".
    let mut top_counts: HashMap<&str, usize> = HashMap::new();
    for p in &paths {
        let top = p.split('/').next().unwrap_or(p);
        *top_counts.entry(top).or_insert(0) += 1;
    }
    let top_dir = counts_max(&top_counts).map(|(p, _)| p).unwrap_or("");
    format!("(mixed) {top_dir}/")
}

/// The prefix of `path` made of its first `take` `/`-separated segments
/// (POSIX-separated, already true of every stored path via
/// `to_rel_posix`, §13 R21). `take >= ` the path's segment count
/// returns the whole path unchanged.
fn nth_prefix(path: &str, take: usize) -> &str {
    if take == 0 {
        return "";
    }
    let mut count = 0;
    for (idx, ch) in path.char_indices() {
        if ch == '/' {
            count += 1;
            if count == take {
                return &path[..idx];
            }
        }
    }
    path
}

fn counts_max<'a>(counts: &HashMap<&'a str, usize>) -> Option<(&'a str, usize)> {
    // Deterministic tie-break: highest count, then lexicographically
    // smallest prefix (HashMap iteration order is not deterministic).
    counts
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(&k, &v)| (k, v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use projection::{ProjectionHierarchyEdge, ProjectionSymbol};

    fn sym(sym_idx: u32, path: &str, line: u32, name: &str) -> ProjectionSymbol {
        ProjectionSymbol {
            sym_idx,
            path: path.to_string(),
            line,
            kind: 0, // Function
            name: name.to_string(),
            language: Some(crate::parse::language::Language::Rust),
        }
    }

    #[test]
    fn two_k5_joined_by_bridge_yields_two_clusters() {
        // Build two K5 cliques (sym_idx 0..5 and 5..10) joined by a single bridge edge (4-5).
        let mut symbols = Vec::new();
        for i in 0..10u32 {
            symbols.push(sym(i, "src/a.rs", i + 1, &format!("f{i}")));
        }
        let mut hierarchy_edges = Vec::new();
        for clique in [0u32, 5u32] {
            for a in clique..clique + 5 {
                for b in (a + 1)..clique + 5 {
                    hierarchy_edges.push(ProjectionHierarchyEdge {
                        from_sym_idx: a,
                        to_sym_idx: b,
                    });
                }
            }
        }
        hierarchy_edges.push(ProjectionHierarchyEdge {
            from_sym_idx: 4,
            to_sym_idx: 5,
        });

        let input = ProjectionInput {
            symbol_count: 10,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &hierarchy_edges,
            file_paths: &[],
        };

        let out = cluster(&input, (1, 8));
        let real_clusters: Vec<_> = out.clusters.iter().collect();
        assert_eq!(
            real_clusters.len(),
            2,
            "expected 2 clusters, got {:?}",
            out.assign
        );
        // Every symbol in clique A shares a cluster, same for clique B, and they differ.
        let ca = out.assign[0];
        let cb = out.assign[5];
        assert_ne!(ca, cb);
        for i in 0..5 {
            assert_eq!(out.assign[i], ca);
        }
        for i in 5..10 {
            assert_eq!(out.assign[i], cb);
        }
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let input = ProjectionInput {
            symbol_count: 0,
            symbols: &[],
            call_edges: &[],
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &[],
        };
        let out = cluster(&input, (1, 8));
        assert!(out.assign.is_empty());
        assert!(out.clusters.is_empty());
    }

    #[test]
    fn ineligible_kind_is_not_eligible() {
        let symbols = vec![
            ProjectionSymbol {
                sym_idx: 0,
                path: "src/a.rs".into(),
                line: 1,
                kind: 13, // Module
                name: "<module:src/a.rs>".into(),
                language: Some(crate::parse::language::Language::Rust),
            },
            sym(1, "src/a.rs", 2, "f"),
        ];
        let input = ProjectionInput {
            symbol_count: 2,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &[],
        };
        let out = cluster(&input, (1, 8));
        assert_eq!(out.assign[0], NOT_ELIGIBLE);
        assert_eq!(out.assign[1], UNCLUSTERED);
    }

    #[test]
    fn isolated_eligible_symbol_is_unclustered() {
        let symbols = vec![sym(0, "src/a.rs", 1, "f")];
        let input = ProjectionInput {
            symbol_count: 1,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &[],
        };
        let out = cluster(&input, (1, 8));
        assert_eq!(out.assign, vec![UNCLUSTERED]);
    }

    #[test]
    fn language_exclusion_marks_not_eligible() {
        let symbols = vec![ProjectionSymbol {
            sym_idx: 0,
            path: "README.md".into(),
            line: 1,
            kind: 12, // Heading would also exclude, use Function to isolate the language check
            name: "intro".into(),
            language: Some(crate::parse::language::Language::Markdown),
        }];
        let input = ProjectionInput {
            symbol_count: 1,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &[],
        };
        let out = cluster(&input, (1, 8));
        assert_eq!(out.assign, vec![NOT_ELIGIBLE]);
    }

    // -----------------------------------------------------------------
    // Projection robustness (P3 follow-up #2): `project` and `cluster`
    // must never panic on arbitrary small `ProjectionInput`s, including
    // out-of-range `sym_idx`/`from_file_id`/`to_sym_idx`, a mismatched
    // `ambiguous` length (shorter or longer than `ref_edges`), empty
    // paths, and an undersized `file_paths`/`symbol_count` relative to
    // what the other fields reference — exactly the shape of garbage a
    // live writer (P4) could hand in before every invariant it
    // currently upholds internally is double-checked against this
    // module's boundary.
    // -----------------------------------------------------------------

    fn arb_symbol() -> impl proptest::strategy::Strategy<Value = projection::ProjectionSymbol> {
        use proptest::prelude::*;
        (
            0u32..30,
            "[a-z/]{0,12}", // the regex itself already covers the empty-path case
            0u32..2000,
            0u8..20, // beyond the real SymbolKind range too (13 is the highest defined kind)
            "[a-zA-Z_]{0,8}",
            proptest::option::of(proptest::sample::select(vec![
                crate::parse::language::Language::Rust,
                crate::parse::language::Language::Markdown,
                crate::parse::language::Language::Python,
            ])),
        )
            .prop_map(|(sym_idx, path, line, kind, name, language)| {
                projection::ProjectionSymbol {
                    sym_idx,
                    path,
                    line,
                    kind,
                    name,
                    language,
                }
            })
    }

    fn arb_call_edge() -> impl proptest::strategy::Strategy<Value = projection::ProjectionCallEdge>
    {
        use proptest::prelude::*;
        (0u32..35, "[a-zA-Z_]{0,8}", 0u32..2000).prop_map(|(caller_sym_idx, callee_name, line)| {
            projection::ProjectionCallEdge {
                caller_sym_idx,
                callee_name,
                line,
            }
        })
    }

    fn arb_ref_edge() -> impl proptest::strategy::Strategy<Value = projection::ProjectionRefEdge> {
        use proptest::prelude::*;
        (0u32..12, 0u32..2000, 0u32..35, 0u8..6).prop_map(
            |(from_file_id, line, to_sym_idx, kind)| projection::ProjectionRefEdge {
                from_file_id,
                line,
                to_sym_idx,
                kind,
            },
        )
    }

    fn arb_hierarchy_edge(
    ) -> impl proptest::strategy::Strategy<Value = projection::ProjectionHierarchyEdge> {
        use proptest::prelude::*;
        (0u32..35, 0u32..35).prop_map(|(from_sym_idx, to_sym_idx)| {
            projection::ProjectionHierarchyEdge {
                from_sym_idx,
                to_sym_idx,
            }
        })
    }

    proptest::proptest! {
        #[test]
        fn project_and_cluster_never_panic_on_arbitrary_input(
            symbols in proptest::collection::vec(arb_symbol(), 0..15),
            call_edges in proptest::collection::vec(arb_call_edge(), 0..15),
            ref_edges in proptest::collection::vec(arb_ref_edge(), 0..15),
            // Deliberately independent of `ref_edges.len()` — this is
            // exactly the "mismatched ambiguous length" case.
            ambiguous_len in 0usize..18,
            hierarchy_edges in proptest::collection::vec(arb_hierarchy_edge(), 0..10),
            file_paths in proptest::collection::vec("[a-z/]{0,10}", 0..8),
            declared_symbol_count in 0u32..20,
            gamma_den in 1u32..32,
        ) {
            let ambiguous: Vec<bool> = (0..ambiguous_len).map(|i| i % 2 == 0).collect();

            let input = ProjectionInput {
                symbol_count: declared_symbol_count,
                symbols: &symbols,
                call_edges: &call_edges,
                ref_edges: &ref_edges,
                ambiguous: &ambiguous,
                hierarchy_edges: &hierarchy_edges,
                file_paths: &file_paths,
            };

            // Must never panic (a panic anywhere below fails the test by
            // propagating, matching the proptest contract).
            let projected = projection::project(&input);
            let expected_symbol_count = declared_symbol_count.max(
                symbols.iter().map(|s| s.sym_idx + 1).max().unwrap_or(0)
            );
            proptest::prop_assert_eq!(projected.symbol_count, expected_symbol_count);

            let out = cluster(&input, (1, gamma_den));
            proptest::prop_assert_eq!(
                out.assign.len(),
                expected_symbol_count as usize,
                "assign length must equal the effective symbol_count"
            );

            // Every sentinel/ordinal in `assign` must be a valid cluster
            // ordinal or one of the three defined sentinels — never an
            // out-of-range leftover value.
            for &a in &out.assign {
                proptest::prop_assert!(
                    (a as usize) < out.clusters.len() || a == NOT_ELIGIBLE || a == UNCLUSTERED || a == NEW,
                    "assign value {a} is neither a valid cluster ordinal nor a defined sentinel"
                );
            }
        }
    }
}
