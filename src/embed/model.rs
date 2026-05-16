//! Context-string construction for embeddings.
//!
//! Symbol-level context strings combine name, tokenized name, path keywords,
//! signature, docstring, and body tokens into a single line fed to the
//! embedder. The character budget is supplied by the caller (the embedder's
//! `char_budget()`) so the context fits each model's token window.

/// Build a context string for embedding from symbol metadata.
///
/// `budget` — maximum length in chars before body tokens are truncated.
/// Typically the embedder's `char_budget()` (1100 for MiniLM-L6-v2).
pub fn build_context(
    kind: &str,
    name: &str,
    file_path: &str,
    signature: Option<&str>,
    doc: Option<&str>,
    body_tokens: Option<&str>,
    budget: usize,
) -> String {
    let mut ctx = format!("{kind} {name}");

    let tokens = super::tokenizer::tokenize(name);
    if tokens.len() > 1 {
        ctx.push_str(&format!(" ({})", tokens.join(" ")));
    }

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

    if let Some(bt) = body_tokens {
        if !bt.is_empty() {
            let remaining = budget.saturating_sub(ctx.len());
            if remaining > 20 {
                let trimmed = if bt.len() > remaining {
                    // Find a char boundary at or before the budget limit
                    let mut cut = remaining;
                    while cut > 0 && !bt.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    bt[..cut].rfind(' ').map_or(&bt[..cut], |p| &bt[..p])
                } else {
                    bt
                };
                let expanded: Vec<String> = trimmed
                    .split_whitespace()
                    .flat_map(super::tokenizer::tokenize)
                    .collect();
                if !expanded.is_empty() {
                    ctx.push_str(&format!(". body: {}", expanded.join(" ")));
                }
            }
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
    use crate::embed::minilm::MINILM_CHAR_BUDGET;

    #[test]
    fn context_with_all_fields() {
        let ctx = build_context(
            "struct",
            "PaymentService",
            "src/billing/service.rs",
            Some("pub struct PaymentService"),
            None,
            None,
            MINILM_CHAR_BUDGET,
        );
        assert_eq!(
            ctx,
            "struct PaymentService (payment service) in billing service, pub struct PaymentService"
        );
    }

    #[test]
    fn context_without_signature() {
        let ctx = build_context(
            "function",
            "main",
            "src/main.rs",
            None,
            None,
            None,
            MINILM_CHAR_BUDGET,
        );
        assert_eq!(ctx, "function main");
    }

    #[test]
    fn context_root_file() {
        let ctx = build_context(
            "function",
            "main",
            "main.rs",
            None,
            None,
            None,
            MINILM_CHAR_BUDGET,
        );
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
            None,
            MINILM_CHAR_BUDGET,
        );
        assert!(ctx.contains("process batch"));
        assert!(ctx.contains("stream processors temperature"));
        assert!(ctx.contains("Process a batch of temperature readings"));
    }

    #[test]
    fn context_with_body_tokens() {
        let ctx = build_context(
            "class",
            "MissingDataRuleEvaluator",
            "src/rules/staleness.rs",
            Some("pub struct MissingDataRuleEvaluator"),
            None,
            Some("offline_threshold sensor_status staleness_guard check_interval"),
            MINILM_CHAR_BUDGET,
        );
        assert!(ctx.contains("missing data rule evaluator"));
        assert!(ctx.contains("body:"));
        assert!(ctx.contains("offline"));
        assert!(ctx.contains("sensor"));
        assert!(ctx.contains("staleness"));
    }

    #[test]
    fn body_tokens_respect_budget() {
        // Tiny budget should suppress body tokens entirely.
        let ctx = build_context(
            "function",
            "foo",
            "src/lib.rs",
            None,
            None,
            Some("alpha beta gamma delta epsilon zeta eta theta"),
            10,
        );
        assert!(!ctx.contains("body:"));
    }

    #[test]
    fn path_keywords_filters_noise() {
        let kw = extract_path_keywords("src/lib/search/mod.rs");
        assert_eq!(kw, "search");
    }

    #[test]
    fn path_keywords_splits_underscores() {
        let kw = extract_path_keywords("src/stream_processors/alert_rules.rs");
        assert_eq!(kw, "stream processors alert rules");
    }
}
