//! Phase 13.12 — ranking evaluation harness.
//!
//! Tiny, dependency-free implementations of the three IR metrics that
//! `vex eval` and the regression-guard test consume:
//!
//! * `ndcg_at_k`   — Normalized Discounted Cumulative Gain @k. The
//!   primary signal: tolerant of multiple acceptable answers, log-decay
//!   over rank position, normalized into `[0.0, 1.0]`.
//! * `recall_at_k` — fraction of relevant items recovered in the top-k.
//!   Coarser than nDCG but useful for "did we even find it?" checks.
//! * `mrr`         — Mean Reciprocal Rank. The classic "top-1 must be
//!   right" signal: `1 / rank_of_first_relevant`, or `0.0` if none.
//!
//! All three are textbook formulas (Manning, Raghavan, Schütze 2008;
//! Järvelin & Kekäläinen 2002). We use binary relevance (a result is
//! either in the `relevant` set or it isn't) — that's all the golden
//! set encodes today, and it keeps the math one screen long.
//!
//! **Anti-goal**: not a research benchmark. The bar is "catches a
//! silent ranking regression in CI", nothing more.

pub mod harness;

use std::collections::HashSet;
use std::hash::Hash;

/// Normalized Discounted Cumulative Gain at rank `k`.
///
/// Binary relevance: each ranked item contributes `1` if it's in
/// `relevant`, else `0`. The DCG sum decays each rank by `log2(rank+1)`,
/// and the result is normalized by the ideal DCG (every relevant item
/// packed at the top), capped at `k`.
///
/// Returns `1.0` when `relevant` is empty (vacuously perfect — no
/// expectations to violate). Returns `0.0` when `k == 0`.
///
/// Formula (Järvelin & Kekäläinen 2002):
///   DCG@k = Σ rel(i) / log2(i + 1)        for i ∈ 1..=k
///   IDCG@k = DCG@k of the ideal ranking
///   nDCG@k = DCG@k / IDCG@k
pub fn ndcg_at_k<T: Eq + Hash>(ranked: &[T], relevant: &HashSet<T>, k: usize) -> f64 {
    if relevant.is_empty() {
        return 1.0;
    }
    if k == 0 {
        return 0.0;
    }
    let dcg = dcg_at_k(ranked, relevant, k);
    // Ideal ranking: as many `1`s at the top as we have relevant items,
    // capped at k. Anything beyond min(|relevant|, k) cannot contribute.
    let ideal_hits = relevant.len().min(k);
    let idcg: f64 = (1..=ideal_hits)
        .map(|i| 1.0 / ((i as f64) + 1.0).log2())
        .sum();
    if idcg == 0.0 {
        // Defensive: ideal_hits == 0 already handled by the empty check,
        // but keep this guard so a future change to relevance scoring
        // can't silently divide by zero.
        return 0.0;
    }
    dcg / idcg
}

fn dcg_at_k<T: Eq + Hash>(ranked: &[T], relevant: &HashSet<T>, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .enumerate()
        .filter(|(_, item)| relevant.contains(*item))
        .map(|(idx, _)| {
            // Rank is 1-based; log2(rank + 1) makes rank 1 worth 1.0,
            // rank 2 worth ~0.631, rank 3 worth 0.5, etc.
            let rank = (idx + 1) as f64;
            1.0 / (rank + 1.0).log2()
        })
        .sum()
}

/// Recall at rank `k`: fraction of relevant items that appear in the
/// top-k of `ranked`. Returns `1.0` when `relevant` is empty (vacuously
/// perfect — no items to miss). Returns `0.0` when `k == 0`.
pub fn recall_at_k<T: Eq + Hash>(ranked: &[T], relevant: &HashSet<T>, k: usize) -> f64 {
    if relevant.is_empty() {
        return 1.0;
    }
    if k == 0 {
        return 0.0;
    }
    let hits = ranked
        .iter()
        .take(k)
        .filter(|item| relevant.contains(*item))
        .count();
    hits as f64 / relevant.len() as f64
}

