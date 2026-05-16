use std::path::Path;

use anyhow::{Context, Result};

use crate::embed::Embedder;
use crate::index::symbols::SymbolKind;
use crate::search::{MatchType, SearchResult};
use crate::store::reader::IndexReader;

/// Semantic search using HNSW index (fast) with brute-force fallback.
pub fn search_with_embedder(
    reader: &IndexReader,
    embedder: &mut dyn Embedder,
    query: &str,
    top_k: usize,
    hnsw_path: &Path,
) -> Result<Vec<SearchResult>> {
    let query_vec = embedder.embed(query).context("embed query")?;

    let scored = search_hnsw_at(hnsw_path, &query_vec, top_k, reader.symbol_count())
        .unwrap_or_else(|| search_brute_force(reader, &query_vec, top_k));

    let results = scored
        .into_iter()
        .filter_map(|(idx, score)| {
            let rec = reader.symbol(idx)?;
            let name = reader.read_string(rec.name_offset).to_string();
            let path = reader.read_string(rec.file_offset).to_string();
            let sig = {
                let s = reader.read_string(rec.signature_offset);
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            };

            Some(SearchResult {
                name,
                kind: SymbolKind::try_from(rec.kind)
                    .map_or("unknown", |k| k.as_str())
                    .to_string(),
                path,
                line: rec.line as usize,
                signature: sig,
                score: score as f64,
                match_type: MatchType::Semantic,
            })
        })
        .collect();

    Ok(results)
}

/// Search HNSW index at a specific path. Returns None if unavailable or inconsistent.
pub(crate) fn search_hnsw_at(
    hnsw_path: &Path,
    query_vec: &[f32],
    top_k: usize,
    expected_symbols: usize,
) -> Option<Vec<(usize, f32)>> {
    use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};

    if !hnsw_path.exists() {
        return None;
    }

    let options = IndexOptions {
        dimensions: query_vec.len(),
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    };

    let index = new_index(&options)
        .map_err(|e| tracing::warn!(error = %e, "failed to create HNSW index handle"))
        .ok()?;

    let path_str = hnsw_path.to_str()?;
    index
        .view(path_str)
        .map_err(|e| {
            tracing::warn!(path = %hnsw_path.display(), error = %e, "failed to view HNSW index")
        })
        .ok()?;

    if index.size() == 0 {
        return None;
    }

    // Validate HNSW key space matches current index
    if index.size() != expected_symbols {
        tracing::warn!(
            hnsw = index.size(),
            symbols = expected_symbols,
            "HNSW/index size mismatch — falling back to brute-force"
        );
        return None;
    }

    let results = index.search(query_vec, top_k).ok()?;

    let scored: Vec<(usize, f32)> = results
        .keys
        .iter()
        .zip(results.distances.iter())
        .map(|(&key, &dist)| {
            // usearch cosine distance = 1 - similarity
            let similarity = (1.0 - dist).max(0.0);
            (key as usize, similarity)
        })
        .collect();

    Some(scored)
}

/// Brute-force cosine similarity over all stored vectors. O(N).
fn search_brute_force(reader: &IndexReader, query_vec: &[f32], top_k: usize) -> Vec<(usize, f32)> {
    let mut scored: Vec<(usize, f32)> = Vec::new();

    for i in 0..reader.symbol_count() {
        if let Some(rec) = reader.symbol(i) {
            if let Some(vec) = reader.vector(rec.vector_index) {
                let sim = cosine_similarity(query_vec, vec);
                scored.push((i, sim));
            }
        }
    }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);
    scored
}

pub(crate) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_vectors_have_similarity_1() {
        let v = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn orthogonal_vectors_have_similarity_0() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn hnsw_missing_returns_none() {
        let path = Path::new("/nonexistent/index.hnsw");
        let query = vec![1.0, 2.0, 3.0];
        assert!(search_hnsw_at(path, &query, 10, 100).is_none());
    }
}
