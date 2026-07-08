//! Body-token extraction — collects identifier + string-literal text
//! from a symbol's AST definition node and returns the deduped
//! lower-cased word stream that feeds the symbol's `body_tokens`
//! field on the FST.
//!
//! Isolated from `mod.rs` so the per-language node-kind dispatch
//! (Python `string`/`f_string`, JS template strings, TOML/YAML scalar
//! values) doesn't share screen space with the higher-level
//! symbol/import extractor in `symbols.rs`.

use std::collections::HashSet;

use crate::parse::language::Language;
use crate::parse::NodeTextExt;

use super::is_keyword;

/// Extract meaningful identifiers from a symbol's AST definition node.
/// Walks the subtree, collects identifier and string literal text,
/// filters keywords and short names. Returns space-separated tokens.
pub(super) fn extract_body_tokens(
    def_node: tree_sitter::Node,
    content: &str,
    lang: Language,
) -> Option<String> {
    let mut seen = HashSet::new();
    let mut tokens = Vec::new(); // preserves first-occurrence order
    let mut stack = vec![def_node];
    let mut nodes_visited = 0usize;
    const MAX_NODES: usize = 2000;
    // Phase 8.4 + v1.11 hotfix: `tree-sitter-toml-ng` emits a bare
    // `"string"` leaf for string values. Other languages also use
    // `"string"` as a node kind (Rust string literals, Python strings
    // via the `string` parent rule, …) but those contain
    // `string_content` / `string_fragment` children that already route
    // through the string-tokenising arm below. Hoisted out of the loop
    // because `lang` is constant for the whole call.
    let is_toml = matches!(lang, Language::Toml);

    while let Some(node) = stack.pop() {
        nodes_visited += 1;
        if nodes_visited > MAX_NODES {
            break;
        }

        let kind = node.kind();
        let is_toml_string = is_toml && kind == "string";

        match kind {
            "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "attribute"
            | "shorthand_field_identifier"
            // Phase 8.4 — config-language identifier-style leaves so
            // semantic search can match against TOML keys, HTML tags /
            // attributes, CSS selectors / properties / keyframe names,
            // and YAML mapping keys. Pre-8.4 these produced
            // `body_tokens = None`, leaving the semantic channel
            // blind to config-file content.
            | "bare_key"
            | "dotted_key"
            | "quoted_key"
            | "attribute_name"
            | "tag_name"
            | "property_name"
            | "class_name"
            | "id_name"
            | "keyframes_name"
            | "string_scalar"
            | "plain_scalar" => {
                // content is guaranteed UTF-8 by read_to_string
                let text = node.node_text(content.as_bytes());
                if text.len() > 1 && !is_keyword(text) && seen.insert(text.to_string()) {
                    tokens.push(text.to_string());
                }
            }
            // Phase 8.4 — config-language string/value leaves. Split
            // by whitespace + filter to alphanumeric+`_` words so
            // free-text TOML/YAML/HTML/CSS values become searchable
            // ("endpoint = \"https://prod.example.com\"" tokenises to
            // `endpoint`, `https`, `prod`, `example`, `com`).
            "string_content"
            | "string_fragment"
            | "attribute_value"
            | "quoted_attribute_value"
            | "single_quote_scalar"
            | "double_quote_scalar"
            | "plain_value"
            | "string_value" => {
                tokenise_string_value(node, content, &mut seen, &mut tokens);
            }
            _ if is_toml_string => {
                tokenise_string_value(node, content, &mut seen, &mut tokens);
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    if tokens.is_empty() {
        return None;
    }

    let mut joined = tokens.join(" ");
    if joined.len() > 400 {
        // Find a char boundary at or before 400 bytes
        let mut cut = 400;
        while cut > 0 && !joined.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(pos) = joined[..cut].rfind(' ') {
            joined.truncate(pos);
        } else {
            joined.truncate(cut);
        }
    }
    Some(joined)
}

/// Split a string-value node into lower-cased alphanumeric+`_` words,
/// deduped via `seen`. Shared between the generic string-value match
/// arm and the TOML-only `"string"` fallthrough so the two paths emit
/// identical tokens.
fn tokenise_string_value(
    node: tree_sitter::Node,
    content: &str,
    seen: &mut HashSet<String>,
    tokens: &mut Vec<String>,
) {
    let text = node.node_text(content.as_bytes());
    for word in text.split_whitespace() {
        let clean: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if clean.len() > 2 && seen.insert(clean.to_lowercase()) {
            tokens.push(clean.to_lowercase());
        }
    }
}
