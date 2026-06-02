//! `vex search` — hybrid structural + BM25 + semantic search with RRF
//! fusion. Extracted from `cli/mod.rs` in S1 Group D.2.

use anyhow::{Context, Result};

use super::args::{DiffFilterArgs, MetadataArgs, OutputFormat, ScopeArgs};
use super::common::{
    apply_path_filters, build_metadata_filter, check_embedder_match, diff_filter_meta,
    resolve_diff_filter, resolve_embedder, resolve_root, resolve_semantic, CmdCtx,
};
use super::index_management::ensure_index_ready;
use super::{output, scope};
use crate::search::{fusion, semantic, structural};
use crate::store::reader::IndexReader;
use crate::util::config;

#[allow(clippy::too_many_arguments)]
pub(crate) fn search(
    ctx: &CmdCtx<'_>,
    query: String,
    limit: usize,
    semantic: bool,
    no_semantic: bool,
    filter_path: Option<String>,
    kind: Vec<String>,
    context_path: Option<String>,
    auto_update: bool,
    no_stale_check: bool,
    no_bm25: bool,
    meta: MetadataArgs,
    why: bool,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
) -> Result<()> {
    let semantic = resolve_semantic(semantic, no_semantic, ctx.cfg);
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let metadata_filter = build_metadata_filter(&meta)?;
    let root = resolve_root(None)?.canonicalize()?;
    // Resolve the diff scope once per invocation. None when no
    // `--since*` / `--changed-only` flag was set.
    let changed_paths = resolve_diff_filter(&root, &diff)?;
    let index_path = ensure_index_ready(
        &root,
        auto_update,
        no_stale_check,
        semantic,
        ctx.local_cache_active,
        ctx.cfg,
    )?;

    let reader = IndexReader::open(&index_path).context("open index")?;

    // Over-fetch when a path filter is active — the post-filter
    // `take(limit)` runs AFTER the results are produced, so a narrow
    // include/exclude or substring would silently truncate matches.
    // Bound by `symbol_count()`, not `usize::MAX`, because index-backed
    // results cannot exceed the symbol table. The 13.7-D3 diff filter
    // is treated identically — a `--since` window can be much narrower
    // than the include/exclude globs, so the same `symbol_count()`
    // ceiling applies.
    let fetch_limit = if filter_path.is_some() || !path_scope.is_empty() || changed_paths.is_some()
    {
        reader.symbol_count()
    } else {
        limit
    };

    let structural_results = structural::search_with_fuzzy(&reader, &query, fetch_limit);

    // BM25 channel: auto-on when the index has BM25 data, opt-out
    // with `--no-bm25`. Returns empty for short queries or when no
    // term hits — safe to always run.
    let bm25_results = if !no_bm25 && reader.has_bm25() {
        crate::search::bm25::search(&reader, &query, fetch_limit)
    } else {
        Vec::new()
    };

    // Capture pre-fusion channel snapshots. `--why` needs them for
    // the trace; `--format json` needs them for the per-result
    // `signals` block in the response envelope (Phase 13.11). Clone
    // only when at least one consumer is active so the text/compact
    // fast path stays allocation-free.
    let want_prefusion = why || matches!(ctx.format, OutputFormat::Json);
    let trace_structural = if want_prefusion {
        structural_results.clone()
    } else {
        Vec::new()
    };
    let trace_bm25 = if want_prefusion {
        bm25_results.clone()
    } else {
        Vec::new()
    };
    let mut trace_semantic: Vec<crate::search::SearchResult> = Vec::new();

    let results = if semantic && reader.has_vectors() {
        let embedder_id = resolve_embedder(None, ctx.cfg);
        // Warn (but don't fail) when the manifest doesn't record an
        // embedder — typically a pre-9.1 index or a deleted manifest.
        // We fall back to assuming DEFAULT_EMBEDDER; if the user
        // configured something else they'll see the warning and can
        // rebuild explicitly.
        let manifest = crate::index::manifest::Manifest::load(&config::manifest_path(&root))?;
        if manifest.embedder_id.is_none() && embedder_id != crate::embed::DEFAULT_EMBEDDER {
            eprintln!(
                "Warning: index manifest has no recorded embedder; assuming \
                 `{}`. If the index was built with `{embedder_id}` the results \
                 may be off — rebuild with `vex index --semantic --embedder {embedder_id}` \
                 to make it explicit.",
                crate::embed::DEFAULT_EMBEDDER
            );
        }
        check_embedder_match(&root, &embedder_id)?;
        let mut embedder =
            crate::embed::make_embedder(&embedder_id).context("load embedding model")?;
        let hnsw_path = config::hnsw_path(&root);
        let semantic_results = semantic::search_with_embedder(
            &reader,
            embedder.as_mut(),
            &query,
            fetch_limit,
            &hnsw_path,
        )?;
        if want_prefusion {
            trace_semantic = semantic_results.clone();
        }
        fusion::fuse3(structural_results, bm25_results, semantic_results, limit)
    } else {
        if semantic && !reader.has_vectors() {
            eprintln!("Warning: no embeddings in index. Run `vex index --semantic` first.");
        }
        if bm25_results.is_empty() {
            structural_results
        } else {
            // 2-channel fusion when semantic is off but BM25 is available.
            fusion::fuse_many(vec![structural_results, bm25_results], limit)
        }
    };

    let rerank_ctx = crate::search::rerank::RerankContext {
        kind_hints: crate::search::rerank::KindSelector::parse_many(&kind)?,
        context_path: context_path.as_deref(),
    };
    let results = crate::search::rerank::rerank(&query, &rerank_ctx, results);
    // Phase 13.7-D3: apply the diff filter BEFORE `take(limit)` so a
    // narrow change-set doesn't first get truncated by `--limit` and
    // then look smaller than it should.
    let pre_diff_results = apply_path_filters(results, filter_path.as_deref(), &path_scope);
    let pre_diff_count = pre_diff_results.len();
    let post_diff_results: Vec<_> = if let Some(ref cp) = changed_paths {
        pre_diff_results
            .into_iter()
            .filter(|r| cp.contains(&r.path))
            .collect()
    } else {
        pre_diff_results
    };
    let post_diff_count = post_diff_results.len();
    let diff_retained = post_diff_count;
    let diff_dropped = pre_diff_count.saturating_sub(post_diff_count);
    let results: Vec<_> = post_diff_results
        .into_iter()
        .filter(|r| metadata_filter.matches(r.signature.as_deref()))
        .take(limit)
        .collect();

    if why {
        let filter = crate::search::trace::FilterSnapshot {
            filter: filter_path.clone(),
            include: scope.include.clone(),
            exclude: scope.exclude.clone(),
            kind: kind.clone(),
        };
        let trace = crate::search::trace::SearchTrace::from_channels(
            &query,
            &trace_structural,
            &trace_bm25,
            &trace_semantic,
            &results,
            filter,
        );
        // stderr so `vex search Foo --why | jq` keeps working —
        // stdout stays a pure result list.
        crate::cli::trace::emit_why_trace(&trace)?;
        if let Some(df) =
            diff_filter_meta(&diff, changed_paths.as_ref(), diff_retained, diff_dropped)
        {
            crate::cli::trace::emit_diff_filter(&df)?;
        }
    }

    // v1.12.0 S8.2 — signal "no results" once, regardless of format. The
    // JSON path emits an empty results array (caller-friendly); the text
    // path prints `No results for "..."`. Both produce exit code 1 via
    // `cli::exit_code::finish`.
    if results.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
        OutputFormat::Json => {
            // Build per-result signals via the same (path, name, line)
            // keying fusion uses, then wrap in the Phase 13 envelope.
            let signals = crate::protocol::signals::build_signals(
                &trace_structural,
                &trace_bm25,
                &trace_semantic,
                &results,
            );
            let manifest_path = config::manifest_path(&root);
            let mut meta = output::build_search_meta(&manifest_path);
            meta.diff_filter =
                diff_filter_meta(&diff, changed_paths.as_ref(), diff_retained, diff_dropped);
            output::print_search_envelope(&results, &signals, meta);
        }
        OutputFormat::Text | OutputFormat::Compact => {
            if results.is_empty() {
                println!("No results for \"{query}\"");
            } else {
                let is_fuzzy = results
                    .iter()
                    .any(|r| matches!(r.match_type, crate::search::MatchType::Fuzzy));
                if is_fuzzy {
                    eprintln!("(fuzzy match — no exact results for \"{query}\")\n");
                }
                output::print_results(&results, &ctx.format);
            }
        }
    }
    Ok(())
}
