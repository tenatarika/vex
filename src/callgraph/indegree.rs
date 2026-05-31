//! Phase 13.2 Inc 4 — top-N callees by reverse indegree.
//!
//! Powers `vex bundle --mode project`: ranks symbols by the number of
//! *distinct* callers found in the persistent call-graph section. No
//! PageRank, no damping, no weighting — architect-review A5 descoped
//! the original PageRank plan because (a) the in-memory cache was
//! useless on the MCP code path (`crates/vex-mcp/src/main.rs:122`
//! spawns a fresh process per call), (b) FST identity-collapse
//! materially distorts PageRank under utility names like `new` / `from`
//! / `parse`, and (c) the 13.12 ranking-eval harness scores retrieval
//! relevance, not project importance — there's no ground truth to
//! validate weighting against.
//!
//! Indegree returns a structural lower-bound signal: "how many places
//! call this thing". Documented as `scoring: "reverse_indegree"` in
//! `mode_hints` and labelled experimental in the CLI help.
//!
//! ## Identity caveat (H12, post-v1.10.1)
//!
//! Call edges store callee identity by *name string* (resolved via the
//! symbol FST). Two symbols sharing a name across files (e.g. `init`,
//! `from`, `parse`, `new`) cannot be distinguished by edge data alone
//! without type-aware dispatch resolution. **H12 fix**: instead of
//! emitting one row per name (picking the first FST hit as the
//! representative), we emit one row per `(name, sym_idx)` pair — every
//! definition that shares a name gets its own ranking slot. The
//! caller count is the same upper-bound for each slot, because the
//! edges don't carry enough type info to apportion callers between
//! definitions; the trade-off is "show the same upper bound twice"
//! over "silently hide the second definition".

use std::collections::{HashMap, HashSet};

use crate::cli::scope::PathScope;
use crate::store::reader::IndexReader;

/// One row in the top-N response. The assembler resolves `sym_idx` to a
/// full `BundleCoreItem` via `reader.symbol(idx)`. `indegree` is the
/// number of distinct caller symbol indices that target this name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndegreeRow {
    pub sym_idx: u32,
    pub indegree: u32,
}

/// Result of one `top_n_by_indegree` call. `total_ranked` is the count
/// before path-glob filtering and `top_n` truncation — surfaces in
/// `mode_hints` so the agent can tell when scoping shrank the slate.
#[derive(Debug, Default, Clone)]
pub struct IndegreeReport {
    pub rows: Vec<IndegreeRow>,
    pub total_ranked: usize,
}

/// Scan the persistent call-edges section once, count distinct callers
/// per callee name, resolve each name to a symbol record via the FST,
/// optionally filter by path scope, sort descending, and truncate to
/// `top_n`.
///
/// Returns an empty report when:
/// - the index has no call graph (`!reader.has_call_graph()`),
/// - the call graph exists but has zero edges,
/// - every ranked symbol fails the path-scope filter.
pub fn top_n_by_indegree(
    reader: &IndexReader,
    top_n: usize,
    path_scope: Option<&PathScope>,
) -> IndegreeReport {
    if !reader.has_call_graph() || top_n == 0 {
        return IndegreeReport::default();
    }

    // Step 1 — dedupe callers per callee name. Keying by name STRING
    // (rather than `callee_name_offset`) avoids the writer-side
    // ambiguity around whether call edges and symbol records share the
    // same string arena offset for a given name. Cost: one extra
    // `read_string` per edge.
    let edge_count = reader.call_edge_count();
    let mut by_name: HashMap<String, HashSet<u32>> = HashMap::new();
    for i in 0..edge_count {
        let Some(edge) = reader.call_edge(i) else {
            continue;
        };
        let name = reader.read_string(edge.callee_name_offset).to_string();
        by_name.entry(name).or_default().insert(edge.caller_sym_idx);
    }

    // Step 2 — resolve each name to ALL matching definitions via the
    // symbol FST. Names with no FST hit are dropped (the callee isn't
    // a defined symbol in this index — e.g. a stdlib function or a
    // method on a non-tracked type). FST returns lowercase keys, so we
    // lowercase before lookup.
    //
    // H12: emit one row per `(name, sym_idx)` pair. Pre-H12 only the
    // first FST hit was kept, which silently hid every other definition
    // sharing the name from the ranking (collapsing `A::init` and
    // `B::init` into a single slot). See module-doc "Identity caveat"
    // for the upper-bound caller-count rationale.
    let Some(fst) = reader.symbol_fst_reader() else {
        return IndegreeReport::default();
    };
    let mut rows = resolve_indegree_rows(by_name, |n| fst.find(n));

    let total_ranked = rows.len();

    // Step 3 — path-scope filter applied after ranking so
    // `total_ranked` in `mode_hints` reflects the unfiltered rank
    // surface. Symbol records carry `file_offset`; we resolve once per
    // surviving row.
    if let Some(scope) = path_scope {
        if !scope.is_empty() {
            rows.retain(|row| match reader.symbol(row.sym_idx as usize) {
                Some(rec) => scope.accept(reader.read_string(rec.file_offset)),
                None => false,
            });
        }
    }

    // Step 4 — sort descending by indegree, tie-break by sym_idx for
    // stability (so a re-run on the same index produces the same
    // top-N rather than a HashMap-iteration-order shuffle).
    rows.sort_by(|a, b| b.indegree.cmp(&a.indegree).then(a.sym_idx.cmp(&b.sym_idx)));
    rows.truncate(top_n);

    IndegreeReport { rows, total_ranked }
}

