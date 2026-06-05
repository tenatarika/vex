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
        // v1.13 P5: vectors are L2-normalized by `pipeline::run` /
        // `pipeline::update` before they reach this writer. Only
        // meaningful when vectors are present; `None` for the
        // no-embeddings case keeps pre-1.13 readers happy and avoids
        // a misleading "normalized: true" for an empty vector array.
        vectors_normalized: (!vectors.is_empty()).then_some(true),
        // v1.14: unconditional `Some(true)` — every index written by this
        // build performed Pass-2 C++ include resolution. The flag is a
        // version marker, not a project-content predicate (pure-Rust
        // projects still get `Some(true)` because the resolver ran over
        // an empty C++ set). Pre-1.14 manifests have `None` and `vex
        // status` surfaces that as "re-run `vex index` to enable".
        cpp_includes_processed: Some(true),
    };
    manifest.save(&manifest_path)?;
    Ok(())
}

/// v1.13 E2b: persistent embedding cache. Contexts whose
/// `xxh3(embedder_id || \0 || ctx)` hits the on-disk cache reuse the
/// stored vector and skip the embed step. ONNX model load is deferred
/// until at least one miss — `vex update` runs with no embedding-
/// affecting changes (e.g. comment-only edits on already-indexed
/// files, or no-op re-runs) skip the ~80 MB model load entirely.
///
/// Cache is updated in-place with newly-embedded vectors and saved
/// atomically before return. Cache load failures degrade silently
/// to the all-miss path — never block the embedding run.
/// Returns `(vectors, hashes)` paired by sym_idx. `hashes[i]` is the
/// `context_hash` that gates `vectors[i]` in the embed cache AND that
/// the HNSW index keys by (v1.14.1 B1.1). Callers thread `hashes`
/// through to `build_hnsw` so the search path can map HNSW results
/// back to sym_idx via the `hash_index` sidecar.
pub(super) fn generate_embeddings(
    parsed: &[ParsedFile],
    embedder_id: &str,
    root: &Path,
) -> Result<(Vec<Vec<f32>>, Vec<u64>)> {
    let total_start = Instant::now();

    // Resolve embedder dim + char budget WITHOUT touching the ONNX
    // model — these are compile-time consts for each known embedder.
    // Lets us pre-build contexts and probe the cache before deciding
    // whether the model load is needed at all.
    let dim = embed::embedder_dim(embedder_id)
        .with_context(|| format!("unknown embedder `{embedder_id}` — no recorded dim"))?;
    let budget = embed::embedder_char_budget(embedder_id)
        .with_context(|| format!("unknown embedder `{embedder_id}` — no recorded char budget"))?;

    // Step 1: build context strings + hashes in output order.
    //
    // v1.14.1 — parallelised via rayon. `build_context` (string assembly
    // + identifier tokenisation + path-keyword extraction) plus
    // `xxh3_64` over the result averages ~5μs/symbol; at 50k symbols
    // the sequential loop cost ~250ms on a warm M1, an outright second
    // on slower laptops. `par_iter().unzip()` preserves source order
    // (rayon contract) so the resulting `(contexts, hashes)` Vecs stay
    // sym_idx-aligned with everything downstream (cache lookup,
    // `hash_index` sidecar, vector slot). Pure CPU, no model load —
    // safe to run before the ONNX init decision below.
    let step1_start = Instant::now();
    let pairs: Vec<(&ParsedFile, &crate::index::symbols::ParsedSymbol)> = parsed
        .iter()
        .flat_map(|f| f.symbols.iter().map(move |s| (f, s)))
        .collect();
    let (contexts, hashes): (Vec<String>, Vec<u64>) = {
        use rayon::prelude::*;
        pairs
            .par_iter()
            .map(|(file, sym)| {
                let ctx = embed::build_context(
                    sym.kind.as_str(),
                    &sym.name,
                    &file.path,
                    sym.signature.as_deref(),
                    sym.doc.as_deref(),
                    sym.body_tokens.as_deref(),
                    budget,
                );
                let h = embed::cache::context_hash(embedder_id, &ctx);
                (ctx, h)
            })
            .unzip()
    };
    let total = contexts.len();
    tracing::debug!(
        symbols = total,
        elapsed = ?step1_start.elapsed(),
        "embed: step 1 parallel context+hash build complete"
    );

    // Step 2: load cache + partition into hits / misses.
    let cache_path = config::embed_cache_path(root, embedder_id);
    let mut cache = embed::cache::EmbedCache::load(&cache_path, embedder_id, dim);

    let mut all_vectors: Vec<Vec<f32>> = vec![Vec::new(); total];
    let mut miss_indices: Vec<usize> = Vec::new();
    for (i, &h) in hashes.iter().enumerate() {
        match cache.get(h) {
            Some(cached) => all_vectors[i] = cached.to_vec(),
            None => miss_indices.push(i),
        }
    }

    let hits = total - miss_indices.len();
    let misses = miss_indices.len();
    tracing::info!(
        total,
        hits,
        misses,
        cache_size_before = cache.len(),
        "embed cache partition"
    );

    // Step 3: if everything hit, return immediately — no ONNX load.
    if misses == 0 {
        tracing::info!(
            total,
            elapsed = ?total_start.elapsed(),
            "embedding complete (all cached, model load skipped)"
        );
        return Ok((all_vectors, hashes));
    }

    // Step 4: embed misses only.
    let model_start = Instant::now();
    tracing::info!(embedder = embedder_id, "loading embedding model");
    let mut embedder = embed::make_embedder(embedder_id)?;
    tracing::info!(
        elapsed = ?model_start.elapsed(),
        model = embedder.id(),
        dim = embedder.dim(),
        "model loaded"
    );

    let embed_start = Instant::now();
    // Collect miss contexts as owned `String`s; embed_batch takes
    // `&[String]`. Slice borrowing across the chunks would require a
    // separate vec anyway, so just clone — cost is dominated by the
    // embed call itself.
    let miss_contexts: Vec<String> = miss_indices.iter().map(|&i| contexts[i].clone()).collect();
    let mut miss_vectors: Vec<Vec<f32>> = Vec::with_capacity(misses);
    for batch in miss_contexts.chunks(EMBED_BATCH_SIZE) {
        let vectors = embedder.embed_batch(batch)?;
        miss_vectors.extend(vectors);
    }
    tracing::info!(
        misses,
        elapsed = ?embed_start.elapsed(),
        "embedding misses complete"
    );

    // Step 5: place miss vectors into output + update cache.
    for (j, &out_idx) in miss_indices.iter().enumerate() {
        let vec = miss_vectors[j].clone();
        cache.insert(hashes[out_idx], vec.clone());
        all_vectors[out_idx] = vec;
    }

    // NOTE: E3 mark-and-sweep used to live here but ran against `hashes`,
    // which in the `vex update` path is the **changed-files-only** subset
    // — it would evict every unchanged symbol's cache entry, defeating
    // the cache on the very next update. Sweep is now hoisted to
    // `prune_embed_cache`, called from the pipeline orchestrator
    // (`pipeline::run` / `pipeline::update`) after the full set of live
    // hashes is known. See `prune_embed_cache` below.

    // Step 6: persist cache. Failure is non-fatal — vectors are still
    // returned; next run will re-embed and re-attempt to persist.
    if let Err(e) = cache.save(&cache_path) {
        tracing::warn!(
            path = %cache_path.display(),
            error = %e,
            "embed cache: save failed; next run will rehash misses"
        );
    } else {
        tracing::info!(
            cache_size_after = cache.len(),
            path = %cache_path.display(),
            "embed cache persisted"
        );
    }

    tracing::info!(
        total,
        elapsed = ?total_start.elapsed(),
        "embedding complete"
    );
    Ok((all_vectors, hashes))
}

