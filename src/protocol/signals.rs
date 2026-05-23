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
