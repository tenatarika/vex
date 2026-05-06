use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::embed;
use crate::index::symbols::ParsedFile;
use crate::parse;
use crate::parse::language::Language;
use crate::store;
use crate::util::config;

const CHUNK_SIZE: usize = 500;
const EMBED_BATCH_SIZE: usize = 256;

/// Index a project directory: discover files, parse in parallel, write to store.
/// If `with_embeddings` is true, generates vector embeddings for each symbol.
/// Returns the number of symbols indexed.
pub fn run(root: &Path, with_embeddings: bool) -> Result<usize> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files = discover_files(&root)?;
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
                let rel = path.strip_prefix(&root).ok()?.to_string_lossy().to_string();

                let done = counter.fetch_add(1, Ordering::Relaxed);
                if done % 500 == 0 {
                    tracing::info!("{done}/{total} files parsed");
                }

                parse::parse_file(&rel, &content, lang).ok()
            })
            .collect();

        all_parsed.extend(parsed);
    }

    let symbol_count: usize = all_parsed.iter().map(|f| f.symbols.len()).sum();

    let vectors = if with_embeddings && symbol_count > 0 {
        generate_embeddings(&all_parsed)?
    } else {
        Vec::new()
    };

    let index_path = config::index_path(&root);
    let cache_dir = index_path.parent()
        .context("index path has no parent directory")?;
    std::fs::create_dir_all(cache_dir)
        .context("create cache directory")?;
    store::writer::write_index_with_vectors(&all_parsed, &vectors, &index_path)
        .context("write index")?;

    tracing::info!(
        symbols = symbol_count,
        vectors = vectors.len(),
        path = ?index_path,
        "indexing complete"
    );
    Ok(symbol_count)
}

fn generate_embeddings(parsed: &[ParsedFile]) -> Result<Vec<Vec<f32>>> {
    let start = Instant::now();
    tracing::info!("loading embedding model");
    let mut embedder = embed::Embedder::new()?;
    tracing::info!(elapsed = ?start.elapsed(), "model loaded");

    let mut contexts = Vec::new();
    for file in parsed {
        for sym in &file.symbols {
            let ctx = embed::build_context(
                sym.kind.as_str(),
                &sym.name,
                &file.path,
                sym.signature.as_deref(),
            );
            contexts.push(ctx);
        }
    }

    let total = contexts.len();
    tracing::info!(total, "embedding symbols");
    let embed_start = Instant::now();

    let mut all_vectors = Vec::with_capacity(total);
    for batch in contexts.chunks(EMBED_BATCH_SIZE) {
        let vectors = embedder.embed_batch(batch)?;
        all_vectors.extend(vectors);
    }

    tracing::info!(total, elapsed = ?embed_start.elapsed(), "embedding complete");
    Ok(all_vectors)
}

fn discover_files(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();

    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .max_depth(Some(50))
        .build()
    {
        let entry = entry?;
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            let path = entry.into_path();
            if std::fs::metadata(&path).is_ok_and(|m| m.len() <= 1_048_576) {
                files.push(path);
            }
        }
    }

    Ok(files)
}
