//! MiniLM-L6-v2 embedder via fastembed (bundled ONNX Runtime).
//!
//! Default and currently only embedder. Output is 384-dim float32.

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::Embedder;

/// Stable identifier for this embedder. Persisted in the manifest and used
/// for mismatch detection at search time.
pub const MINILM_ID: &str = "minilm-l6-v2";

/// Embedding dimension (384) for all-MiniLM-L6-v2.
pub const MINILM_DIM: u32 = 384;

/// Character budget for the context string fed to MiniLM-L6-v2.
/// MiniLM accepts ~256 wordpiece tokens (~4.5 chars/token → ~1100 chars).
pub const MINILM_CHAR_BUDGET: usize = 1100;

pub struct MiniLMEmbedder {
    model: TextEmbedding,
}

impl MiniLMEmbedder {
    /// Initialize the model. Downloads ~86 MB on first use into the
    /// global vex embedding cache (shared across projects). Without an
    /// explicit cache dir fastembed would drop `.fastembed_cache/` into
    /// the current working directory — that re-downloads the same model
    /// for every project and pollutes the project tree.
    pub fn new() -> Result<Self> {
        let cache_dir = crate::util::config::embed_cache_dir();
        std::fs::create_dir_all(&cache_dir).ok();
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::AllMiniLML6V2)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(true),
        )
        .context("failed to load MiniLM-L6-v2 embedding model")?;
        Ok(Self { model })
    }
}

impl Embedder for MiniLMEmbedder {
    fn id(&self) -> &'static str {
        MINILM_ID
    }

    fn dim(&self) -> u32 {
        MINILM_DIM
    }

    fn char_budget(&self) -> usize {
        MINILM_CHAR_BUDGET
    }

    fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let results = self.model.embed(vec![text], None)?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding model returned no vector for input"))
    }

    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let results = self.model.embed(refs, None)?;
        Ok(results)
    }
}
