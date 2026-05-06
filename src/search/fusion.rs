use std::collections::HashMap;

use crate::search::{MatchType, SearchResult};

/// Reciprocal Rank Fusion: merge ranked lists from structural + semantic search.
///
/// RRF score = sum( 1 / (k + rank) ) for each list the result appears in.
/// Results in both lists get boosted (higher combined score).
pub fn fuse(
    structural: Vec<SearchResult>,
    semantic: Vec<SearchResult>,
    limit: usize,
) -> Vec<SearchResult> {
    const K: f64 = 60.0;

    type Key = (String, String, usize); // (path, name, line)
    let mut scores: HashMap<Key, (f64, Option<SearchResult>)> = HashMap::new();

    for (rank, result) in structural.into_iter().enumerate() {
        let key = (result.path.clone(), result.name.clone(), result.line);
        let entry = scores.entry(key).or_insert((0.0, None));
        entry.0 += 1.0 / (K + rank as f64);
        entry.1 = Some(result);
    }

    for (rank, result) in semantic.into_iter().enumerate() {
        let key = (result.path.clone(), result.name.clone(), result.line);
        let entry = scores.entry(key).or_insert((0.0, None));
        entry.0 += 1.0 / (K + rank as f64);
        if entry.1.is_none() {
            entry.1 = Some(result);
        } else {
            // Found in both lists — mark as hybrid
            if let Some(ref mut r) = entry.1 {
                r.match_type = MatchType::Hybrid;
            }
        }
    }

    let mut results: Vec<SearchResult> = scores
        .into_values()
        .filter_map(|(score, result)| {
            result.map(|mut r| {
                r.score = score;
                r
            })
        })
        .collect();

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);
    results
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
}
