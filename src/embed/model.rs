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
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding model returned no vector for input"))
    }

    /// Batch embed multiple texts efficiently.
    pub fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let results = self.model.embed(refs, None)?;
        Ok(results)
    }
}

/// Build a context string for embedding from symbol metadata.
/// Includes tokenized name, path-derived domain keywords, signature, and docstring.
pub fn build_context(
    kind: &str,
    name: &str,
    file_path: &str,
    signature: Option<&str>,
    doc: Option<&str>,
) -> String {
    let mut ctx = format!("{kind} {name}");

    // Add tokenized name for better semantic matching
    // e.g. "MissingDataRuleEvaluator" → "missing data rule evaluator"
    let tokens = super::tokenizer::tokenize(name);
    if tokens.len() > 1 {
        ctx.push_str(&format!(" ({})", tokens.join(" ")));
    }

    // Add path-derived domain keywords
    // e.g. "src/stream_processors/temperature.rs" → "stream processors temperature"
    let path_words = extract_path_keywords(file_path);
    if !path_words.is_empty() {
        ctx.push_str(&format!(" in {path_words}"));
    }

    if let Some(sig) = signature {
        ctx.push_str(&format!(", {sig}"));
    }

    if let Some(d) = doc {
        if !d.is_empty() {
            ctx.push_str(&format!(". {d}"));
        }
    }

    ctx
}

/// Extract meaningful keywords from a file path.
/// "src/stream_processors/temperature.rs" → "stream processors temperature"
fn extract_path_keywords(path: &str) -> String {
    let without_ext = path.rsplit_once('.').map(|(p, _)| p).unwrap_or(path);

    without_ext
        .split('/')
        .filter(|seg| {
            !matches!(
                *seg,
                "src" | "lib" | "main" | "mod" | "index" | "test" | "tests"
            )
        })
        .flat_map(|seg| seg.split('_'))
        .filter(|w| w.len() > 1)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_with_all_fields() {
        let ctx = build_context(
            "struct",
            "PaymentService",
            "src/billing/service.rs",
            Some("pub struct PaymentService"),
            None,
        );
        assert_eq!(
            ctx,
            "struct PaymentService (payment service) in billing service, pub struct PaymentService"
        );
    }

    #[test]
    fn context_without_signature() {
        let ctx = build_context("function", "main", "src/main.rs", None, None);
        // "main" is a single token, so no tokenized expansion
        assert_eq!(ctx, "function main");
    }

    #[test]
    fn context_root_file() {
        let ctx = build_context("function", "main", "main.rs", None, None);
        assert_eq!(ctx, "function main");
    }

    #[test]
    fn context_with_docstring() {
        let ctx = build_context(
            "function",
            "process_batch",
            "src/stream_processors/temperature.rs",
            Some("fn process_batch(&self, data: &[f32])"),
            Some("Process a batch of temperature readings from IoT sensors"),
        );
        assert!(ctx.contains("process batch"));
        assert!(ctx.contains("stream processors temperature"));
        assert!(ctx.contains("Process a batch of temperature readings"));
    }

    #[test]
    fn path_keywords_filters_noise() {
        let kw = extract_path_keywords("src/lib/search/mod.rs");
        // "src", "lib", "mod" filtered out; "search" kept
        assert_eq!(kw, "search");
    }

    #[test]
    fn path_keywords_splits_underscores() {
        let kw = extract_path_keywords("src/stream_processors/alert_rules.rs");
        assert_eq!(kw, "stream processors alert rules");
    }
}
