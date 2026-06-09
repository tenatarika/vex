//! GPU-memory-aware micro-batching for embedding.
//!
//! Transformer attention memory and compute scale as `batch × seq²`. vex feeds
//! contexts up to ~256 tokens, and fastembed pads every inference batch to the
//! longest sequence *in that batch*. With the naive approach (fixed batch of
//! 256 in arbitrary symbol order) a single batch that happens to contain a few
//! long C++ symbols processes all 256 at the maximum length — allocating >10 GB
//! of VRAM and wasting most of the compute on padding. On a large repo that
//! both hogs the GPU and runs an order of magnitude slower than it should.
//!
//! This module sizes each inference batch from the *actual* context lengths, so
//! peak VRAM stays bounded with **no user configuration**: contexts are sorted
//! by length and grouped greedily so `batch_count × max_len²` never exceeds a
//! fixed budget (capped at [`MAX_BATCH`] items). Short contexts therefore batch
//! in bulk (fast); long contexts fall into small batches (bounded VRAM). Output
//! vectors are returned in the original input order. See `docs/GPU_SUPPORT.md`.

use anyhow::Result;
use fastembed::TextEmbedding;

/// Budget on `batch_count × max_len²` (`max_len` in UTF-8 bytes — a monotonic,
/// conservative proxy for token count: bytes ≥ chars ≥ tokens). Tuned so a worst-case batch peaks ~2 GB on MiniLM-L6-v2, which
/// keeps vex from monopolising a shared GPU. Overridable via the
/// `VEX_GPU_ATTN_BUDGET` env var for tuning; the default needs no config.
const DEFAULT_ATTN_BUDGET: usize = 40_000_000;

/// Hard cap on items per inference batch. Bounds the *linear* per-batch buffers
/// (FFN / BiasGelu ≈ `count × tokens × ffn_hidden`) that the quadratic
/// attention budget alone doesn't constrain — a huge-count short-context batch
/// otherwise needs a single ~600 MB allocation that won't fit the capped CUDA
/// arena. At 256 the largest single allocation stays ≲150 MB. See
/// `docs/GPU_SUPPORT.md`.
const MAX_BATCH: usize = 256;

fn attn_budget() -> usize {
    std::env::var("VEX_GPU_ATTN_BUDGET")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_ATTN_BUDGET)
}

/// Embed `texts` with length-aware micro-batching (see module docs). Returns
/// one vector per input, in input order.
pub fn embed_length_aware(model: &mut TextEmbedding, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let budget = attn_budget();
    // Indices sorted by context length (ascending) so each batch is
    // length-homogeneous — eliminating the padding waste of mixed batches.
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&i| texts[i].len());

    let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
    let mut start = 0;
    while start < order.len() {
        // Grow the batch while `count × max_len²` stays within budget (and
        // under MAX_BATCH). Because `order` is ascending, the longest item in
        // the window is the one being added, so `max_len` is its length.
        let mut end = start;
        while end < order.len() {
            let count = end - start + 1;
            let max_len = texts[order[end]].len().max(1);
            let over_budget = count * max_len * max_len > budget;
            if (over_budget || count > MAX_BATCH) && end > start {
                break;
            }
            end += 1;
        }
        let batch_idx = &order[start..end];
        let batch: Vec<&str> = batch_idx.iter().map(|&i| texts[i].as_str()).collect();
        let n = batch.len();
        // `Some(n)` forces a single fastembed inference batch of exactly this
        // size — we've already chosen it; don't let fastembed re-chunk.
        let vectors = model.embed(batch, Some(n))?;
        // Move each vector into its original slot (no clone): `vectors` and
        // `batch_idx` are equal length — we forced a single batch of exactly `n`.
        for (vec, &i) in vectors.into_iter().zip(batch_idx) {
            out[i] = vec;
        }
        start = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    // Length-batching arithmetic is exercised here without loading a model;
    // the model-backed path is covered by the CLI/index integration tests.
    const BUDGET: usize = 40_000_000;
    const MAX_BATCH: usize = 256;

    /// Re-implements the windowing loop over lengths to assert the invariant
    /// `count × max_len² ≤ budget` (and `count ≤ MAX_BATCH`) for every batch,
    /// with a single over-budget item allowed to stand alone.
    fn batches(lens: &[usize]) -> Vec<Vec<usize>> {
        let mut order: Vec<usize> = (0..lens.len()).collect();
        order.sort_by_key(|&i| lens[i]);
        let mut out = Vec::new();
        let mut start = 0;
        while start < order.len() {
            let mut end = start;
            while end < order.len() {
                let count = end - start + 1;
                let max_len = lens[order[end]].max(1);
                if (count * max_len * max_len > BUDGET || count > MAX_BATCH) && end > start {
                    break;
                }
                end += 1;
            }
            out.push(order[start..end].to_vec());
            start = end;
        }
        out
    }

    #[test]
    fn short_contexts_batch_in_bulk_long_ones_shrink() {
        // 5000 short (10-char) + a handful of long (1000-char) contexts.
        let mut lens = vec![10usize; 5000];
        lens.extend([1000, 1000, 1000]);
        let groups = batches(&lens);
        for g in &groups {
            let max_len = g.iter().map(|&i| lens[i].max(1)).max().unwrap();
            assert!(
                g.len() * max_len * max_len <= BUDGET || g.len() == 1,
                "batch of {} at max_len {} exceeds budget",
                g.len(),
                max_len
            );
            assert!(g.len() <= MAX_BATCH);
        }
        // Every input embedded exactly once.
        let total: usize = groups.iter().map(|g| g.len()).sum();
        assert_eq!(total, lens.len());
        // Long contexts (1000²·n ≤ 40M → n ≤ 40) batch far smaller than short.
        let long_group = groups
            .iter()
            .find(|g| g.iter().any(|&i| lens[i] == 1000))
            .unwrap();
        assert!(long_group.len() <= 40);
    }

    #[test]
    fn single_oversized_context_stands_alone() {
        let lens = [10, 100_000]; // 100k² ≫ budget
        let groups = batches(&lens);
        // The oversized one is its own batch; nothing is dropped.
        assert_eq!(groups.iter().map(|g| g.len()).sum::<usize>(), 2);
        assert!(groups.iter().any(|g| g.len() == 1 && lens[g[0]] == 100_000));
    }
}
