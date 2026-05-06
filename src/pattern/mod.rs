pub mod matcher;

use std::path::Path;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use crate::parse::language::Language;

/// A match found by the pattern matcher.
#[derive(Debug, Clone)]
pub struct PatternMatch {
    pub path: String,
    pub line: usize,
    pub matched_text: String,
    pub captures: Vec<(String, String)>, // (metavar_name, captured_text)
}

/// Scan files in a directory for code matching a structural pattern.
pub fn scan(root: &Path, pattern: &str, lang: Language, limit: usize) -> Result<Vec<PatternMatch>> {
    let root = root.canonicalize().context("canonicalize root")?;

    // Reject unsupported languages early
    if matches!(lang, Language::Kotlin | Language::TypeScript) {
        bail!("pattern matching not supported for {} yet", lang.as_str());
    }

    let pattern_tree = matcher::parse_pattern(pattern, lang).context("parse pattern")?;

    // Discover files
    let files: Vec<_> = discover_lang_files(&root, lang)?;

    // Scan files in parallel
    let matches: Vec<PatternMatch> = files
        .par_iter()
        .flat_map(|path| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            matcher::find_matches(&content, &pattern_tree, &rel)
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

fn discover_lang_files(root: &Path, lang: Language) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();

    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .max_depth(Some(50))
        .build()
    {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();

        let matches_lang = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension)
            .is_some_and(|l| l == lang);

        if matches_lang {
            files.push(path);
        }
    }

    Ok(files)
}
