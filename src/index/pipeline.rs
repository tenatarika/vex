use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::embed;
use crate::index::hasher;
use crate::index::manifest::{self, Manifest};
use crate::index::symbols::ParsedFile;
use crate::parse;
use crate::parse::language::Language;
use crate::store;
use crate::util::config;

const CHUNK_SIZE: usize = 500;
const EMBED_BATCH_SIZE: usize = 256;

/// Full rebuild: index all files from scratch.
pub fn run(root: &Path, with_embeddings: bool) -> Result<usize> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files = discover_files(&root)?;
    tracing::info!(count = files.len(), "discovered files");

    // Hash and parse in one pass to avoid reading files twice
    let file_hashes = hash_files(&root, &files);
    let all_parsed = parse_files(&root, &files)?;
    let symbol_count: usize = all_parsed.iter().map(|f| f.symbols.len()).sum();

    let vectors = if with_embeddings && symbol_count > 0 {
        generate_embeddings(&all_parsed)?
    } else {
        Vec::new()
    };

    write_output(&root, &all_parsed, &vectors, &file_hashes)?;

    tracing::info!(
        symbols = symbol_count,
        vectors = vectors.len(),
        "indexing complete"
    );
    Ok(symbol_count)
}

/// Incremental update: detect changed files via content hashes, then rebuild index.
/// Currently detects changes but does a full rebuild (partial writes planned for future).
/// Returns (total_symbols, changed_count, deleted_count).
pub fn update(root: &Path, with_embeddings: bool) -> Result<(usize, usize, usize)> {
    let root = root.canonicalize().context("canonicalize root")?;
    let manifest_path = config::manifest_path(&root);
    let old_manifest = Manifest::load(&manifest_path)?;

    let files = discover_files(&root)?;
    let file_hashes = hash_files(&root, &files);

    let diff = manifest::diff_files(&file_hashes, &old_manifest);

    if diff.changed.is_empty() && diff.deleted.is_empty() {
        tracing::info!(unchanged = diff.unchanged, "nothing to update");
        let index_path = config::index_path(&root);
        let symbol_count = if index_path.exists() {
            let reader = crate::store::reader::IndexReader::open(&index_path)
                .context("open existing index for symbol count")?;
            reader.symbol_count()
        } else {
            0
        };
        return Ok((symbol_count, 0, 0));
    }

    tracing::info!(
        changed = diff.changed.len(),
        deleted = diff.deleted.len(),
        unchanged = diff.unchanged,
        "incremental update"
    );

    let all_parsed = parse_files(&root, &files)?;
    let symbol_count: usize = all_parsed.iter().map(|f| f.symbols.len()).sum();

    let vectors = if with_embeddings && symbol_count > 0 {
        generate_embeddings(&all_parsed)?
    } else {
        Vec::new()
    };

    write_output(&root, &all_parsed, &vectors, &file_hashes)?;

    Ok((symbol_count, diff.changed.len(), diff.deleted.len()))
}

fn hash_files(root: &Path, files: &[std::path::PathBuf]) -> Vec<(String, u64)> {
    files
        .par_iter()
        .filter_map(|path| {
            let content = std::fs::read(path).ok()?;
            let rel = path.strip_prefix(root).ok()?.to_string_lossy().to_string();
            let hash = hasher::content_hash(&content);
            Some((rel, hash))
        })
        .collect()
}

fn parse_files(root: &Path, files: &[std::path::PathBuf]) -> Result<Vec<ParsedFile>> {
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
                if done % 500 == 0 && done > 0 {
                    tracing::info!("{done}/{total} files parsed");
                }

                parse::parse_file(&rel, &content, lang).ok()
            })
            .collect();

        all_parsed.extend(parsed);
    }

    Ok(all_parsed)
}

fn write_output(
    root: &Path,
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    file_hashes: &[(String, u64)],
) -> Result<()> {
    let index_path = config::index_path(root);
    let cache_dir = index_path.parent().context("index path has no parent")?;
    std::fs::create_dir_all(cache_dir).context("create cache directory")?;

    store::writer::write_index_with_vectors(parsed, vectors, &index_path).context("write index")?;

    // Save manifest with pre-computed hashes (no extra file reads)
    let manifest_path = config::manifest_path(root);
    let manifest = Manifest {
        files: file_hashes.iter().cloned().collect::<HashMap<_, _>>(),
    };
    manifest.save(&manifest_path)?;

    Ok(())
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
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();

        // Filter by supported extension BEFORE reading — avoids I/O on irrelevant files
        if path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension)
            .is_none()
        {
            continue;
        }

        // Skip files > 1 MB (likely generated/minified)
        if std::fs::metadata(&path).is_ok_and(|m| m.len() <= 1_048_576) {
            files.push(path);
        }
    }

    Ok(files)
}
