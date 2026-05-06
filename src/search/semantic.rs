use anyhow::{Context, Result};

use crate::embed::Embedder;
use crate::search::{MatchType, SearchResult};
use crate::store::reader::IndexReader;

/// Semantic search: embed the query, then brute-force cosine similarity over stored vectors.
///
/// For small indexes (<100k symbols), brute-force is fast enough (~1ms).
/// TODO: add HNSW (usearch) for million-symbol codebases.
pub fn search_with_embedder(
    reader: &IndexReader,
    embedder: &mut Embedder,
    query: &str,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    let query_vec = embedder.embed(query).context("embed query")?;

    let mut scored: Vec<(usize, f32)> = Vec::new();

    for i in 0..reader.symbol_count() {
        if let Some(rec) = reader.symbol(i) {
            if let Some(vec) = reader.vector(rec.vector_index) {
                let sim = cosine_similarity(&query_vec, vec);
                scored.push((i, sim));
            }
        }
    }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);

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
                kind: super::structural::symbol_kind_str(rec.kind).to_string(),
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

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
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
}
