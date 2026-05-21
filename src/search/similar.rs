//! Semantic similarity over the existing vectors + HNSW.
//!
//! `find_similar` resolves a symbol by name, fetches its stored embedding,
//! and returns the top-K nearest neighbors (excluding the symbol itself)
//! filtered by a cosine similarity threshold.
//!
//! `find_duplicates` scans every symbol with a vector and surfaces pairs
//! whose similarity exceeds the threshold. Pairs are deduplicated by
//! canonical `(min_idx, max_idx)` ordering and short bodies are skipped
//! to suppress matches between trivial 1-liners.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::index::symbols::SymbolKind;
use crate::search::semantic;
use crate::store::reader::IndexReader;

/// A symbol that is semantically close to a query symbol.
#[derive(Debug, Clone)]
pub struct SimilarMatch {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: usize,
    pub signature: Option<String>,
    pub similarity: f32,
}

/// Find symbols semantically similar to `target_name`.
///
/// - `limit` — max neighbors to return after self-exclusion and threshold filter
/// - `threshold` — minimum cosine similarity in `0.0..=1.0`
///
/// Errors when:
/// - the index has no vectors (`vex index --semantic` wasn't run)
/// - the target symbol does not exist
/// - the target symbol exists but has no embedding (`vector_index == u32::MAX`)
pub fn find_similar(
    reader: &IndexReader,
    hnsw_path: &Path,
    target_name: &str,
    limit: usize,
    threshold: f32,
) -> Result<Vec<SimilarMatch>> {
    if !reader.has_vectors() {
        bail!(
            "index has no vectors — run `vex index --semantic` to build embeddings, \
             then retry with `vex similar`"
        );
    }

    let target_idx = resolve_symbol_idx(reader, target_name)?;

    let target_rec = reader
        .symbol(target_idx)
        .context("symbol index out of bounds after resolution")?;

    if target_rec.vector_index == u32::MAX {
        bail!(
            "symbol `{target_name}` exists in the index but has no embedding — \
             re-run `vex index --semantic` to generate vectors for all symbols"
        );
    }

    let target_vec = reader
        .vector(target_rec.vector_index)
        .context("failed to read target vector from index")?;

    // Try HNSW first; fall back to brute-force.
    let scored = nearest_neighbors(reader, target_vec, limit + 1, hnsw_path);

    // Filter: drop self, drop below threshold, drop out-of-bounds indices
    // (defence-in-depth against stale HNSW returning bad keys), sort
    // descending, truncate.
    let mut results: Vec<SimilarMatch> = scored
        .into_iter()
        .filter(|&(idx, sim)| idx != target_idx && sim >= threshold)
        .filter_map(|(idx, sim)| build_match(reader, idx, sim))
        .collect();

    results.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);

    Ok(results)
}

/// Find pairs of symbols whose cosine similarity exceeds `threshold`.
///
/// - `min_body_lines` — symbols with fewer body lines are skipped (suppresses
///   trivial 1-liner matches such as getters)
/// - `limit` — caps the number of returned pairs
///
/// Pairs are returned in descending similarity order. Each unordered pair
/// `{A, B}` appears at most once.
pub fn find_duplicates(
    reader: &IndexReader,
    hnsw_path: &Path,
    threshold: f32,
    min_body_lines: usize,
    limit: usize,
) -> Result<Vec<(SimilarMatch, SimilarMatch)>> {
    // Zero-symbol index — return empty immediately (before the vector check).
    if reader.symbol_count() == 0 {
        return Ok(vec![]);
    }

    if !reader.has_vectors() {
        bail!(
            "index has no vectors — run `vex index --semantic` to build embeddings, \
             then retry with `vex duplicates`"
        );
    }

    let body_lines = compute_body_lines(reader);

    // Search for top-K neighbors per symbol. K is derived from the requested
    // pair `limit` so dense clusters (many similar symbols) don't silently
    // miss pairs: a hardcoded K=6 would cap pair coverage in any cluster
    // larger than ~6 elements. Bounded by `symbol_count` to avoid asking HNSW
    // for more keys than exist. Minimum of 6 = 5 neighbors + self.
    let neighbors_per_sym = limit.saturating_add(1).max(6).min(reader.symbol_count());

    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new(); // (similarity, a, b)

    for sym_idx in 0..reader.symbol_count() {
        let rec = match reader.symbol(sym_idx) {
            Some(r) => r,
            None => continue,
        };

        // Skip symbols without a vector.
        if rec.vector_index == u32::MAX {
            continue;
        }

        // Skip symbols with short bodies.
        if body_lines[sym_idx] < min_body_lines {
            continue;
        }

        let query_vec = match reader.vector(rec.vector_index) {
            Some(v) => v,
            None => continue,
        };

        let scored = nearest_neighbors(reader, query_vec, neighbors_per_sym, hnsw_path);

        for (other_idx, sim) in scored {
            // Drop self-pairs.
            if other_idx == sym_idx {
                continue;
            }

            // Bounds-check before indexing body_lines (defence-in-depth
            // against HNSW returning out-of-bounds keys).
            let Some(&other_body_lines) = body_lines.get(other_idx) else {
                continue;
            };

            // Both members must satisfy min_body_lines.
            if other_body_lines < min_body_lines {
                continue;
            }

            // Drop below threshold.
            if sim < threshold {
                continue;
            }

            // Canonical dedup.
            let key = (sym_idx.min(other_idx), sym_idx.max(other_idx));
            if seen.insert(key) {
                pairs.push((sim, key.0, key.1));
            }
        }
    }

    // Sort descending by similarity.
    pairs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    pairs.truncate(limit);

    let result = pairs
        .into_iter()
        .filter_map(|(sim, a, b)| {
            let ma = build_match(reader, a, sim)?;
            let mb = build_match(reader, b, sim)?;
            Some((ma, mb))
        })
        .collect();

    Ok(result)
}

