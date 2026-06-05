use std::path::Path;

use anyhow::{Context, Result};

use crate::embed::Embedder;
use crate::index::symbols::SymbolKind;
use crate::search::{MatchType, SearchResult};
use crate::store::reader::IndexReader;

/// Semantic search using HNSW index (fast) with brute-force fallback.
///
/// `normalized` reflects the manifest's `vectors_normalized` flag —
/// when `true`, the stored vectors are unit-length and the brute-force
/// fallback uses dot product instead of full cosine. The query vector
/// is normalized here so it matches.
pub fn search_with_embedder(
    reader: &IndexReader,
    embedder: &mut dyn Embedder,
    query: &str,
    top_k: usize,
    hnsw_path: &Path,
    normalized: bool,
) -> Result<Vec<SearchResult>> {
    let mut query_vec = embedder.embed(query).context("embed query")?;
    if normalized {
        normalize_in_place(&mut query_vec);
    }

    let scored = search_hnsw_at(hnsw_path, &query_vec, top_k, reader.symbol_count())
        .unwrap_or_else(|| search_brute_force(reader, &query_vec, top_k, normalized));

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

/// v1.13 P1: an opened HNSW index handle, reusable across many
/// queries. The previous `search_hnsw_at` created + `view()`-ed a
/// fresh `usearch::Index` per call — `find_duplicates`'s outer loop
/// then mmap'd the HNSW file `symbol_count` times per invocation. The
/// handle lifts that cost to once per query.
pub(crate) struct HnswHandle {
    index: usearch::Index,
}

impl HnswHandle {
    /// Open the HNSW file at `hnsw_path` and prepare it for repeated
    /// `search()` calls. Returns `None` for the same reasons
    /// `search_hnsw_at` previously returned `None`:
    /// - file missing
    /// - index creation / view failed
    /// - the persisted index is empty
    /// - the persisted index size disagrees with `expected_symbols`
    ///   (stale HNSW relative to the live index — caller should fall
    ///   back to brute-force).
    pub(crate) fn open(
        hnsw_path: &Path,
        query_dim: usize,
        expected_symbols: usize,
    ) -> Option<Self> {
        use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};

        if !hnsw_path.exists() {
            return None;
        }

        let options = IndexOptions {
            dimensions: query_dim,
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

        if index.size() != expected_symbols {
            tracing::warn!(
                hnsw = index.size(),
                symbols = expected_symbols,
                "HNSW/index size mismatch — falling back to brute-force"
            );
            return None;
        }

        Some(Self { index })
    }

    /// Query the opened index. Returns `None` only if usearch reports
    /// a search error; an empty result set is `Some(Vec::new())`.
    pub(crate) fn search(&self, query_vec: &[f32], top_k: usize) -> Option<Vec<(usize, f32)>> {
        let results = self.index.search(query_vec, top_k).ok()?;
        Some(
            results
                .keys
                .iter()
                .zip(results.distances.iter())
                .map(|(&key, &dist)| {
                    // usearch cosine distance = 1 - similarity
                    let similarity = (1.0 - dist).max(0.0);
                    (key as usize, similarity)
                })
                .collect(),
        )
    }
}

/// Single-call wrapper kept for the search-with-embedder path and for
/// any caller that does exactly one HNSW lookup per `vex` invocation.
/// New multi-query loops should open a [`HnswHandle`] once instead.
pub(crate) fn search_hnsw_at(
    hnsw_path: &Path,
    query_vec: &[f32],
    top_k: usize,
    expected_symbols: usize,
) -> Option<Vec<(usize, f32)>> {
    HnswHandle::open(hnsw_path, query_vec.len(), expected_symbols)?.search(query_vec, top_k)
}

/// Brute-force cosine similarity over all stored vectors. O(N).
///
/// `normalized` gates the v1.13 P5 fast path: when `true`, vectors are
/// already L2-normalized so `dot_product` is mathematically equivalent
/// to cosine and skips the per-call `sqrt` + norm computations.
fn search_brute_force(
    reader: &IndexReader,
    query_vec: &[f32],
    top_k: usize,
    normalized: bool,
) -> Vec<(usize, f32)> {
    let mut scored: Vec<(usize, f32)> = Vec::new();

    for i in 0..reader.symbol_count() {
        if let Some(rec) = reader.symbol(i) {
            if let Some(vec) = reader.vector(rec.vector_index) {
                let sim = if normalized {
                    dot_product(query_vec, vec)
                } else {
                    cosine_similarity(query_vec, vec)
                };
                scored.push((i, sim));
            }
        }
    }

    // Tie-break on symbol index so equal-similarity scores produce a
    // deterministic ordering across runs. Without this, two `vex search
    // --semantic` invocations on the same index can disagree at the
    // tie-break boundary.
    //
    // Note: `docs/RANKING-EVAL.md` specifies `(path, name, line)` as the
    // canonical total-order secondary key. This per-channel pre-fusion
    // sort uses `sym_idx` instead because (a) the surrounding `fuse_many`
    // re-applies the `(path, name, line)` order at the fusion layer where
    // results have a `SearchResult` shape, and (b) resolving the symbol
    // index back to a `SearchResult` here would require an extra reader
    // lookup per comparison. `sym_idx` is itself deterministic (writer
    // assembly order is fixed), so the contract is preserved end-to-end
    // even though this specific sort uses a coarser key.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
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

/// v1.13 P5 fast path. Equivalent to [`cosine_similarity`] **only when
/// both inputs are L2-normalized**. Skips the per-call `sqrt` + norm
/// computations. Caller must guarantee the inputs are unit vectors;
/// the brute-force search loop gates on the manifest's
/// `vectors_normalized` flag.
#[inline]
pub(crate) fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// L2-normalize `v` in place. No-op on zero vectors (would otherwise
/// divide by zero — they remain all-zero, which `dot_product` will
/// score as 0.0, matching `cosine_similarity`'s short-circuit).
pub(crate) fn normalize_in_place(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
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
    fn normalize_in_place_produces_unit_vector() {
        let mut v = vec![3.0, 4.0]; // norm = 5
        normalize_in_place(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn normalize_in_place_zero_vector_stays_zero() {
        // Guard against NaN — without the `if norm > 0.0` short-circuit
        // we'd divide by zero and produce a NaN-laden vector that
        // downstream `dot_product` would propagate as a similarity score.
        let mut v = vec![0.0_f32; 4];
        normalize_in_place(&mut v);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    /// `dot_product` on two normalized vectors must equal
    /// `cosine_similarity` within float epsilon — this is the
    /// equivalence the v1.13 P5 fast path leans on. If a future refactor
    /// breaks the relationship (e.g. by changing one fn's reduction
    /// order), this test fails before the result reaches users.
    #[test]
    fn dot_product_equals_cosine_for_normalized() {
        let cases: &[(Vec<f32>, Vec<f32>)] = &[
            (vec![1.0, 0.0, 0.0], vec![1.0, 0.0, 0.0]), // identical
            (vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]), // orthogonal
            (vec![1.0, 2.0, 3.0, 4.0], vec![4.0, 3.0, 2.0, 1.0]),
            (vec![0.1, -0.5, 0.7, 0.2], vec![-0.3, 0.6, 0.1, 0.8]),
        ];
        for (a_raw, b_raw) in cases {
            let mut a = a_raw.clone();
            let mut b = b_raw.clone();
            normalize_in_place(&mut a);
            normalize_in_place(&mut b);
            let cosine = cosine_similarity(&a, &b);
            let dot = dot_product(&a, &b);
            assert!(
                (cosine - dot).abs() < 1e-6,
                "dot_product diverged from cosine_similarity for normalized inputs: \
                 cosine={cosine}, dot={dot}, a={a:?}, b={b:?}"
            );
        }
    }

    #[test]
    fn hnsw_missing_returns_none() {
        let path = Path::new("/nonexistent/index.hnsw");
        let query = vec![1.0, 2.0, 3.0];
        assert!(search_hnsw_at(path, &query, 10, 100).is_none());
    }
}
