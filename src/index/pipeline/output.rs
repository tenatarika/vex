//! Index output side — building the in-memory sections (BM25, call
//! graph, pattern skeletons), generating embeddings, building the HNSW
//! semantic index, and writing the binary index file.
//!
//! `write_output_locked` is the entry point: it stitches every section
//! together and calls into `store::writer` for the final atomic write.
//! The caller MUST already hold the [`super::IndexLock`] — both `run`
//! and `update` acquire it before this work runs.
//!
//! Isolated from `mod.rs` so the orchestration (manifest re-check,
//! lock handling, decisions about what to rebuild) stays separable
//! from the mechanical "build each section + write" step.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::embed;
use crate::index::manifest::Manifest;
use crate::index::symbols::ParsedFile;
use crate::parse::language::Language;
use crate::store;
use crate::util::config;

use super::{IndexOptions, EMBED_BATCH_SIZE};

/// Pick the `vector_dim` to record in the Header.
///
/// Priority: actual length of the first vector → registry lookup by id →
/// default MiniLM dim. The first wins so non-embedded indexes still record a
/// sensible legacy default.
pub(super) fn vector_dim_for(embedder_id: &str, vectors: &[Vec<f32>]) -> u32 {
    if let Some(first) = vectors.first() {
        return first.len() as u32;
    }
    embed::embedder_dim(embedder_id).unwrap_or(embed::MINILM_DIM)
}

/// Build the BM25 index from per-symbol term bags.
///
/// Each symbol becomes a document whose terms are drawn from:
/// - `name` (split on non-alnum, lowercased)
/// - `signature` (same split)
/// - `body_tokens` (already extracted, space-separated)
/// - `doc` (docstring; same split)
///
/// Returns `(fst_bytes, postings_bytes, stats_bytes)`. The triple is empty
/// when there are no documents (no symbols across all parsed files).
pub(super) fn build_bm25_index(parsed: &[ParsedFile]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    // `doc_count` counts every `ParsedSymbol`, including synthetic
    // `SymbolKind::Module` rows (Phase 14.1) that the inner loop skips
    // when populating term bags. The Module slots still reserve a
    // zero-length doc-length entry in BM25 stats so that `sym_idx` —
    // which is incremented for *every* symbol regardless of kind — stays
    // aligned with `SymbolRecord` positions in the records section. Net
    // overhead: 2 bytes per indexed file in the stats footer.
    let doc_count: usize = parsed.iter().map(|f| f.symbols.len()).sum();
    if doc_count == 0 {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    let mut builder = crate::store::bm25::Bm25IndexBuilder::new(doc_count);
    let mut sym_idx: u32 = 0;
    for file in parsed {
        for sym in &file.symbols {
            // Phase 14.1: synthetic Module symbols carry only call edges,
            // never BM25 content. Skip the bag-building but advance
            // `sym_idx` so the doc-id alignment with `SymbolRecord`
            // positions stays correct (doc slot stays zero-length).
            if sym.kind == crate::index::symbols::SymbolKind::Module {
                sym_idx += 1;
                continue;
            }
            let mut bag = String::with_capacity(256);
            bag.push_str(&sym.name);
            bag.push(' ');
            if let Some(s) = &sym.signature {
                bag.push_str(s);
                bag.push(' ');
            }
            if let Some(b) = &sym.body_tokens {
                bag.push_str(b);
                bag.push(' ');
            }
            if let Some(d) = &sym.doc {
                bag.push_str(d);
            }
            let terms = crate::store::bm25::tokenize_document(&bag);
            builder.add_document(sym_idx, &terms);
            sym_idx += 1;
        }
    }
    builder.build()
}

/// Resolve each [`RawCallEdge`] to a [`CallEdgeBuilder`] with a concrete
/// `caller_sym_idx`. Edges whose caller doesn't match any symbol in the
/// parsed-file iteration order are dropped — this happens for languages
/// where the call-graph query produces function names that the symbol
/// extractor doesn't register (e.g. inner closures, anonymous fns).
///
/// The key is `(file_path, fn_name, definition_line)` — the `line` is
/// load-bearing because a single file can contain multiple functions
/// sharing a name (overloaded methods, duplicate `impl` blocks, methods
/// with the same name across nested modules). Keying only on `(path, name)`
/// would cause every duplicate to inherit the first occurrence's symbol
/// index, attributing call sites to the wrong caller.
///
/// The iteration order MUST match what `writer::write_index_to` uses to
/// assign `symbol_idx` (`parsed.iter().flat_map(|f| f.symbols.iter())`)
/// otherwise the resulting indices will be wrong.
pub(super) fn resolve_call_edges(
    parsed: &[ParsedFile],
) -> Vec<crate::store::call_graph::CallEdgeBuilder> {
    let mut sym_idx_of: HashMap<(&str, &str, usize), u32> = HashMap::new();
    let mut next_idx: u32 = 0;
    for file in parsed {
        for sym in &file.symbols {
            // Preserve the first symbol when `(path, name, line)` collides
            // (genuinely identical entries — duplicate symbols at the same
            // line are a parser bug, not a real case). The line component
            // makes overloaded/same-name siblings distinct.
            sym_idx_of
                .entry((file.path.as_str(), sym.name.as_str(), sym.line))
                .or_insert(next_idx);
            next_idx += 1;
        }
    }

    // Phase 14.1: per-file synthetic Module symbol name lookup key. Only
    // allocate the `<module:path>` string when the file actually contains a
    // sentinel edge — empty `caller_fn_name` + `caller_fn_line == 0` is the
    // marker emitted by `callgraph::extract_call_edges` for module-scope
    // calls. By tree-sitter grammar invariants no real function definition
    // has an empty name or line 0, so this conjunction is collision-free.
    let mut out = Vec::new();
    for file in parsed {
        let module_sym_name = file
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name.is_empty() && e.caller_fn_line == 0)
            .then(|| format!("<module:{}>", file.path));
        for edge in &file.call_edges {
            let (caller_name, caller_line) =
                if edge.caller_fn_name.is_empty() && edge.caller_fn_line == 0 {
                    // `module_sym_name` is `Some` whenever any sentinel
                    // exists in this file — proven by the `any(...)` above.
                    (module_sym_name.as_deref().unwrap_or(""), 1)
                } else {
                    (edge.caller_fn_name.as_str(), edge.caller_fn_line)
                };
            let Some(&caller_sym_idx) =
                sym_idx_of.get(&(file.path.as_str(), caller_name, caller_line))
            else {
                continue;
            };
            out.push(crate::store::call_graph::CallEdgeBuilder {
                caller_sym_idx,
                callee_name: edge.callee_name.clone(),
                line: edge.line as u32,
            });
        }
    }
    out
}

