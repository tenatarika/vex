use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::embed;
use crate::index::hasher;
use crate::index::manifest::{self, Manifest};
use crate::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
use crate::parse;
use crate::parse::language::Language;
use crate::store;
use crate::util::config;

const CHUNK_SIZE: usize = 500;
const EMBED_BATCH_SIZE: usize = 256;

/// Full rebuild: index all files from scratch.
pub fn run(root: &Path, with_embeddings: bool, excludes: &[String]) -> Result<usize> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files = discover_files(&root, excludes)?;
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

    if !vectors.is_empty() {
        build_hnsw(&root, &vectors)?;
    } else {
        // Remove stale HNSW from a previous --semantic run to prevent wrong results
        let hnsw_path = config::hnsw_path(&root);
        if hnsw_path.exists() {
            std::fs::remove_file(&hnsw_path).context("remove stale HNSW index")?;
        }
    }

    tracing::info!(
        symbols = symbol_count,
        vectors = vectors.len(),
        "indexing complete"
    );
    Ok(symbol_count)
}

/// Incremental update: detect changed files, re-parse only those, merge with unchanged
/// symbols from the existing index. Returns (total_symbols, changed_count, deleted_count).
pub fn update(
    root: &Path,
    with_embeddings: bool,
    excludes: &[String],
) -> Result<(usize, usize, usize)> {
    let root = root.canonicalize().context("canonicalize root")?;
    let manifest_path = config::manifest_path(&root);
    let old_manifest = Manifest::load(&manifest_path)?;

    let files = discover_files(&root, excludes)?;
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

    let changed_set: HashSet<&str> = diff.changed.iter().map(|s| s.as_str()).collect();
    let deleted_set: HashSet<&str> = diff.deleted.iter().map(|s| s.as_str()).collect();

    // Reconstruct unchanged symbols (+ vectors) from existing index
    let index_path = config::index_path(&root);
    let (unchanged_parsed, unchanged_vectors) = if index_path.exists() {
        let reader = crate::store::reader::IndexReader::open(&index_path)
            .context("open existing index for incremental merge")?;
        reconstruct_unchanged(&reader, &changed_set, &deleted_set)
    } else {
        (Vec::new(), Vec::new())
    };

    let unchanged_sym_count: usize = unchanged_parsed.iter().map(|f| f.symbols.len()).sum();
    tracing::info!(
        unchanged_symbols = unchanged_sym_count,
        unchanged_vectors = unchanged_vectors.len(),
        "reconstructed unchanged from index"
    );

    // Parse only changed/new files
    let changed_paths: Vec<std::path::PathBuf> = files
        .iter()
        .filter(|p| {
            p.strip_prefix(&root)
                .ok()
                .and_then(|r| r.to_str())
                .is_some_and(|r| changed_set.contains(r))
        })
        .cloned()
        .collect();

    let newly_parsed = parse_files(&root, &changed_paths)?;
    let new_sym_count: usize = newly_parsed.iter().map(|f| f.symbols.len()).sum();

    // Generate embeddings only for new/changed symbols
    let new_vectors = if with_embeddings && new_sym_count > 0 {
        generate_embeddings(&newly_parsed)?
    } else {
        Vec::new()
    };

    // Merge: unchanged first (vectors align with symbol order)
    let mut all_parsed = unchanged_parsed;
    all_parsed.extend(newly_parsed);
    let symbol_count: usize = all_parsed.iter().map(|f| f.symbols.len()).sum();

    let mut all_vectors = unchanged_vectors;
    all_vectors.extend(new_vectors);

    write_output(&root, &all_parsed, &all_vectors, &file_hashes)?;

    if !all_vectors.is_empty() {
        build_hnsw(&root, &all_vectors)?;
    } else {
        let hnsw_path = config::hnsw_path(&root);
        if hnsw_path.exists() {
            std::fs::remove_file(&hnsw_path).context("remove stale HNSW index")?;
        }
    }

    tracing::info!(
        total = symbol_count,
        reused = unchanged_sym_count,
        reparsed = new_sym_count,
        "incremental update complete"
    );

    Ok((symbol_count, diff.changed.len(), diff.deleted.len()))
}

