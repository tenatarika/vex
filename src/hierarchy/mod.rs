use std::path::Path;

use anyhow::{Context, Result};
use rayon::prelude::*;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::parse::language::Language;

mod extract;
mod queries;
use queries::{inheritance_query, relation_label};

pub(crate) use extract::capture_hierarchy_edges;

/// A match where a class/struct implements or extends a base type.
#[derive(Debug, Clone)]
pub struct ImplMatch {
    pub path: String,
    pub line: usize,
    pub name: String,
    pub base: String,
    /// Relation kind. Possible values: `"impl"` (Rust), `"extends"`
    /// (Java/C#/Kotlin/Swift/Cpp/Python/TS/PHP class), `"inherits"`
    /// (Ruby `class < Bar`), `"include"` (Ruby mixin), `"uses"` (PHP
    /// trait composition). See `queries::relation_label` for the
    /// per-language mapping.
    pub relation: &'static str,
}

/// Find all types that inherit from / implement `base_name` across all supported languages.
pub fn find_implementations(
    root: &Path,
    base_name: &str,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<ImplMatch>> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files: Vec<_> = crate::util::walk::discover_source_files(&root, excludes)?
        .into_iter()
        .filter(|(_, lang)| inheritance_query(*lang).is_some())
        .collect();

    let matches: Vec<ImplMatch> = files
        .par_iter()
        .flat_map(|(path, lang)| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            find_in_source(&content, *lang, &rel, base_name)
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

/// Find all implementations of `base_name` in a single source string.
fn find_in_source(content: &str, lang: Language, path: &str, base_name: &str) -> Vec<ImplMatch> {
    let query_src = match inheritance_query(lang) {
        Some(q) => q,
        None => return Vec::new(),
    };

    let ts_lang = lang.ts_language();

    let query = match Query::new(&ts_lang, query_src) {
        Ok(q) => q,
        Err(_) => return Vec::new(),
    };

    let mut parser = Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }

    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let base_idx = match query.capture_index_for_name("base") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let child_idx = match query.capture_index_for_name("child") {
        Some(i) => i,
        None => return Vec::new(),
    };

    let mut cursor = QueryCursor::new();
    let mut query_matches = cursor.matches(&query, tree.root_node(), content.as_bytes());
    let mut results = Vec::new();

    while let Some(m) = query_matches.next() {
        let mut base_text = None;
        let mut child_text = None;
        let mut child_line = 0;

        for capture in m.captures {
            let text = &content[capture.node.byte_range()];
            if capture.index == base_idx {
                base_text = Some(text);
            } else if capture.index == child_idx {
                child_text = Some(text);
                child_line = capture.node.start_position().row + 1;
            }
        }

        if let (Some(base), Some(child)) = (base_text, child_text) {
            if base == base_name {
                results.push(ImplMatch {
                    path: path.to_string(),
                    line: child_line,
                    name: child.to_string(),
                    base: base.to_string(),
                    relation: relation_label(lang, m.pattern_index),
                });
            }
        }
    }

    results
}

#[cfg(test)]
mod tests;
