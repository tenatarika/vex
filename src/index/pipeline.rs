use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use rayon::prelude::*;

use crate::index::symbols::ParsedFile;
use crate::parse;
use crate::parse::language::Language;

const CHUNK_SIZE: usize = 500;

/// Index a project directory: discover files, parse in parallel, write to store.
pub fn run(root: &Path) -> Result<Vec<ParsedFile>> {
    let files = discover_files(root)?;
    tracing::info!(count = files.len(), "discovered files");

    let counter = AtomicUsize::new(0);
    let total = files.len();

    let mut all_parsed = Vec::new();

    for chunk in files.chunks(CHUNK_SIZE) {
        let parsed: Vec<ParsedFile> = chunk
            .par_iter()
            .filter_map(|path| {
                let ext = path.extension()?.to_str()?;
                let lang = Language::from_extension(ext)?;
                let content = std::fs::read_to_string(path).ok()?;
                let rel = path.strip_prefix(root).ok()?.to_string_lossy().to_string();

                let done = counter.fetch_add(1, Ordering::Relaxed);
                if done % 500 == 0 {
                    tracing::info!("{done}/{total} files parsed");
                }

                parse::parse_file(&rel, &content, lang).ok()
            })
            .collect();

        // TODO: write to store here
        all_parsed.extend(parsed);
    }

    tracing::info!(symbols = all_parsed.iter().map(|f| f.symbols.len()).sum::<usize>(), "indexing complete");
    Ok(all_parsed)
}

fn discover_files(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();

    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .max_depth(Some(50))
        .build()
    {
        let entry = entry?;
        if entry.file_type().map_or(false, |ft| ft.is_file()) {
            let path = entry.into_path();
            // Skip files > 1 MB
            if std::fs::metadata(&path).map_or(false, |m| m.len() <= 1_048_576) {
                files.push(path);
            }
        }
    }

    Ok(files)
}