/// v1.14.1 E3 — load the embed cache, sweep orphan entries against the
/// **full** live-hash set, and save it back. Called by the pipeline
/// orchestrator (`pipeline::run` and `pipeline::update`) once the full
/// set of currently-indexed `context_hash`es is known. NOT called from
/// inside `generate_embeddings` because the update path invokes it with
/// only the changed-files slice — sweeping against that subset would
/// silently evict every unchanged symbol's cache entry, defeating the
/// cache on the next update (the bug the original E3 inadvertently
/// introduced; rust-reviewer found it before the change landed).
///
/// Cache-load failure (missing/malformed sidecar) is the cold-start
/// path: `EmbedCache::load` returns empty and there's nothing to sweep.
/// Save failure is non-fatal — we log and continue; next run rehashes.
pub(super) fn prune_embed_cache(
    root: &Path,
    embedder_id: &str,
    dim: u32,
    live_hashes: &[u64],
) -> Result<()> {
    let cache_path = config::embed_cache_path(root, embedder_id);
    let mut cache = embed::cache::EmbedCache::load(&cache_path, embedder_id, dim);
    if cache.is_empty() {
        return Ok(());
    }
    let before = cache.len();
    let swept = cache.sweep_to(live_hashes);
    if swept == 0 {
        return Ok(());
    }
    tracing::info!(
        swept,
        cache_size_before = before,
        cache_size_after = cache.len(),
        "embed cache: reclaimed orphan entries (E3 mark-and-sweep)"
    );
    if let Err(e) = cache.save(&cache_path) {
        tracing::warn!(
            path = %cache_path.display(),
            error = %e,
            "embed cache: post-sweep save failed; orphans will reappear next run"
        );
    }
    Ok(())
}