/// Run HNSW (if available) or brute-force nearest-neighbor search.
///
/// Returns up to `top_k` `(symbol_index, cosine_similarity)` pairs sorted
/// descending by similarity. Self may be included — callers filter it out.
fn nearest_neighbors(
    reader: &IndexReader,
    query_vec: &[f32],
    top_k: usize,
    hnsw_path: &Path,
) -> Vec<(usize, f32)> {
    if let Some(scored) =
        semantic::search_hnsw_at(hnsw_path, query_vec, top_k, reader.symbol_count())
    {
        return scored;
    }

    // Brute-force fallback.
    let mut scored: Vec<(usize, f32)> = Vec::new();
    for i in 0..reader.symbol_count() {
        if let Some(rec) = reader.symbol(i) {
            if let Some(vec) = reader.vector(rec.vector_index) {
                let sim = semantic::cosine_similarity(query_vec, vec);
                scored.push((i, sim));
            }
        }
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);
    scored
}

/// Approximate body line count per symbol.
///
/// Body length ≈ `next_symbol_in_same_file.line − this_symbol.line`.
/// The last symbol in each file gets `usize::MAX` (treat as arbitrarily long —
/// we cannot measure it, so we don't penalise it).
fn compute_body_lines(reader: &IndexReader) -> Vec<usize> {
    let count = reader.symbol_count();
    let mut body_lines = vec![usize::MAX; count];

    for (sym_idx, entry) in body_lines.iter_mut().enumerate() {
        let rec = match reader.symbol(sym_idx) {
            Some(r) => r,
            None => continue,
        };
        let this_file = rec.file_offset;
        let this_line = rec.line as usize;

        // Look at the next symbol in the array.
        if sym_idx + 1 < count {
            if let Some(next) = reader.symbol(sym_idx + 1) {
                if next.file_offset == this_file {
                    // Same file: delta is the body approximation.
                    let next_line = next.line as usize;
                    *entry = next_line.saturating_sub(this_line);
                }
                // Different file: leave as usize::MAX (last in its file).
            }
        }
        // Last symbol in entire index: already usize::MAX.
    }

    body_lines
}

/// Resolve `target_name` to a `SimilarMatch` record (the *seed* of a
/// `vex similar` call). Lets `--explain` extract the seed body without a
/// second FST round-trip through `structural::search_with_fuzzy` — both
/// would resolve the same record, but going through this entry point
/// keeps the two paths in sync and clearly signals intent.
///
/// `similarity` is set to `1.0` — the seed is identical to itself, so any
/// downstream consumer that displays the score sees the expected value.
pub fn resolve_seed_match(reader: &IndexReader, target_name: &str) -> Option<SimilarMatch> {
    let idx = resolve_symbol_idx(reader, target_name).ok()?;
    build_match(reader, idx, 1.0)
}

/// Resolve a symbol name to its symbol index via the symbol FST.
/// Returns the first matching index (FST ordering — typically the most relevant).
pub(crate) fn resolve_symbol_idx(reader: &IndexReader, target_name: &str) -> Result<usize> {
    let fst = reader
        .symbol_fst_reader()
        .context("index has no symbol FST — re-run `vex index`")?;
    let indices = fst.find(target_name);
    if let Some(&idx) = indices.first() {
        return Ok(idx as usize);
    }
    bail!("symbol not found: {target_name}")
}

/// Convert a symbol record into a `SimilarMatch` with the given similarity score.
///
/// Returns `None` when `sym_idx` does not point at a valid symbol record. This
/// can happen if the HNSW graph was built against a different (e.g. stale)
/// version of the index and returns out-of-bounds keys — callers MUST filter
/// `None` rather than unwrap.
pub(crate) fn build_match(
    reader: &IndexReader,
    sym_idx: usize,
    similarity: f32,
) -> Option<SimilarMatch> {
    let rec = reader.symbol(sym_idx)?;
    let name = reader.read_string(rec.name_offset).to_string();
    let path = reader.read_string(rec.file_offset).to_string();
    let signature = {
        let s = reader.read_string(rec.signature_offset);
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };
    let kind = SymbolKind::try_from(rec.kind)
        .map_or("unknown", |k| k.as_str())
        .to_string();

    Some(SimilarMatch {
        name,
        kind,
        path,
        line: rec.line as usize,
        signature,
        similarity,
    })
}
