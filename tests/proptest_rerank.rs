use proptest::prelude::*;
use vex::search::fusion::fuse;
use vex::search::rerank::{rerank, RerankContext};
use vex::search::{MatchType, SearchResult};

// ---------------------------------------------------------------------------
// Arbitrary SearchResult strategy
// ---------------------------------------------------------------------------

fn arb_match_type() -> impl Strategy<Value = MatchType> {
    prop_oneof![
        Just(MatchType::Structural),
        Just(MatchType::Semantic),
        Just(MatchType::Hybrid),
        Just(MatchType::Fuzzy),
    ]
}

fn arb_search_result() -> impl Strategy<Value = SearchResult> {
    let kinds = prop_oneof![
        Just("function"),
        Just("class"),
        Just("struct"),
        Just("method"),
        Just("trait"),
        Just("enum"),
        Just("interface"),
        Just("constant"),
        Just("impl"),
        Just("property"),
    ];
    (
        "[a-zA-Z_][a-zA-Z0-9_]{0,30}",             // name
        kinds,                                     // kind
        "src(/[a-z]{1,10}){0,5}/[a-z]{1,10}\\.rs", // path
        1..10000usize,                             // line
        0.001f64..100.0f64,                        // score — positive, finite, no edge cases
        arb_match_type(),
    )
        .prop_map(|(name, kind, path, line, score, match_type)| SearchResult {
            name,
            kind: kind.to_string(),
            path,
            line,
            signature: None,
            score,
            match_type,
        })
}

// ---------------------------------------------------------------------------
// 1. rerank_preserves_length
//    Reranking must never drop or duplicate results.
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn rerank_preserves_length(results in prop::collection::vec(arb_search_result(), 1..100)) {
        let ctx = RerankContext::default();
        let n = results.len();
        let ranked = rerank("query", &ctx, results);
        prop_assert_eq!(ranked.len(), n);
    }
}

// ---------------------------------------------------------------------------
// 2. rerank_output_is_sorted_descending
//    The output of rerank must always be in non-increasing score order.
//    This is the core ordering contract of the function.
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn rerank_output_is_sorted_descending(
        results in prop::collection::vec(arb_search_result(), 2..100),
    ) {
        let ctx = RerankContext::default();
        let ranked = rerank("SomeQuery", &ctx, results);

        for window in ranked.windows(2) {
            let (a, b) = (&window[0], &window[1]);
            prop_assert!(
                a.score >= b.score,
                "expected scores in non-increasing order but got {} > {} \
                 (name='{}' before name='{}')",
                b.score, a.score, a.name, b.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3. rerank_no_nan_or_inf
//    After reranking, no score must be NaN or infinite.
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn rerank_no_nan_or_inf(results in prop::collection::vec(arb_search_result(), 1..100)) {
        let ctx = RerankContext::default();
        let ranked = rerank("search_query", &ctx, results);
        for r in &ranked {
            prop_assert!(
                r.score.is_finite(),
                "score for '{}' is not finite: {}",
                r.name,
                r.score
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. rerank_no_negative_scores
//    All boosts applied by rerank are positive multipliers, so non-negative
//    input scores must remain non-negative after reranking.
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn rerank_no_negative_scores(results in prop::collection::vec(arb_search_result(), 1..100)) {
        // arb_search_result already generates scores in 0.001..100.0, so all
        // inputs are positive. We verify the invariant is preserved after all
        // multiplicative boosts.
        let ctx = RerankContext::default();
        let ranked = rerank("some_query", &ctx, results);
        for r in &ranked {
            prop_assert!(
                r.score >= 0.0,
                "score for '{}' should be non-negative, got {}",
                r.name,
                r.score
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 5. fusion_commutativity
//    fuse(A, B, limit) and fuse(B, A, limit) must contain the same set of
//    (name, path, line) triples when limit is large enough to return every
//    unique result (i.e. limit >= |A| + |B|).  When limit truncates, equal-
//    score items at the boundary may be selected differently by the unstable
//    sort depending on argument order, so we avoid that ambiguity by
//    guaranteeing no truncation.
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn fusion_commutativity(
        a in prop::collection::vec(arb_search_result(), 1..25),
        b in prop::collection::vec(arb_search_result(), 1..25),
    ) {
        // Use a limit large enough to capture all distinct results from both
        // lists, so no tie-breaking truncation can cause divergence.
        let limit = a.len() + b.len();

        let fused_ab = fuse(a.clone(), b.clone(), limit);
        let fused_ba = fuse(b, a, limit);

        // Collect and sort (name, path, line) keys for set comparison.
        let mut keys_ab: Vec<(String, String, usize)> = fused_ab
            .iter()
            .map(|r| (r.name.clone(), r.path.clone(), r.line))
            .collect();
        let mut keys_ba: Vec<(String, String, usize)> = fused_ba
            .iter()
            .map(|r| (r.name.clone(), r.path.clone(), r.line))
            .collect();

        keys_ab.sort_unstable();
        keys_ba.sort_unstable();

        prop_assert_eq!(
            keys_ab,
            keys_ba,
            "fuse(A,B) and fuse(B,A) must contain the same (name, path, line) triples \
             when limit covers all results"
        );
    }
}
