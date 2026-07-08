//! Symbol + import extraction — the main public seam consumed by
//! `parse::mod`. One tree-sitter parse drives both: the SCM query
//! (from `parse::queries`) captures named symbols and import sites,
//! and per-symbol `body_tokens` are populated via `super::body`'s
//! `extract_body_tokens` walker.
//!
//! Isolated from the AST classifiers in `refs.rs` and the body-token
//! walker in `body.rs` so the high-level orchestration stays readable.

use anyhow::Result;
use streaming_iterator::StreamingIterator;
use tree_sitter::QueryCursor;

use crate::index::symbols::{ParsedRef, ParsedSymbol, SymbolKind};
use crate::parse::language::Language;
use crate::parse::parser_pool::parse_text;
use crate::parse::queries;
use crate::parse::NodeTextExt;

use super::body::extract_body_tokens;
use super::GrammarLoadError;

/// Extract symbols and AST-based import references in a single tree-sitter parse.
///
/// Returns `(symbols, imports)`. Each [`ParsedSymbol`] carries the captured
/// `name`, `kind`, `line`, `signature`, `doc`, and `body_tokens`; each
/// [`ParsedRef`] is an import-site reference with the `name` and a `context`
/// snippet of the source line. Errors as [`GrammarLoadError`] when the
/// language grammar fails to load (ABI mismatch, renamed AST node).
///
/// ```
/// use vex::parse::extractor::extract_symbols_and_imports;
/// use vex::parse::language::Language;
///
/// let src = "use std::collections::HashMap;\nfn main() {}";
/// let (symbols, imports) = extract_symbols_and_imports(src, Language::Rust).unwrap();
/// assert!(symbols.iter().any(|s| s.name == "main"));
/// assert!(imports.iter().any(|r| r.name == "HashMap"));
/// ```
pub fn extract_symbols_and_imports(
    content: &str,
    lang: Language,
) -> Result<(Vec<ParsedSymbol>, Vec<ParsedRef>)> {
    let query = match queries::try_get_query(lang) {
        Ok(q) => q,
        Err(e) => {
            return Err(GrammarLoadError {
                lang,
                reason: e.to_string(),
            }
            .into());
        }
    };

    // v1.12.0 P3 — borrow a pooled per-thread parser instead of constructing
    // one per file. The Tree owns its data after parse(), so we can drop the
    // parser borrow before iterating with QueryCursor.
    let tree = parse_text(lang, content)?;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), content.as_bytes());

    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    // v1.12.0 P4 — precompute line slices once so the per-capture context
    // lookup is O(1) instead of `content.lines().nth(n)`'s O(line_count).
    // The match loop fires per captured symbol/import; on a 5k-LOC file
    // with a few hundred captures the old pattern was O(N × K) — small per
    // file but visible at workspace scale.
    let line_slices: Vec<&str> = content.lines().collect();

    while let Some(m) = matches.next() {
        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            let name = node.node_text(content.as_bytes());
            if name.is_empty() {
                continue;
            }
            let line = node.start_position().row + 1;

            if *capture_name == "import.name" {
                let context = line_slices.get(line - 1).map(|l| l.trim().to_string());
                imports.push(ParsedRef {
                    name: strip_import_quotes(name).to_string(),
                    line,
                    context,
                });
                continue;
            }

            let kind = match *capture_name {
                "fn.name" => SymbolKind::Function,
                "struct.name" => SymbolKind::Struct,
                "enum.name" => SymbolKind::Enum,
                "trait.name" => SymbolKind::Trait,
                "impl.type" => SymbolKind::Impl,
                "impl.method" => SymbolKind::Method,
                "class.name" => SymbolKind::Class,
                "interface.name" => SymbolKind::Interface,
                "type.name" => SymbolKind::TypeAlias,
                "property.name" => SymbolKind::Property,
                "const.name" => SymbolKind::Constant,
                "heading.name" => SymbolKind::Heading,
                _ => continue,
            };

            let parent = node.parent();
            let signature = parent.map(|p| {
                let start = p.start_byte();
                let mut end = (start + 200).min(content.len());
                while end > start && !content.is_char_boundary(end) {
                    end -= 1;
                }
                let slice = &content[start..end];
                slice.lines().next().unwrap_or("").to_string()
            });

            // Headings have no doc comments; extract_doc_above would misidentify parent headings
            let doc = if kind == SymbolKind::Heading {
                None
            } else {
                extract_doc_above(&line_slices, line)
            };
            let body_tokens = parent.and_then(|def| extract_body_tokens(def, content, lang));

            symbols.push(ParsedSymbol {
                name: name.to_string(),
                kind,
                line,
                signature,
                doc,
                body_tokens,
            });
        }
    }

    Ok((symbols, imports))
}