/// Mean Reciprocal Rank for a single query.
///
/// Returns `1 / rank_of_first_relevant_item` (1-based), or `0.0` when
/// no relevant item is found anywhere in `ranked`. The "mean" is left
/// to the caller — aggregate per-query MRRs across the golden set by
/// arithmetic mean.
///
/// Returns `1.0` when `relevant` is empty (vacuously — there's nothing
/// to demand a top rank for).
pub fn mrr<T: Eq + Hash>(ranked: &[T], relevant: &HashSet<T>) -> f64 {
    if relevant.is_empty() {
        return 1.0;
    }
    ranked
        .iter()
        .position(|item| relevant.contains(item))
        .map(|idx| 1.0 / ((idx + 1) as f64))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relevant_set<const N: usize>(items: [&str; N]) -> HashSet<String> {
        items.into_iter().map(String::from).collect()
    }

    fn ranked(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ----- ndcg_at_k -----

    #[test]
    fn ndcg_perfect_top_rank_is_one() {
        let r = relevant_set(["a"]);
        let ranked = ranked(&["a", "b", "c"]);
        // a at rank 1: DCG = 1 / log2(2) = 1.0, IDCG = 1.0
        assert!((ndcg_at_k(&ranked, &r, 10) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ndcg_irrelevant_top_is_zero() {
        let r = relevant_set(["a"]);
        let ranked = ranked(&["b", "c", "d"]);
        assert_eq!(ndcg_at_k(&ranked, &r, 10), 0.0);
    }

    #[test]
    fn ndcg_rank_two_matches_textbook_formula() {
        // a at rank 2: DCG = 1 / log2(3) ≈ 0.6309
        // IDCG (one relevant): 1 / log2(2) = 1.0
        // nDCG ≈ 0.6309
        let r = relevant_set(["a"]);
        let ranked = ranked(&["b", "a", "c"]);
        let v = ndcg_at_k(&ranked, &r, 10);
        let expected = (1.0 / 3.0_f64.log2()) / (1.0 / 2.0_f64.log2());
        assert!((v - expected).abs() < 1e-9, "got {v}, expected {expected}");
    }

    #[test]
    fn ndcg_two_relevant_both_top_is_one() {
        // a at 1, b at 2: DCG = 1/log2(2) + 1/log2(3)
        // IDCG (two relevant): same shape → nDCG = 1.0
        let r = relevant_set(["a", "b"]);
        let ranked = ranked(&["a", "b", "c"]);
        assert!((ndcg_at_k(&ranked, &r, 10) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ndcg_empty_relevant_is_one() {
        let r: HashSet<String> = HashSet::new();
        let ranked = ranked(&["a", "b"]);
        assert_eq!(ndcg_at_k(&ranked, &r, 10), 1.0);
    }

    #[test]
    fn ndcg_k_zero_is_zero() {
        let r = relevant_set(["a"]);
        let ranked = ranked(&["a"]);
        assert_eq!(ndcg_at_k(&ranked, &r, 0), 0.0);
    }

    #[test]
    fn ndcg_k_caps_evaluation_window() {
        // Relevant at rank 11; nDCG@10 must be 0.
        let r = relevant_set(["target"]);
        let mut items = vec!["junk"; 10];
        items.push("target");
        let ranked = ranked(&items);
        assert_eq!(ndcg_at_k(&ranked, &r, 10), 0.0);
        // At k=20, the same ranking recovers some signal.
        assert!(ndcg_at_k(&ranked, &r, 20) > 0.0);
    }

    // ----- recall_at_k -----

    #[test]
    fn recall_perfect_when_all_relevant_in_window() {
        let r = relevant_set(["a", "b"]);
        let ranked = ranked(&["a", "b", "c"]);
        assert_eq!(recall_at_k(&ranked, &r, 10), 1.0);
    }

    #[test]
    fn recall_half_when_one_of_two_in_window() {
        let r = relevant_set(["a", "b"]);
        let ranked = ranked(&["a", "x", "y"]);
        assert_eq!(recall_at_k(&ranked, &r, 10), 0.5);
    }

    #[test]
    fn recall_zero_when_none_in_window() {
        let r = relevant_set(["a"]);
        let ranked = ranked(&["b", "c"]);
        assert_eq!(recall_at_k(&ranked, &r, 10), 0.0);
    }

    #[test]
    fn recall_empty_relevant_is_one() {
        let r: HashSet<String> = HashSet::new();
        let ranked = ranked(&["a"]);
        assert_eq!(recall_at_k(&ranked, &r, 10), 1.0);
    }

    #[test]
    fn recall_k_caps_window() {
        let r = relevant_set(["target"]);
        let mut items = vec!["junk"; 5];
        items.push("target");
        let ranked = ranked(&items);
        assert_eq!(recall_at_k(&ranked, &r, 5), 0.0);
        assert_eq!(recall_at_k(&ranked, &r, 10), 1.0);
    }

    // ----- mrr -----

    #[test]
    fn mrr_rank_one_is_one() {
        let r = relevant_set(["a"]);
        let ranked = ranked(&["a", "b"]);
        assert_eq!(mrr(&ranked, &r), 1.0);
    }

    #[test]
    fn mrr_rank_two_is_half() {
        let r = relevant_set(["a"]);
        let ranked = ranked(&["b", "a"]);
        assert_eq!(mrr(&ranked, &r), 0.5);
    }

    #[test]
    fn mrr_rank_three_is_one_third() {
        let r = relevant_set(["a"]);
        let ranked = ranked(&["x", "y", "a"]);
        assert!((mrr(&ranked, &r) - (1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn mrr_no_relevant_in_ranked_is_zero() {
        let r = relevant_set(["a"]);
        let ranked = ranked(&["b", "c"]);
        assert_eq!(mrr(&ranked, &r), 0.0);
    }

    #[test]
    fn mrr_uses_first_relevant_occurrence() {
        // a at rank 2 should win even though b at rank 1 is also relevant
        // — no, b wins because it's earlier. Confirms "first" semantics.
        let r = relevant_set(["a", "b"]);
        let ranked = ranked(&["b", "a", "c"]);
        assert_eq!(mrr(&ranked, &r), 1.0);
    }

    #[test]
    fn mrr_empty_relevant_is_one() {
        let r: HashSet<String> = HashSet::new();
        let ranked = ranked(&["a"]);
        assert_eq!(mrr(&ranked, &r), 1.0);
    }
}