/// Output of [`collect_pattern_skeletons`] — the flattened skeleton tuples
/// the writer consumes, paired with the per-language grammar fingerprints
/// Inc 5 will compare against the live grammar.
type SkeletonsForWriter = (
    Vec<(u32, crate::pattern::skeleton::Skeleton)>,
    Vec<(u8, u32)>,
);

/// Flatten per-file pattern skeletons into the `(file_id, Skeleton)`
/// shape the writer expects, and compute grammar fingerprints for the
/// distinct T1 languages that contributed at least one skeleton. The
/// fingerprint lets Inc 5 detect grammar drift between index build and
/// query time and fall back to live-scan when the on-disk skeletons no
/// longer agree with the live grammar.
pub(super) fn collect_pattern_skeletons(parsed: &[ParsedFile]) -> SkeletonsForWriter {
    let mut skeletons: Vec<(u32, crate::pattern::skeleton::Skeleton)> = Vec::new();
    let mut langs_with_skeletons: HashSet<Language> = HashSet::new();
    for (file_id, file) in parsed.iter().enumerate() {
        if file.skeletons.is_empty() {
            continue;
        }
        // Recover the language from the file extension — ParsedFile
        // does not carry it explicitly, but the extension is canonical
        // (parse_files uses Language::from_extension).
        let lang = std::path::Path::new(&file.path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension);
        if let Some(lang) = lang {
            langs_with_skeletons.insert(lang);
        }
        for sk in &file.skeletons {
            skeletons.push((file_id as u32, sk.clone()));
        }
    }
    let fingerprints: Vec<(u8, u32)> = langs_with_skeletons
        .into_iter()
        .map(|lang| {
            (
                lang.lang_id(),
                crate::store::pattern_skeletons::grammar_fingerprint_for_lang(lang),
            )
        })
        .collect();
    (skeletons, fingerprints)
}