/// Strip a single layer of matching surrounding quote-pair delimiters from
/// an import name captured by a tree-sitter query.
///
/// Some grammars expose string literals only as a single `(string)` node
/// (Lua tree-sitter-lua 0.5 when there are no escape sequences) or as a
/// `(string_literal)` containing the quotes verbatim (C/C++ `#include`).
/// Stripping the wrapping delimiters here means `vex usages util` matches
/// a Lua `require("util")` rather than failing because the stored name
/// is `"util"` (quotes included).
///
/// Handled pairs: `"..."`, `'...'`, `<...>` (C/C++ system includes), and
/// `[[...]]` (Lua long-bracket strings). Mismatched or empty inputs are
/// returned unchanged.
fn strip_import_quotes(name: &str) -> &str {
    let bytes = name.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if matches!((first, last), (b'"', b'"') | (b'\'', b'\'') | (b'<', b'>')) {
            return &name[1..name.len() - 1];
        }
    }
    if bytes.len() >= 4 && name.starts_with("[[") && name.ends_with("]]") {
        return &name[2..name.len() - 2];
    }
    name
}

/// Extract doc comment or docstring from lines immediately above a symbol.
/// Returns up to ~200 chars of cleaned comment text, or None.
///
/// Takes a pre-collected `&[&str]` line slice instead of `&str`+`.lines()`
/// so the caller's `line_slices` (v1.12.0 P4 — built once per file) is
/// reused; the older `let lines: Vec<&str> = content.lines().collect()`
/// inside this fn was an O(line_count) allocation per symbol.
fn extract_doc_above(lines: &[&str], symbol_line: usize) -> Option<String> {
    if symbol_line <= 1 {
        return None;
    }
    let mut doc_lines: Vec<&str> = Vec::new();
    let mut idx = symbol_line - 2; // 0-indexed line above symbol

    loop {
        if idx >= lines.len() {
            break;
        }
        let trimmed = lines[idx].trim();

        if trimmed.starts_with("///")
            || trimmed.starts_with("//!")
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/**")
            || trimmed.starts_with('*')
            || trimmed.starts_with("\"\"\"")
            || trimmed.starts_with("'''")
        {
            let cleaned = trimmed
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim_start_matches("//")
                .trim_start_matches("/**")
                .trim_start_matches("*/")
                .trim_start_matches('*')
                .trim_start_matches('#')
                .trim();
            if !cleaned.is_empty() {
                doc_lines.push(cleaned);
            }
        } else if trimmed.is_empty() {
            // skip blank lines between comment and symbol
        } else {
            break;
        }

        if idx == 0 {
            break;
        }
        idx -= 1;
    }

    if doc_lines.is_empty() {
        return None;
    }

    doc_lines.reverse();
    let mut doc = doc_lines.join(" ");
    if doc.len() > 200 {
        let mut cut = 200;
        while cut > 0 && !doc.is_char_boundary(cut) {
            cut -= 1;
        }
        doc.truncate(cut);
    }
    Some(doc)
}
