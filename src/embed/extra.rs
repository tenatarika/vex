//! Additional fastembed-backed embedders beyond the default MiniLM-L6-v2.
//!
//! These enable heavier and/or code-specialized models. MiniLM-L6-v2 (~22M
//! params) is too small to be compute-bound, so a GPU execution provider gives
//! it no speedup (see `docs/GPU_SUPPORT.md`); the heavier models here
//! (`jina-code` ~161M, `bge-large` ~335M, …) are where GPU acceleration —
//! and, for `jina-code`, better code-search quality — actually pay off.
//!
//! Unlike [`super::minilm`], these carry no pinned-SHA integrity check: they
//! are explicit opt-ins via `--embedder <id>` / `.vex.toml embedder`, and
//! fastembed verifies its own downloads against the Hugging Face metadata.

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::device::{execution_providers, Device};
use super::Embedder;

/// Static description of one selectable fastembed model.
pub struct Spec {
    /// Stable vex embedder id (persisted in the manifest; `--embedder <id>`).
    pub id: &'static str,
    /// Backing fastembed model.
    pub model: EmbeddingModel,
    /// Output vector dimension. Must match what the index Header records.
    pub dim: u32,
    /// Context-string char budget. Held at the MiniLM value so contexts stay
    /// comparable across embedders; all models below accept ≥ 512 tokens.
    pub char_budget: usize,
    /// Miss-count threshold below which `Device::Auto` stays on CPU (the GPU
    /// warm-up isn't worth it for a tiny `vex update`). Scales inversely with
    /// model size: a heavier model has a much higher per-symbol CPU cost, so
    /// the GPU break-even is at *fewer* misses. See `docs/GPU_SUPPORT.md` §3.4.
    /// Heuristic default (hardware-dependent); an explicit `--gpu`/`--device`
    /// bypasses the gate entirely.
    pub gpu_auto_min_misses: usize,
}

/// Registry of additional embedders, in display order. `EmbeddingModel`
/// variants are unit enums, so this is a `const` table.
pub const SPECS: &[Spec] = &[
    Spec {
        id: "jina-code",
        model: EmbeddingModel::JinaEmbeddingsV2BaseCode,
        dim: 768,
        char_budget: 1100,
        // ~161M (~7× MiniLM): measured GPU break-even ≈ 33 misses.
        gpu_auto_min_misses: 32,
    },
    Spec {
        id: "bge-base-en-v1.5",
        model: EmbeddingModel::BGEBaseENV15,
        dim: 768,
        char_budget: 1100,
        // ~109M (~5× MiniLM).
        gpu_auto_min_misses: 64,
    },
    Spec {
        id: "bge-large-en-v1.5",
        model: EmbeddingModel::BGELargeENV15,
        dim: 1024,
        char_budget: 1100,
        // ~335M (~15× MiniLM): break-even is very low.
        gpu_auto_min_misses: 16,
    },
    Spec {
        id: "mxbai-large",
        model: EmbeddingModel::MxbaiEmbedLargeV1,
        dim: 1024,
        char_budget: 1100,
        // ~335M (~15× MiniLM).
        gpu_auto_min_misses: 16,
    },
];

/// Look up a non-MiniLM spec by id.
pub fn spec_for(id: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.id == id)
}

/// A vex [`Embedder`] backed by an arbitrary fastembed model.
pub struct FastEmbedModel {
    model: TextEmbedding,
    spec: &'static Spec,
    /// True when a GPU execution provider was registered, so `embed_batch`
    /// uses length-aware micro-batching (bounds VRAM + avoids padding waste).
    gpu: bool,
}

impl FastEmbedModel {
    /// Load `spec`'s model onto `device`. Downloads on first use into the
    /// shared vex embedding cache (heavier models are larger — `bge-large` is
    /// several hundred MB). `Device::Cpu` (and `Auto` on a CPU-only build)
    /// yields an empty execution-provider list — the legacy CPU path.
    pub fn new(spec: &'static Spec, device: Device) -> Result<Self> {
        let cache_dir = crate::util::config::embed_cache_dir();
        std::fs::create_dir_all(&cache_dir).with_context(|| {
            format!(
                "create embedding cache directory at {}",
                cache_dir.display()
            )
        })?;
        let eps = execution_providers(device)?;
        let gpu = !eps.is_empty();
        let model = TextEmbedding::try_new(
            InitOptions::new(spec.model.clone())
                .with_cache_dir(cache_dir)
                .with_show_download_progress(true)
                .with_execution_providers(eps),
        )
        .with_context(|| format!("failed to load embedding model `{}`", spec.id))?;
        Ok(Self { model, spec, gpu })
    }
}

impl Embedder for FastEmbedModel {
    fn id(&self) -> &'static str {
        self.spec.id
    }

    fn dim(&self) -> u32 {
        self.spec.dim
    }

    fn char_budget(&self) -> usize {
        self.spec.char_budget
    }

    fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let results = self.model.embed(vec![text], None)?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding model returned no vector for input"))
    }

    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if self.gpu {
            return crate::embed::batching::embed_length_aware(&mut self.model, texts);
        }
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        self.model.embed(refs, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_invariants() {
        assert!(!SPECS.is_empty());
        for s in SPECS {
            assert!(s.dim > 0, "{} has zero dim", s.id);
            assert!(s.char_budget > 0, "{} has zero budget", s.id);
            // Every additional model is heavier than MiniLM, so its GPU gate
            // must be below MiniLM's (256) and never zero (CUDA warm-up always
            // costs *something* — a 1-symbol update isn't worth it).
            assert!(
                s.gpu_auto_min_misses > 0 && s.gpu_auto_min_misses < 256,
                "{} threshold {} out of (0, 256)",
                s.id,
                s.gpu_auto_min_misses
            );
        }
    }

    #[test]
    fn heavier_model_has_lower_or_equal_gpu_threshold() {
        // jina-code (~161M) breaks even at fewer misses than bge-base (~109M).
        let jina = spec_for("jina-code").unwrap();
        let bge_base = spec_for("bge-base-en-v1.5").unwrap();
        let bge_large = spec_for("bge-large-en-v1.5").unwrap();
        assert!(jina.gpu_auto_min_misses <= bge_base.gpu_auto_min_misses);
        assert!(bge_large.gpu_auto_min_misses <= jina.gpu_auto_min_misses);
        assert_eq!(jina.dim, 768);
    }

    #[test]
    fn unknown_id_has_no_spec() {
        assert!(spec_for("does-not-exist").is_none());
    }
}
