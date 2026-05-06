use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// Embedding model wrapper using fastembed (bundled ONNX Runtime + MiniLM).
pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// Initialize the embedding model. Downloads on first use (~86 MB).
    pub fn new() -> Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::AllMiniLML6V2).with_show_download_progress(true),
        )
        .context("failed to load embedding model")?;

        Ok(Self { model })
    }

    /// Generate embedding for a single text.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let results = self.model.embed(vec![text], None)?;
        Ok(results.into_iter().next().unwrap_or_default())
    }

    /// Batch embed multiple texts efficiently.
    pub fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let results = self.model.embed(refs, None)?;
        Ok(results)
    }
}

/// Build a context string for embedding from symbol metadata.
/// Richer context = better semantic search quality.
pub fn build_context(
    kind: &str,
    name: &str,
    file_path: &str,
    signature: Option<&str>,
) -> String {
    let mut ctx = format!("{kind} {name}");

    // Extract module hint from file path
    if let Some(module) = extract_module(file_path) {
        ctx.push_str(&format!(" in {module}"));
    }

    if let Some(sig) = signature {
        ctx.push_str(&format!(", {sig}"));
    }

    ctx
}

fn extract_module(path: &str) -> Option<&str> {
    let without_ext = path.rsplit_once('.').map(|(p, _)| p).unwrap_or(path);
    without_ext.rsplit_once('/').map(|(dir, _)| dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_with_all_fields() {
        let ctx = build_context("struct", "PaymentService", "src/billing/service.rs", Some("pub struct PaymentService"));
        assert_eq!(ctx, "struct PaymentService in src/billing, pub struct PaymentService");
    }

    #[test]
    fn context_without_signature() {
        let ctx = build_context("function", "main", "src/main.rs", None);
        assert_eq!(ctx, "function main in src");
    }

    #[test]
    fn context_root_file() {
        let ctx = build_context("function", "main", "main.rs", None);
        assert_eq!(ctx, "function main");
    }
}
