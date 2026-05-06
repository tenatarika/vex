use crate::search::SearchResult;

/// Semantic search via HNSW vector similarity.
///
/// Phase 2: will use usearch + ort for ONNX embedding inference.
/// For now this is a placeholder.
pub fn search(
    _query_embedding: &[f32],
    _top_k: usize,
) -> Vec<SearchResult> {
    // TODO: implement HNSW search with usearch crate
    Vec::new()
}
