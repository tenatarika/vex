use super::Signals;

use crate::search::SearchResult;

/// Build per-result signals by re-keying pre-fusion channel lists onto merged results.
/// Keying mirrors `fusion::fuse_many` — `(path, name, line)` tuple.
///
/// Stage 1: signature only. Body is `todo!()`. Implementation lands in Stage 3.
pub fn build_signals(
    structural: &[SearchResult],
    bm25: &[SearchResult],
    semantic: &[SearchResult],
    merged: &[SearchResult],
) -> Vec<Signals> {
    let _ = (structural, bm25, semantic, merged);
    todo!("Phase 13.11 Stage 3: build_signals via (path, name, line) keying")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{MatchType, SearchResult};

    fn make_result(name: &str, path: &str, line: usize) -> SearchResult {
        SearchResult {
            name: name.to_string(),
            kind: "fn".to_string(),
            path: path.to_string(),
            line,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        }
    }

    #[test]
    #[should_panic(expected = "Phase 13.11 Stage 3")]
    fn build_signals_returns_one_entry_per_merged_result() {
        // After Stage 3 this should return Vec<Signals> with len == merged.len().
        let r0 = make_result("alpha", "src/a.rs", 1);
        let r1 = make_result("beta", "src/b.rs", 5);
        let merged = vec![r0.clone(), r1.clone()];
        let signals = build_signals(&[r0], &[], &[r1], &merged);
        assert_eq!(signals.len(), 2, "one Signals entry per merged result");
    }

    #[test]
    #[should_panic(expected = "Phase 13.11 Stage 3")]
    fn build_signals_assigns_bm25_rank_by_pre_fusion_position() {
        // Pre-fusion BM25 list [r0, r1, r2]; merged contains r2.
        // Expected: signals for r2 has bm25_rank == Some(2) (0-indexed position).
        let r0 = make_result("alpha", "src/a.rs", 1);
        let r1 = make_result("beta", "src/b.rs", 5);
        let r2 = make_result("gamma", "src/c.rs", 10);
        let bm25_list = vec![r0.clone(), r1.clone(), r2.clone()];
        let merged = vec![r2.clone()];
        let signals = build_signals(&[], &bm25_list, &[], &merged);
        assert_eq!(
            signals[0].bm25_rank,
            Some(2),
            "r2 is at position 2 in bm25_list so bm25_rank must be Some(2)"
        );
    }

    #[test]
    #[should_panic(expected = "Phase 13.11 Stage 3")]
    fn build_signals_assigns_semantic_rank_by_pre_fusion_position() {
        // Pre-fusion semantic list [r0, r1]; merged contains r0.
        // Expected: signals for r0 has semantic_rank == Some(0).
        let r0 = make_result("alpha", "src/a.rs", 1);
        let r1 = make_result("beta", "src/b.rs", 5);
        let semantic_list = vec![r0.clone(), r1.clone()];
        let merged = vec![r0.clone()];
        let signals = build_signals(&[], &[], &semantic_list, &merged);
        assert_eq!(
            signals[0].semantic_rank,
            Some(0),
            "r0 is at position 0 in semantic_list so semantic_rank must be Some(0)"
        );
    }

    #[test]
    #[should_panic(expected = "Phase 13.11 Stage 3")]
    fn build_signals_omits_channels_where_result_did_not_appear() {
        // result only in BM25 — semantic_rank must be None and fst_hit false.
        let r = make_result("only_bm25", "src/x.rs", 3);
        let merged = vec![r.clone()];
        let signals = build_signals(&[], &[r], &[], &merged);
        assert_eq!(
            signals[0].semantic_rank, None,
            "result absent from semantic list must have semantic_rank == None"
        );
        assert!(
            !signals[0].fst_hit,
            "result absent from structural list must have fst_hit == false"
        );
    }

    #[test]
    #[should_panic(expected = "Phase 13.11 Stage 3")]
    fn build_signals_handles_duplicate_keys_across_channels() {
        // Same (path, name, line) in both FST (structural) and BM25.
        // Merged result should have both ranks populated and fst_hit == true.
        let r = make_result("shared", "src/shared.rs", 7);
        let merged = vec![r.clone()];
        let signals = build_signals(&[r.clone()], &[r.clone()], &[], &merged);
        assert!(
            signals[0].fst_hit,
            "result present in structural list must have fst_hit == true"
        );
        assert_eq!(
            signals[0].bm25_rank,
            Some(0),
            "result at position 0 in bm25 list must have bm25_rank == Some(0)"
        );
    }

    #[test]
    #[should_panic(expected = "Phase 13.11 Stage 3")]
    fn build_signals_fst_hit_true_when_in_structural() {
        // fst_hit is keyed on (path, name, line) membership in structural list.
        let r = make_result("fst_symbol", "src/lib.rs", 42);
        let merged = vec![r.clone()];
        let signals = build_signals(&[r], &[], &[], &merged);
        assert!(
            signals[0].fst_hit,
            "result in structural list must have fst_hit == true"
        );
    }
}