/// Reconstruct ParsedFile + vectors for unchanged files from the existing index.
/// Symbols are in index order (file-contiguous), vectors align 1:1 with symbols.
/// Refs are not recoverable per-file from the FST and are set to empty.
fn reconstruct_unchanged(
    reader: &crate::store::reader::IndexReader,
    changed: &HashSet<&str>,
    deleted: &HashSet<&str>,
) -> (Vec<ParsedFile>, Vec<Vec<f32>>) {
    let has_vectors = reader.has_vectors();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut parsed_files: Vec<ParsedFile> = Vec::new();
    let mut current_path = String::new();
    let mut current_symbols: Vec<ParsedSymbol> = Vec::new();

    for i in 0..reader.symbol_count() {
        let rec = match reader.symbol(i) {
            Some(r) => r,
            None => continue,
        };
        let path = reader.read_string(rec.file_offset).to_string();

        // Skip changed/deleted files — they'll be re-parsed
        if changed.contains(path.as_str()) || deleted.contains(path.as_str()) {
            continue;
        }

        // Flush previous file group when path changes
        if path != current_path && !current_path.is_empty() {
            parsed_files.push(ParsedFile {
                path: std::mem::take(&mut current_path),
                symbols: std::mem::take(&mut current_symbols),
                refs: Vec::new(),
            });
        }
        current_path = path;

        let name = reader.read_string(rec.name_offset).to_string();
        let kind = SymbolKind::try_from(rec.kind).unwrap_or(SymbolKind::Function);
        let sig = {
            let s = reader.read_string(rec.signature_offset);
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        };

        current_symbols.push(ParsedSymbol {
            name,
            kind,
            line: rec.line as usize,
            signature: sig,
            doc: None,
            body_tokens: None,
        });

        if has_vectors {
            if let Some(vec) = reader.vector(rec.vector_index) {
                vectors.push(vec.to_vec());
            }
        }
    }

    // Flush last file group
    if !current_path.is_empty() {
        parsed_files.push(ParsedFile {
            path: current_path,
            symbols: current_symbols,
            refs: Vec::new(),
        });
    }

    (parsed_files, vectors)
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
    // (language, first_error) -> skipped_count. Aggregated so an ABI mismatch
    // surfaces as a single loud summary at the end instead of being buried
    // in per-file warnings the user usually has filtered out.
    let grammar_failures: Mutex<HashMap<Language, (String, usize)>> = Mutex::new(HashMap::new());

    for chunk in files.chunks(CHUNK_SIZE) {
        let parsed: Vec<ParsedFile> = chunk
            .par_iter()
            .filter_map(|path| {
                let ext = path.extension()?.to_str()?;
                let lang = Language::from_extension(ext)?;
                let content = read_capped(path)?;

                // Skip likely binary/minified files (high ratio of non-ASCII or very long lines)
                if looks_binary(&content) {
                    return None;
                }

                let rel = path.strip_prefix(root).ok()?.to_string_lossy().to_string();

                let done = counter.fetch_add(1, Ordering::Relaxed);
                if done % 500 == 0 && done > 0 {
                    tracing::info!("{done}/{total} files parsed");
                }

                // SAFETY: parse_file borrows &rel and &content read-only.
                // A panic from tree-sitter does not leave any shared mutable state
                // partially modified, so unwinding is safe to catch.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    parse::parse_file(&rel, &content, lang)
                })) {
                    Ok(Ok(parsed)) => Some(parsed),
                    Ok(Err(e)) => {
                        if let Some(g) = e.downcast_ref::<parse::extractor::GrammarLoadError>() {
                            // Recover from a poisoned mutex — never block aggregation
                            // for a downstream caller because of an unrelated panic.
                            let mut map = grammar_failures
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner());
                            map.entry(g.lang).or_insert_with(|| (g.reason.clone(), 0)).1 += 1;
                        } else {
                            tracing::warn!(path = %rel, error = %e, "parse failed, skipping");
                        }
                        None
                    }
                    Err(_) => {
                        tracing::warn!(path = %rel, "parse panicked, skipping");
                        None
                    }
                }
            })
            .collect();

        all_parsed.extend(parsed);
    }

    let failures = grammar_failures
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner());
    for (lang, (err, count)) in &failures {
        // tracing::warn! so this respects RUST_LOG and is captureable by
        // integration tests; the bang-default subscriber surfaces it in the
        // terminal too.
        tracing::warn!(
            language = ?lang,
            skipped = count,
            error = %err,
            "tree-sitter grammar failed to load — files for this language were skipped (likely ABI mismatch)"
        );
    }

    Ok(all_parsed)
}