/// Pure helper extracted from [`top_n_by_indegree`] so the H12 contract
/// (one row per `(name, sym_idx)` pair, full caller count per row) is
/// unit-testable without spinning up a real [`IndexReader`].
///
/// `fst_lookup` mirrors `SymbolFstReader::find` — the caller passes a
/// closure that resolves a lowercased name to a list of symbol indices.
fn resolve_indegree_rows(
    by_name: HashMap<String, HashSet<u32>>,
    fst_lookup: impl Fn(&str) -> Vec<u32>,
) -> Vec<IndegreeRow> {
    let mut rows = Vec::with_capacity(by_name.len());
    for (name, callers) in by_name {
        let indices = fst_lookup(&name.to_lowercase());
        if indices.is_empty() {
            continue;
        }
        let indegree = callers.len() as u32;
        for sym_idx in indices {
            rows.push(IndegreeRow { sym_idx, indegree });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indegree_row_sort_ordering_is_descending_with_tiebreak() {
        // Direct unit test on the sort/tiebreak invariant without
        // needing a real IndexReader. Mirrors the comparator in
        // `top_n_by_indegree` step 4.
        let mut rows = [
            IndegreeRow {
                sym_idx: 5,
                indegree: 1,
            },
            IndegreeRow {
                sym_idx: 2,
                indegree: 3,
            },
            IndegreeRow {
                sym_idx: 1,
                indegree: 3,
            },
            IndegreeRow {
                sym_idx: 3,
                indegree: 0,
            },
        ];
        rows.sort_by(|a, b| b.indegree.cmp(&a.indegree).then(a.sym_idx.cmp(&b.sym_idx)));
        let indegs: Vec<u32> = rows.iter().map(|r| r.indegree).collect();
        let idxs: Vec<u32> = rows.iter().map(|r| r.sym_idx).collect();
        assert_eq!(indegs, vec![3, 3, 1, 0]);
        // Tie at indegree=3: sym_idx 1 wins (ascending).
        assert_eq!(idxs[0..2], [1, 2]);
    }

    #[test]
    fn report_default_is_empty() {
        let r = IndegreeReport::default();
        assert!(r.rows.is_empty());
        assert_eq!(r.total_ranked, 0);
    }

    // ---- H12: one row per (name, sym_idx) ----

    #[test]
    fn h12_two_definitions_with_same_name_both_emit_rows() {
        // Two callee names; "init" has two definitions (FST returns
        // sym_idx 10 and 20), "foo" has one (sym_idx 30). Pre-H12 only
        // sym_idx 10 would appear in rows; post-H12 both 10 and 20
        // appear, each carrying the full caller-count upper bound.
        let mut by_name: HashMap<String, HashSet<u32>> = HashMap::new();
        by_name.insert("init".into(), [1u32, 2, 3].into_iter().collect());
        by_name.insert("foo".into(), [4u32].into_iter().collect());

        let rows = resolve_indegree_rows(by_name, |n| match n {
            "init" => vec![10, 20],
            "foo" => vec![30],
            _ => vec![],
        });

        let mut by_idx: HashMap<u32, u32> = rows.iter().map(|r| (r.sym_idx, r.indegree)).collect();
        assert_eq!(
            by_idx.remove(&10),
            Some(3),
            "init's first definition must get the full upper-bound count"
        );
        assert_eq!(
            by_idx.remove(&20),
            Some(3),
            "init's second definition must ALSO appear (pre-H12 was hidden)"
        );
        assert_eq!(by_idx.remove(&30), Some(1));
        assert!(by_idx.is_empty(), "no extra rows: {by_idx:?}");
    }

    #[test]
    fn h12_name_with_no_fst_hit_is_skipped() {
        let mut by_name: HashMap<String, HashSet<u32>> = HashMap::new();
        by_name.insert("std_unknown".into(), [1u32].into_iter().collect());
        let rows = resolve_indegree_rows(by_name, |_| vec![]);
        assert!(rows.is_empty(), "unresolved name must not produce a row");
    }
}
