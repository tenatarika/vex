use anyhow::Result;

use super::tokenizer;

/// Embedding model wrapper for ONNX inference.
///
/// Phase 2: will load MiniLM-L6-v2 via ort crate.
/// For now provides a placeholder that returns zero vectors.
pub struct Embedder {
    dim: usize,
}

impl Embedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    /// Load a real ONNX model from disk.
    pub fn load(_model_path: &str) -> Result<Self> {
        // TODO: load via ort::Session
        Ok(Self { dim: 384 })
    }

    /// Generate embedding for a symbol context string.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        let _tokens = tokenizer::tokenize(text);
        // TODO: run ONNX inference
        vec![0.0; self.dim]
    }

    /// Batch embed multiple texts.
    pub fn embed_batch(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }
}

/// Build a context string for embedding from symbol metadata.
pub fn build_context(
    kind: &str,
    name: &str,
    module: Option<&str>,
    signature: Option<&str>,
) -> String {
    let mut ctx = format!("{kind} {name}");
    if let Some(m) = module {
        ctx.push_str(&format!(" in {m}"));
    }
    if let Some(sig) = signature {
        ctx.push_str(&format!(", signature: {sig}"));
    }
    ctx
}