/// Compute `context_hash` for every symbol in `parsed`, in sym_idx
/// order. Used by the v1.14.1 B1.1 update path which already has the
/// merged `all_vectors` (unchanged + freshly-embedded) but needs the
/// matching `hashes` slice to key the HNSW. `generate_embeddings`
/// produces hashes only for changed files; this helper covers the
/// whole index in one pass.
///
/// **Note on stability across builds:** `body_tokens` participates in
/// the hash. Symbols reconstructed from an existing index have
/// `body_tokens: None` (the field isn't persisted today — see
/// `parse_files::reconstruct_unchanged`), so the same symbol's hash
/// will differ between a fresh `vex index` and a subsequent `vex
/// update` that reconstructs it. That's fine for B1.1's full-rebuild
/// path (the sidecar is regenerated from scratch); B1.2's incremental
/// path will need either persisted body_tokens or a body-agnostic
/// hash to keep this stable.
pub(super) fn compute_hashes_for(parsed: &[ParsedFile], embedder_id: &str) -> Result<Vec<u64>> {
    let budget = embed::embedder_char_budget(embedder_id)
        .with_context(|| format!("unknown embedder `{embedder_id}` — no recorded char budget"))?;
    // Parallel mirror of `generate_embeddings` Step 1. The update path
    // calls this over the full merged corpus (unchanged + newly parsed),
    // potentially 50k+ symbols — sequential would re-introduce the
    // ~250ms wall-clock bottleneck Step 1 was parallelised away from.
    // Same ordered-unzip contract: rayon preserves source order so the
    // resulting Vec stays sym_idx-aligned with `all_vectors` and the
    // `index.hashes` sidecar.
    use rayon::prelude::*;
    let pairs: Vec<(&ParsedFile, &crate::index::symbols::ParsedSymbol)> = parsed
        .iter()
        .flat_map(|f| f.symbols.iter().map(move |s| (f, s)))
        .collect();
    let hashes: Vec<u64> = pairs
        .par_iter()
        .map(|(file, sym)| {
            let ctx = embed::build_context(
                sym.kind.as_str(),
                &sym.name,
                &file.path,
                sym.signature.as_deref(),
                sym.doc.as_deref(),
                sym.body_tokens.as_deref(),
                budget,
            );
            embed::cache::context_hash(embedder_id, &ctx)
        })
        .collect();
    Ok(hashes)
}

/// v1.14.1 B1.1: build HNSW keyed by `context_hash` (not by sym_idx).
/// Content-based keys are stable across `vex update` runs — the
/// prerequisite for B1.2 incremental update. The pairing sidecar
/// `index.hashes` is written alongside so the query path can map HNSW
/// results back to sym_idx via `search::hash_index::load`.
///
/// Pre-1.14.1 indexes keyed by sym_idx are rebuilt on next `vex
/// index`; `HnswHandle::open` requires the sidecar to be present so a
/// stale numeric-keyed `index.hnsw` without a sidecar degrades to
/// brute-force search (same as a missing HNSW altogether).
pub(super) fn build_hnsw(root: &Path, vectors: &[Vec<f32>], hashes: &[u64]) -> Result<()> {
    let hnsw_path = config::hnsw_path(root);
    let hash_index_path = config::hash_index_path(root);
    build_hnsw_at(&hnsw_path, &hash_index_path, vectors, hashes)
}

