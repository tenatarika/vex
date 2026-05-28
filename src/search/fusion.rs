use std::collections::HashMap;

use crate::search::{MatchType, SearchResult};

/// Reciprocal Rank Fusion across an arbitrary number of ranked lists.
///
/// Results found in two or more lists are tagged `MatchType::Hybrid`; otherwise
/// the original list's `MatchType` is preserved on the merged record.
pub fn fuse_many(lists: Vec<Vec<SearchResult>>, limit: usize) -> Vec<SearchResult> {
    const K: f64 = 60.0;

    type Key = (String, String, usize); // (path, name, line)
    let mut scores: HashMap<Key, (f64, Option<SearchResult>, u32)> = HashMap::new();

    for list in lists {
        for (rank, result) in list.into_iter().enumerate() {
            let key = (result.path.clone(), result.name.clone(), result.line);
            let entry = scores.entry(key).or_insert((0.0, None, 0));
            entry.0 += 1.0 / (K + rank as f64);
            if entry.1.is_none() {
                entry.1 = Some(result);
                entry.2 = 1;
            } else {
                entry.2 += 1;
                if let Some(ref mut r) = entry.1 {
                    r.match_type = MatchType::Hybrid;
                }
            }
        }
    }

    let mut results: Vec<SearchResult> = scores
        .into_values()
        .filter_map(|(score, result, _)| {
            result.map(|mut r| {
                r.score = score;
                r
            })
        })
        .collect();

    // Tie-break on (path, name, line) so equal-score entries get a total
    // order independent of `HashMap` iteration. Without this, two runs over
    // the same inputs can produce different result orderings and break the
    // `rank_percentile_monotonic_descending` contract.
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.line.cmp(&b.line))
    });
    results.truncate(limit);
    results
}

/// Three-way RRF for `structural + bm25 + semantic`. Convenience wrapper.
pub fn fuse3(
    structural: Vec<SearchResult>,
    bm25: Vec<SearchResult>,
    semantic: Vec<SearchResult>,
    limit: usize,
) -> Vec<SearchResult> {
    fuse_many(vec![structural, bm25, semantic], limit)
}

/// Two-way RRF — back-compat wrapper around [`fuse_many`] for callers that
/// only need to merge `structural + semantic`. Prefer `fuse3` or `fuse_many`
/// for new code that wants the BM25 channel. Kept `pub` because integration
/// tests (`proptest_rerank`, `integration_test`) exercise it directly as
/// part of the public surface.
#[allow(dead_code)] // used by integration tests, not by the bin
pub fn fuse(
    structural: Vec<SearchResult>,
    semantic: Vec<SearchResult>,
    limit: usize,
) -> Vec<SearchResult> {
    fuse_many(vec![structural, semantic], limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(name: &str, score: f64, match_type: MatchType) -> SearchResult {
        SearchResult {
            name: name.to_string(),
            kind: "function".to_string(),
            path: "test.rs".to_string(),
            line: 1,
            signature: None,
            score,
            match_type,
        }
    }

    #[test]
    fn hybrid_results_rank_higher() {
        let structural = vec![
            make_result("Foo", 1.0, MatchType::Structural),
            make_result("Bar", 0.8, MatchType::Structural),
        ];
        let semantic = vec![
            make_result("Bar", 0.9, MatchType::Semantic),
            make_result("Baz", 0.7, MatchType::Semantic),
        ];

        let fused = fuse(structural, semantic, 10);
        // Bar appears in both lists, should rank highest
        assert_eq!(fused[0].name, "Bar");
        assert!(matches!(fused[0].match_type, MatchType::Hybrid));
    }

    fn at(name: &str, path: &str, line: usize) -> SearchResult {
        SearchResult {
            name: name.to_string(),
            kind: "function".to_string(),
            path: path.to_string(),
            line,
            signature: None,
            score: 0.0,
            match_type: MatchType::Structural,
        }
    }

    #[test]
    fn fuse_is_deterministic_for_equal_scores() {
        // H6 regression: when every result has identical RRF rank (e.g.
        // each list contributes one disjoint result at rank 0), the
        // final ordering must be a stable function of `(path, name,
        // line)` rather than `HashMap` iteration order.
        let make = || {
            vec![
                vec![at("alpha", "z.rs", 10)],
                vec![at("beta", "a.rs", 20)],
                vec![at("gamma", "m.rs", 5)],
                vec![at("delta", "b.rs", 1)],
            ]
        };
        let first = fuse_many(make(), 10);
        for _ in 0..16 {
            let next = fuse_many(make(), 10);
            let names_first: Vec<_> = first.iter().map(|r| (&r.path, &r.name, r.line)).collect();
            let names_next: Vec<_> = next.iter().map(|r| (&r.path, &r.name, r.line)).collect();
            assert_eq!(
                names_first, names_next,
                "fuse_many produced different orderings across runs"
            );
        }
        // The deterministic ordering is `(path, name, line)` ascending
        // when all scores tie. `a.rs` < `b.rs` < `m.rs` < `z.rs`.
        let paths: Vec<&str> = first.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "b.rs", "m.rs", "z.rs"]);
    }
}
