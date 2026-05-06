use anyhow::{Context, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, QueryCursor};

use super::language::Language;
use super::queries;
use crate::index::symbols::{ParsedRef, ParsedSymbol, SymbolKind};

/// Extract symbols from source code using tree-sitter.
pub fn extract_symbols(content: &str, lang: Language) -> Result<Vec<ParsedSymbol>> {
    let query = match queries::get_query(lang) {
        Some(q) => q,
        None => return Ok(Vec::new()),
    };

    let mut parser = Parser::new();
    let ts_lang = get_ts_language(lang);
    parser.set_language(&ts_lang).context("set language")?;

    let tree = parser
        .parse(content, None)
        .context("tree-sitter parse failed")?;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), content.as_bytes());

    let mut symbols = Vec::new();

    while let Some(m) = matches.next() {
        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            let name = node.utf8_text(content.as_bytes()).unwrap_or_default();
            let line = node.start_position().row + 1;

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
                _ => continue,
            };

            let signature = node.parent().map(|p| {
                let start = p.start_byte();
                let mut end = (start + 200).min(content.len());
                // Walk back to nearest char boundary to avoid panic on multi-byte UTF-8
                while end > start && !content.is_char_boundary(end) {
                    end -= 1;
                }
                let slice = &content[start..end];
                slice.lines().next().unwrap_or("").to_string()
            });

            symbols.push(ParsedSymbol {
                name: name.to_string(),
                kind,
                line,
                signature,
            });
        }
    }

    Ok(symbols)
}

/// Extract references (symbol usages) via simple identifier scanning.
pub fn extract_references(content: &str, _lang: Language) -> Vec<ParsedRef> {
    let mut refs = Vec::new();
    for (line_num, line) in content.lines().enumerate() {
        // Match CamelCase identifiers as type references
        for cap in regex_lite_camel_case(line) {
            refs.push(ParsedRef {
                name: cap.to_string(),
                line: line_num + 1,
                context: Some(line.trim().to_string()),
            });
        }
    }
    refs
}

fn get_ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        Language::Swift => tree_sitter_swift::LANGUAGE.into(),
        Language::Kotlin | Language::TypeScript => unreachable!("no grammar loaded"),
    }
}

/// Simple CamelCase identifier extractor (no regex crate dependency).
fn regex_lite_camel_case(line: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_uppercase() {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            // Must have at least one lowercase letter (not ALL_CAPS constant)
            if word.len() > 1 && word.bytes().any(|b| b.is_ascii_lowercase()) {
                results.push(word);
            }
        } else {
            i += 1;
        }
    }
    results
}