/// Core HNSW + sidecar builder with explicit paths. The `build_hnsw`
/// wrapper above resolves them via `config::hnsw_path` /
/// `config::hash_index_path`; this layer is split out so unit tests can
/// drive the same code path without touching the `set_cache_override`
/// `OnceLock` (which under `cargo test` is shared across thread-parallel
/// sibling tests and produces the wrong cache dir for whichever ran
/// second). Production callers should always go through `build_hnsw`.
pub(super) fn build_hnsw_at(
    hnsw_path: &Path,
    hash_index_path: &Path,
    vectors: &[Vec<f32>],
    hashes: &[u64],
) -> Result<()> {
    use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};

    anyhow::ensure!(
        vectors.len() == hashes.len(),
        "build_hnsw: vectors/hashes length mismatch ({} vs {})",
        vectors.len(),
        hashes.len(),
    );

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

    for (vec, &h) in vectors.iter().zip(hashes.iter()) {
        index.add(h, vec).context("add vector to HNSW index")?;
    }

    let path_str = hnsw_path
        .to_str()
        .context("HNSW path contains non-UTF-8 characters")?;
    index.save(path_str).context("save HNSW index")?;

    // Sidecar: paired sym_idx-ordered hash list. Without this the query
    // path can't materialise the hash→sym_idx mapping HnswHandle needs.
    crate::search::hash_index::save(hash_index_path, hashes)
        .context("save HNSW hash-index sidecar")?;

    tracing::info!(
        vectors = vectors.len(),
        path = %hnsw_path.display(),
        sidecar = %hash_index_path.display(),
        "HNSW index built"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{cache::context_hash, MINILM_CHAR_BUDGET, MINILM_DIM, MINILM_ID};
    use crate::index::symbols::{ParsedSymbol, SymbolKind};

    fn mk_sym(name: &str, line: usize) -> ParsedSymbol {
        ParsedSymbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            line,
            signature: Some(format!("fn {name}()")),
            doc: None,
            body_tokens: None,
        }
    }

    /// End-to-end E2b check: when every context hashes to a cache hit,
    /// `generate_embeddings` returns the cached vectors and does NOT
    /// touch the ONNX model. The proof-of-no-model-load is implicit
    /// (this test runs in < 100 ms vs > 1 s for an ONNX load + first
    /// embed) but the public-API behavior — correct vectors returned
    /// in symbol order — is what users actually depend on.
    #[test]
    fn all_hit_returns_cached_vectors_without_model_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let cache_root = root.join(".vex_cache");
        std::fs::create_dir_all(&cache_root).unwrap();
        std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
        crate::util::config::set_cache_override(cache_root, false);

        let parsed = vec![ParsedFile {
            path: "a.rs".to_string(),
            symbols: vec![mk_sym("foo", 1), mk_sym("bar", 10)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
        }];

        // Pre-seed the cache with synthetic vectors keyed by the same
        // hash function `generate_embeddings` will use. Using
        // distinguishable per-symbol vectors so we can prove the
        // results came back in the right output order.
        let ctx_foo = embed::build_context(
            "function",
            "foo",
            "a.rs",
            Some("fn foo()"),
            None,
            None,
            MINILM_CHAR_BUDGET,
        );
        let ctx_bar = embed::build_context(
            "function",
            "bar",
            "a.rs",
            Some("fn bar()"),
            None,
            None,
            MINILM_CHAR_BUDGET,
        );
        let h_foo = context_hash(MINILM_ID, &ctx_foo);
        let h_bar = context_hash(MINILM_ID, &ctx_bar);
        let mut v_foo = vec![0.0_f32; MINILM_DIM as usize];
        v_foo[0] = 1.0;
        let mut v_bar = vec![0.0_f32; MINILM_DIM as usize];
        v_bar[1] = 1.0;

        let mut cache = embed::cache::EmbedCache::empty(MINILM_ID, MINILM_DIM);
        cache.insert(h_foo, v_foo.clone());
        cache.insert(h_bar, v_bar.clone());
        let cache_path = crate::util::config::embed_cache_path(root, MINILM_ID);
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        cache.save(&cache_path).unwrap();

        // The call must NOT load ONNX (no network, no model file in
        // tmp). If the all-hit early-return ever regresses, the test
        // either hangs on download or panics in `make_embedder`.
        let (out, hashes) =
            generate_embeddings(&parsed, MINILM_ID, root).expect("generate_embeddings");

        assert_eq!(out.len(), 2);
        assert_eq!(out[0], v_foo, "foo position 0");
        assert_eq!(out[1], v_bar, "bar position 1");
        // v1.14.1 B1.1: the returned hashes must be sym_idx-ordered
        // and identical to the cache keys — that's the contract
        // `build_hnsw` and the `index.hashes` sidecar rely on.
        assert_eq!(hashes, vec![h_foo, h_bar]);
    }

    /// v1.14.1 B1.1 end-to-end: build a hash-keyed HNSW from a tiny
    /// fixture, open it via `HnswHandle`, query the v_foo vector and
    /// confirm the top result maps back to sym_idx 0. This is the
    /// query path the rest of vex relies on for `vex search
    /// --semantic` / `vex similar` / `vex duplicates` once the index
    /// carries v1.14.1 sidecars. Catches regressions in either the
    /// builder (sidecar / HNSW pair drift) or the handle's
    /// `hash → sym_idx` translation.
    #[test]
    fn build_hnsw_with_hashes_and_query_returns_sym_idx() {
        // Uses `build_hnsw_at` with explicit paths so this test never
        // touches `crate::util::config::set_cache_override` — that's an
        // `OnceLock<CacheLayout>` and under `cargo test` the sibling
        // `all_hit_returns_cached_vectors_without_model_load` test
        // racing for it would leave whichever ran second pointing at a
        // dropped TempDir. The production wrapper `build_hnsw(root, ..)`
        // does the config lookup; `build_hnsw_at` covers the same
        // builder + sidecar path with zero process-wide state.
        let tmp = tempfile::TempDir::new().unwrap();
        let hnsw_path = tmp.path().join("index.hnsw");
        // `HnswHandle::open` derives the sidecar as `hnsw_path.parent()
        // .join("index.hashes")`, so co-locating the two files mirrors
        // the production layout.
        let hash_index_path = tmp.path().join("index.hashes");

        // Two orthogonal MiniLM-shaped vectors at sym_idx 0 and 1 with
        // synthetic but plausible context hashes. Orthogonal so the
        // top-1 result is unambiguous (cosine = 1 for the query vector
        // matching its own stored copy, ~0 for the other).
        let dim = MINILM_DIM as usize;
        let mut v_foo = vec![0.0_f32; dim];
        v_foo[0] = 1.0;
        let mut v_bar = vec![0.0_f32; dim];
        v_bar[1] = 1.0;
        let h_foo: u64 = 0xFEED_C0FF_EEC0_DE42;
        let h_bar: u64 = 0x00BA_0BAB_DEAD_BEEF;

        build_hnsw_at(
            &hnsw_path,
            &hash_index_path,
            &[v_foo.clone(), v_bar.clone()],
            &[h_foo, h_bar],
        )
        .expect("build_hnsw_at");

        // Both files must exist after the build — `HnswHandle::open`
        // bails on a missing sidecar even if the HNSW is fine.
        assert!(hnsw_path.exists(), "HNSW file must exist");
        assert!(hash_index_path.exists(), "hash-index sidecar must exist");

        // Open the handle: expected_symbols = 2 (matches both HNSW size
        // and sidecar length). Should succeed; missing sidecar would
        // return None.
        let handle =
            crate::search::semantic::HnswHandle::open(&hnsw_path, dim, 2).expect("open HnswHandle");

        // Query with v_foo — top-1 must be sym_idx 0 with similarity
        // ~1.0 (the stored vector under hash h_foo).
        let results = handle.search(&v_foo, 1).expect("search succeeds");
        assert_eq!(results.len(), 1);
        let (sym_idx, sim) = results[0];
        assert_eq!(sym_idx, 0, "top-1 must resolve to sym_idx 0 via hash map");
        assert!(sim > 0.99, "self-similarity should be ~1.0, got {sim}");
    }
}
