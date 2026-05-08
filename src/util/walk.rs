use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

use crate::parse::language::Language;

/// Build a configured file walker with exclude patterns applied.
pub fn walk_builder(root: &Path, excludes: &[String]) -> Result<WalkBuilder> {
    let mut builder = WalkBuilder::new(root);
    builder.hidden(true).max_depth(Some(50));

    if !excludes.is_empty() {
        let mut ov = OverrideBuilder::new(root);
        for pattern in excludes {
            anyhow::ensure!(
                !pattern.starts_with('!'),
                "exclude pattern {pattern:?} must not start with '!' — use the pattern without prefix"
            );
            ov.add(&format!("!{pattern}"))
                .with_context(|| format!("invalid exclude pattern: {pattern}"))?;
        }
        builder.overrides(ov.build()?);
    }

    Ok(builder)
}

/// Discover all source files with their detected language.
/// Used by callgraph, hierarchy, and other live-scan commands.
pub fn discover_source_files(root: &Path, excludes: &[String]) -> Result<Vec<(PathBuf, Language)>> {
    let mut files = Vec::new();

    for entry in walk_builder(root, excludes)?.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if let Some(lang) = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension)
        {
            files.push((path, lang));
        }
    }

    Ok(files)
}