/// Build the index sections and write them out. The caller MUST already hold
/// the [`IndexLock`]; both `run` and `update` acquire it before the expensive
/// parse + embed so concurrent instances serialize and skip redundant rebuilds
/// instead of all embedding in parallel.
#[allow(clippy::too_many_arguments)] // mirrors the writer entry shape
pub(super) fn write_output_locked(
    root: &Path,
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    file_hashes: &[(String, u64)],
    embedder_id: Option<String>,
    opts: IndexOptions,
    is_full_rebuild: bool,
) -> Result<()> {
    let index_path = config::index_path(root);
    let cache_dir = index_path.parent().context("index path has no parent")?;
    std::fs::create_dir_all(cache_dir).context("create cache directory")?;

    let git_head = crate::index::staleness::read_git_head(root);
    let indexed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Skip the corresponding build work entirely when the section is
    // opted out — the reader gates these via `*_len == 0`, so passing
    // empty slices / `None` is a valid disabled state in the format.
    let call_edges = if opts.with_call_graph {
        resolve_call_edges(parsed)
    } else {
        Vec::new()
    };

    let bm25_built = if opts.with_bm25 {
        Some(build_bm25_index(parsed)?)
    } else {
        None
    };
    let bm25 = bm25_built.as_ref().and_then(|(fst, posts, stats)| {
        if fst.is_empty() {
            None
        } else {
            Some((fst.as_slice(), posts.as_slice(), stats.as_slice()))
        }
    });
    // 11.4 Inc 4 — collect (file_id, Skeleton) tuples and compute
    // per-language grammar fingerprints. The empty-opt-out path
    // still produces a v6 index, just with all-zero header fields.
    let (pattern_skeletons, lang_fingerprints) = if opts.with_pattern_index {
        collect_pattern_skeletons(parsed)
    } else {
        (Vec::new(), Vec::new())
    };
    store::writer::write_index_with_call_graph_and_skeletons_and_fingerprints(
        parsed,
        vectors,
        vector_dim,
        &call_edges,
        bm25,
        &pattern_skeletons,
        &lang_fingerprints,
        &index_path,
    )
    .context("write index")?;

    // v1.12.0 T4 — build + persist the bloom sidecar. Failure is
    // non-fatal: callers fall back to direct FST lookups when the
    // sidecar is absent or corrupt, so a write error must not block
    // the index run. NOTE: ordering matters — `index.vex` is already
    // atomically in place. A failed bloom write leaves either no
    // sidecar (fresh first run) or a stale sidecar from a prior run.
    // Both are safe: bloom only adds false positives, never false
    // negatives, so even a stale sidecar can't make `vex check` miss a
    // present symbol.
    let bloom_path = config::bloom_path(root);
    let bloom = crate::search::bloom::SymbolBloom::from_parsed_files(parsed);
    if let Err(e) = bloom.save(&bloom_path) {
        tracing::warn!(
            path = %bloom_path.display(),
            error = %e,
            "failed to persist bloom sidecar (stale sidecar may remain); \
             cmd_check will fall back to FST or use the stale bitmap"
        );
    }

    let manifest_path = config::manifest_path(root);
    let manifest = Manifest {
        files: file_hashes.iter().cloned().collect::<HashMap<_, _>>(),
        git_head,
        indexed_at: Some(indexed_at),
        embedder_id,
        // Persist explicit `Some(false)` for opt-outs so `vex update`
        // can detect them. `Some(true)` rather than `None` for the
        // default case so post-10.3 manifests are unambiguous.
        call_graph: Some(opts.with_call_graph),
        bm25: Some(opts.with_bm25),
        pattern_index: Some(opts.with_pattern_index),
        // 11.4 Inc 5: `pattern_index_full` distinguishes a full
        // `vex index` from a `vex update`. The reader checks this
        // before using the indexed prefilter — incremental builds
        // produce a partial section (only re-parsed files have
        // skeletons) and would silently drop matches in unchanged
        // files. `is_update` is plumbed by the writer wrapper.
        pattern_index_full: Some(is_full_rebuild),
    };
    manifest.save(&manifest_path)?;
    Ok(())
}

pub(super) fn generate_embeddings(
    parsed: &[ParsedFile],
    embedder_id: &str,
) -> Result<Vec<Vec<f32>>> {
    let start = Instant::now();
    tracing::info!(embedder = embedder_id, "loading embedding model");
    let mut embedder = embed::make_embedder(embedder_id)?;
    let budget = embedder.char_budget();
    tracing::info!(
        elapsed = ?start.elapsed(),
        model = embedder.id(),
        dim = embedder.dim(),
        "model loaded"
    );

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
                budget,
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

pub(super) fn build_hnsw(root: &Path, vectors: &[Vec<f32>]) -> Result<()> {
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