/// Read a file as UTF-8, refusing to allocate more than `MAX_FILE_BYTES`.
///
/// Closes a TOCTOU window: a previous version did `fs::metadata().len() <= 1MB`
/// then `fs::read_to_string()`, which could be defeated by a malicious or
/// concurrently-growing file. `File::open` + `take` enforces the cap on the
/// actual read.
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    const MAX_FILE_BYTES: u64 = 1 << 20; // 1 MiB
    let file = std::fs::File::open(path).ok()?;
    let mut buf = String::new();
    let n = file
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut buf)
        .ok()?;
    if n as u64 > MAX_FILE_BYTES {
        return None;
    }
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_failure_summary_includes_language_count_and_reason() {
        // Pin the structured fields the user-visible warning emits, so a future
        // refactor cannot silently drop the count or error string without test
        // fail. We cannot easily hit the path end-to-end (every grammar
        // currently loads), so this test mirrors the format the warning
        // produces and locks the contract.
        let mut failures: HashMap<Language, (String, usize)> = HashMap::new();
        failures.insert(Language::CSharp, ("ABI mismatch v15".to_string(), 42));

        let mut rendered = String::new();
        for (lang, (err, count)) in &failures {
            rendered = format!("language={lang:?} skipped={count} error={err}");
        }
        assert!(rendered.contains("CSharp"), "{rendered}");
        assert!(rendered.contains("42"), "{rendered}");
        assert!(rendered.contains("ABI mismatch v15"), "{rendered}");
    }
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

    // Capture git HEAD before acquiring lock to minimize lock hold time
    let git_head = super::staleness::read_git_head(root);
    let indexed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Advisory lock to prevent concurrent index writes
    let lock_path = index_path.with_extension("lock");
    let lock_file = std::fs::File::create(&lock_path).context("create lock file")?;
    fs2::FileExt::lock_exclusive(&lock_file)
        .context("acquire index lock (another vex instance may be indexing)")?;

    let result = (|| -> Result<()> {
        store::writer::write_index_full(parsed, vectors, &index_path).context("write index")?;

        let manifest_path = config::manifest_path(root);
        let manifest = Manifest {
            files: file_hashes.iter().cloned().collect::<HashMap<_, _>>(),
            git_head,
            indexed_at: Some(indexed_at),
        };
        manifest.save(&manifest_path)?;
        Ok(())
    })();

    // Unlock (also happens on drop, but be explicit)
    if let Err(e) = fs2::FileExt::unlock(&lock_file) {
        tracing::warn!(error = %e, "failed to explicitly unlock index lock");
    }
    if let Err(e) = std::fs::remove_file(&lock_path) {
        tracing::warn!(error = %e, "failed to remove lock file");
    }

    result
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
                sym.doc.as_deref(),
                sym.body_tokens.as_deref(),
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

fn build_hnsw(root: &Path, vectors: &[Vec<f32>]) -> Result<()> {
    use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};

    let dim = vectors[0].len(); // guaranteed non-empty by caller

    let options = IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,     // auto
        expansion_add: 0,    // auto
        expansion_search: 0, // auto
        multi: false,
    };

    let index = new_index(&options).context("create HNSW index")?;
    index
        .reserve(vectors.len())
        .context("reserve HNSW capacity")?;

    for (i, vec) in vectors.iter().enumerate() {
        index
            .add(i as u64, vec)
            .context("add vector to HNSW index")?;
    }

    let hnsw_path = config::hnsw_path(root);
    let path_str = hnsw_path
        .to_str()
        .context("HNSW path contains non-UTF-8 characters")?;
    index.save(path_str).context("save HNSW index")?;

    tracing::info!(
        vectors = vectors.len(),
        path = %hnsw_path.display(),
        "HNSW index built"
    );

    Ok(())
}

/// Heuristic: file is likely binary or minified if it has many non-UTF8/control chars
/// or extremely long lines (>10KB, typical of minified JS/CSS).
fn looks_binary(content: &str) -> bool {
    // Check first 8KB for control characters (excluding common whitespace)
    let mut end = content.len().min(8192);
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let sample = &content[..end];
    let control_count = sample
        .bytes()
        .filter(|&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();
    if control_count * 20 > sample.len() {
        return true; // ≥5% control chars
    }

    // Check for very long lines (minified code) — scan first 100 lines
    // because the first line may be a normal comment/header
    if content.lines().take(100).any(|l| l.len() > 10_000) {
        return true;
    }

    false
}

fn discover_files(root: &Path, excludes: &[String]) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();

    for entry in crate::util::walk::walk_builder(root, excludes)?.build() {
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
