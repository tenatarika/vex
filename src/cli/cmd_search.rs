//! `vex search` — hybrid structural + BM25 + semantic search with RRF
//! fusion. Extracted from `cli/mod.rs` in S1 Group D.2.

use std::path::Path;

use anyhow::{Context, Result};

use super::args::{DiffFilterArgs, MetadataArgs, OutputFormat, ScopeArgs};
use super::common::{
    apply_path_filters, build_metadata_filter, check_embedder_match, diff_filter_meta,
    resolve_diff_filter, resolve_embedder, resolve_root, resolve_semantic, CmdCtx,
};
use super::index_management::ensure_index_ready;
use super::{output, scope};
use crate::search::{fusion, semantic, structural, SearchResult};
use crate::store::reader::IndexReader;
use crate::util::config::{self, VexConfig};
use crate::util::ident::is_identifier_shaped;
use crate::workspace;

/// Over-fetch multiplier for `--exclude-generated`, whose post-filter reads
/// file heads from disk. Bounds the number of probes to
/// `limit * GENERATED_OVER_FETCH` rather than the whole symbol table.
const GENERATED_OVER_FETCH: usize = 20;

/// The resolved per-invocation search request, shared by the single-repo
/// path and every member of a workspace fanout.
struct SearchReq<'a> {
    query: &'a str,
    limit: usize,
    /// Already resolved against `.vex.toml` (the raw `--semantic`/
    /// `--no-semantic` pair is collapsed before this struct is built).
    semantic: bool,
    filter_path: Option<&'a str>,
    kind: &'a [String],
    context_path: Option<&'a str>,
    no_bm25: bool,
    code_only: bool,
    exclude_generated: bool,
    meta: &'a MetadataArgs,
    scope: &'a ScopeArgs,
    diff: &'a DiffFilterArgs,
    auto_update: bool,
    no_stale_check: bool,
}

/// Everything one repo's search produces. `results` is the only field the
/// workspace path reads; the trace fields + reasons feed the single-repo
/// `--why` trace and JSON `signals`/`_meta` envelope.
struct SearchOutcome {
    results: Vec<SearchResult>,
    trace_structural: Vec<SearchResult>,
    trace_bm25: Vec<SearchResult>,
    trace_semantic: Vec<SearchResult>,
    semantic_channel_reason: Option<&'static str>,
    /// True when an identifier-shaped query returned zero structural (FST)
    /// hits, so the ranking drifted to neighbours (callers / imports) rather
    /// than a definition. Drives the stderr advisory (human path) and the
    /// `_meta.vex.dev/search_hint` envelope field (agent path).
    drifted: bool,
    diff_retained: usize,
    diff_dropped: usize,
    /// How many results `--exclude-generated` removed. Lower bound — the filter
    /// is lazy, so counting stops once `limit` rows are collected.
    generated_dropped: usize,
    changed_paths: Option<crate::util::git_diff::ChangedPaths>,
}

/// The human-readable search-drift advisory for an identifier-shaped query
/// that found no structural (FST) match. Shared by the stderr notice and the
/// JSON envelope's `_meta.vex.dev/search_hint.message` so the two never drift.
fn search_drift_message(query: &str) -> String {
    format!(
        "`vex search {query}` found no symbol named `{query}` in this index. \
         Hybrid ranking may surface callers / imports instead. For exact-symbol \
         lookup try `vex check {query}` (existence), `vex show {query}` \
         (definition body), or `vex usages {query} --strict` (every reference). \
         See `docs/COOKBOOK.md` FAQ for the full decision rule."
    )
}

/// One workspace member's search outcome, carried through the `--workspace`
/// fanout. The workspace envelope surfaces these per repo (the top-level
/// `_meta` can't carry per-member reasons).
struct RepoOutcome {
    repo: String,
    results: Vec<SearchResult>,
    /// `Some(reason)` if this member's index was stale; `None` when fresh.
    stale: Option<String>,
    /// Why the semantic channel did not contribute for this member, verbatim
    /// from `produce_results`: `None` = it ran (or wasn't degraded),
    /// `Some(NOT_REQUESTED)` = no `--semantic`, `Some(INDEX_LACKS_VECTORS)` =
    /// asked-for but no embeddings. Only the last is surfaced downstream (see
    /// the JSON/text emit sites); `NOT_REQUESTED` is suppressed as noise.
    semantic_channel: Option<&'static str>,
    /// Whether this member's search drifted (identifier-shaped query, zero
    /// structural hits). Aggregated across members to decide the workspace
    /// envelope's top-level `_meta.vex.dev/search_hint` — see the JSON branch.
    drifted: bool,
}

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
    code_only: bool,
    exclude_generated: bool,
    meta: MetadataArgs,
    why: bool,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
    workspace: bool,
) -> Result<()> {
    let semantic = resolve_semantic(semantic, no_semantic, ctx.cfg);
    let req = SearchReq {
        query: &query,
        limit,
        semantic,
        filter_path: filter_path.as_deref(),
        kind: &kind,
        context_path: context_path.as_deref(),
        no_bm25,
        code_only,
        exclude_generated,
        meta: &meta,
        scope: &scope,
        diff: &diff,
        auto_update,
        no_stale_check,
    };

    if workspace {
        return search_workspace(ctx, &req);
    }

    let root = resolve_root(None)?.canonicalize()?;
    let want_prefusion = why || matches!(ctx.format, OutputFormat::Json);
    let outcome = produce_results(
        &root,
        ctx.cfg,
        ctx.local_cache_active,
        &req,
        want_prefusion,
        true,
    )?;
    let SearchOutcome {
        results,
        trace_structural,
        trace_bm25,
        trace_semantic,
        semantic_channel_reason,
        drifted,
        diff_retained,
        diff_dropped,
        generated_dropped,
        changed_paths,
    } = outcome;

    if why {
        let filter = crate::search::trace::FilterSnapshot {
            filter: filter_path.clone(),
            include: scope.include.clone(),
            exclude: scope.exclude.clone(),
            kind: kind.clone(),
            code_only,
            exclude_generated,
            generated_dropped: (generated_dropped > 0).then_some(generated_dropped),
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
            meta.semantic_channel = semantic_channel_reason;
            // §4 agent-output: carry the search-drift advisory in the envelope
            // so MCP agents / JSON consumers see it (stderr is invisible to
            // them). NOT gated by `emit_diagnostics` — that only silences the
            // stderr copy for the workspace fanout.
            if drifted {
                meta.search_hint = Some(serde_json::json!({
                    "reason": "no_local_definition",
                    "query": query,
                    "message": search_drift_message(&query),
                }));
            }
            // §4 result_kind: the structural channel folds a fuzzy
            // (Levenshtein) fallback into `trace_structural` with
            // `MatchType::Fuzzy`. That fallback is all-or-nothing per query
            // (see `symbol_fst::search_with_fallback`), and the pre-fusion
            // trace keeps its tag (fusion only relabels the merged `results`),
            // so any Fuzzy row here means the whole set is a typo-corrected
            // near-miss — never a `"def"`.
            let structural_fuzzy = trace_structural
                .iter()
                .any(|r| matches!(r.match_type, crate::search::MatchType::Fuzzy));
            output::print_search_envelope(&results, &signals, meta, structural_fuzzy);
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

/// Run the full hybrid search pipeline against one repo and return its
/// results (+ trace material). `want_prefusion` controls whether the
/// per-channel snapshots needed for `--why` / JSON `signals` are captured.
/// `emit_diagnostics` gates the advisory stderr notices (drift hint,
/// missing-embeddings / embedder warnings); the workspace fanout passes
/// `false` so they don't repeat once per member.
fn produce_results(
    root: &Path,
    cfg: &VexConfig,
    local_cache_active: bool,
    req: &SearchReq<'_>,
    want_prefusion: bool,
    emit_diagnostics: bool,
) -> Result<SearchOutcome> {
    let semantic = req.semantic;
    let path_scope = scope::PathScope::from_args(&req.scope.include, &req.scope.exclude)?;
    let metadata_filter = build_metadata_filter(req.meta)?;
    // Resolve the diff scope per repo (a `--since*` window is relative to
    // each repo's own git history).
    let changed_paths = resolve_diff_filter(root, req.diff)?;
    let index_path = ensure_index_ready(
        root,
        req.auto_update,
        req.no_stale_check,
        semantic,
        local_cache_active,
        cfg,
    )?;

    let reader = IndexReader::open(&index_path).context("open index")?;

    // Local aliases so the (unchanged) pipeline body below reads the same
    // as before the extraction.
    let query = req.query;
    let limit = req.limit;
    let no_bm25 = req.no_bm25;
    let code_only = req.code_only;
    let exclude_generated = req.exclude_generated;
    let filter_path = req.filter_path;
    let kind = req.kind;
    let context_path = req.context_path;

    // Over-fetch when a path filter is active — the post-filter
    // `take(limit)` runs AFTER the results are produced, so a narrow
    // include/exclude or substring would silently truncate matches.
    // Bound by `symbol_count()`, not `usize::MAX`, because index-backed
    // results cannot exceed the symbol table. The 13.7-D3 diff filter
    // is treated identically — a `--since` window can be much narrower
    // than the include/exclude globs, so the same `symbol_count()`
    // ceiling applies.
    // v1.20.0 (D4) — `code_only` joins path_scope/diff in the over-fetch
    // predicate: on doc-heavy repos a common keyword's top-N BM25 hits can
    // be mostly `*.md` rows, and dropping them post-fetch without
    // over-fetching would silently return far fewer than `limit`
    // requested.
    let fetch_limit = if filter_path.is_some()
        || !path_scope.is_empty()
        || changed_paths.is_some()
        || code_only
    {
        reader.symbol_count()
    } else if exclude_generated {
        // `exclude_generated` also needs over-fetch — it drops rows after the
        // search — but unlike the filters above its predicate reads from disk
        // instead of answering from the index. Escalating to `symbol_count()`
        // would, on the very repo profile this flag exists for (generated code
        // outnumbering hand-written), mean opening a file head for nearly every
        // candidate. Bound it instead: over-fetch enough to absorb a heavily
        // generated result set, and accept returning fewer than `limit` on a
        // corpus that is almost entirely generated. When one of the index-only
        // filters above is also active it wins, since its over-fetch is free.
        limit
            .saturating_mul(GENERATED_OVER_FETCH)
            .min(reader.symbol_count())
    } else {
        limit
    };

    let structural_results = structural::search_with_fuzzy(&reader, query, fetch_limit);

    // v1.17 — search-drift hint. When the query is identifier-shaped
    // (a single bare symbol name, no spaces / punctuation) AND the
    // structural FST channel returned zero matches, the user almost
    // certainly meant "find the definition of X". RRF will still
    // surface BM25 + semantic neighbours (callers, imports), which is
    // correct behaviour but the typical UX failure that prompted this
    // hint: imported-from-dependency symbols look like they "didn't
    // find anything useful". Suggest the precise-lookup tools.
    //
    // The hint goes to stderr for the human path (gated by `emit_diagnostics`
    // so the workspace fanout doesn't repeat it per member). The same
    // condition also drives the `_meta.vex.dev/search_hint` envelope field
    // (set by the caller) — that path is NOT gated by `emit_diagnostics`,
    // because MCP agents / `--format json` consumers never see stderr and are
    // exactly who the hint is for (PROTOCOL-EVOLUTION §4).
    let drifted = structural_results.is_empty() && is_identifier_shaped(query);
    if emit_diagnostics && drifted {
        eprintln!("hint: {}", search_drift_message(query));
    }

    // BM25 channel: auto-on when the index has BM25 data, opt-out
    // with `--no-bm25`. Returns empty for short queries or when no
    // term hits — safe to always run.
    let bm25_results = if !no_bm25 && reader.has_bm25() {
        crate::search::bm25::search(&reader, query, fetch_limit)
    } else {
        Vec::new()
    };

    // Capture pre-fusion channel snapshots. `--why` needs them for
    // the trace; `--format json` needs them for the per-result
    // `signals` block in the response envelope (Phase 13.11). Clone
    // only when at least one consumer is active so the text/compact
    // fast path stays allocation-free. `want_prefusion` is decided by
    // the caller (false for workspace fanout — no trace there).
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

    // v1.20.0 (D4) — pin the reason the semantic channel did NOT run so
    // the envelope's `_meta.vex.dev/semantic_channel` can surface it.
    // `None` when semantic ran successfully (the channel contributed
    // results, agents can read `signals.semantic_cosine` per row).
    let semantic_channel_reason: Option<&'static str> = if !semantic {
        Some(crate::protocol::semantic_channel_reason::NOT_REQUESTED)
    } else if !reader.has_vectors() {
        Some(crate::protocol::semantic_channel_reason::INDEX_LACKS_VECTORS)
    } else {
        None
    };

    let results = if semantic && reader.has_vectors() {
        let embedder_id = resolve_embedder(None, cfg);
        // Warn (but don't fail) when the manifest doesn't record an
        // embedder — typically a pre-9.1 index or a deleted manifest.
        // We fall back to assuming DEFAULT_EMBEDDER; if the user
        // configured something else they'll see the warning and can
        // rebuild explicitly.
        let manifest = crate::index::manifest::Manifest::load(&config::manifest_path(root))?;
        if emit_diagnostics
            && manifest.embedder_id.is_none()
            && embedder_id != crate::embed::DEFAULT_EMBEDDER
        {
            eprintln!(
                "Warning: index manifest has no recorded embedder; assuming \
                 `{}`. If the index was built with `{embedder_id}` the results \
                 may be off — rebuild with `vex index --semantic --embedder {embedder_id}` \
                 to make it explicit.",
                crate::embed::DEFAULT_EMBEDDER
            );
        }
        check_embedder_match(root, &embedder_id)?;
        let mut embedder =
            crate::embed::make_embedder(&embedder_id).context("load embedding model")?;
        let hnsw_path = config::hnsw_path(root);
        let semantic_results = semantic::search_with_embedder(
            &reader,
            embedder.as_mut(),
            query,
            fetch_limit,
            &hnsw_path,
            manifest.vectors_normalized.unwrap_or(false),
        )?;
        if want_prefusion {
            trace_semantic = semantic_results.clone();
        }
        fusion::fuse3(structural_results, bm25_results, semantic_results, limit)
    } else {
        if emit_diagnostics && semantic && !reader.has_vectors() {
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
        kind_hints: crate::search::rerank::KindSelector::parse_many(kind)?,
        context_path,
    };
    let results = crate::search::rerank::rerank(query, &rerank_ctx, results);
    // Phase 13.7-D3: apply the diff filter BEFORE `take(limit)` so a
    // narrow change-set doesn't first get truncated by `--limit` and
    // then look smaller than it should.
    let pre_diff_results = apply_path_filters(results, filter_path, &path_scope);
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
    // v1.20.0 (D4) — `--code-only` strips hits in prose-format files
    // (`*.md`/`*.markdown`/`*.txt`/`*.rst`/`*.adoc`). Default off so a
    // search for "README" still finds it; agents pass this for
    // code-intent queries where CHANGELOG / README headings would
    // otherwise pollute the top of the result list.
    // `exclude_generated` reads each candidate file's header, unlike every
    // other filter here, which answers from the index alone. Two things keep
    // that affordable: the predicate sits before `take(limit)` in a lazy
    // iterator chain, so it stops being evaluated once `limit` rows have
    // passed (not once `fetch_limit` rows have been scanned), and the
    // per-path verdict is memoised because a symbol-dense file contributes
    // many rows.
    let mut generated_memo: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    let mut generated_dropped = 0usize;
    let results: Vec<_> = post_diff_results
        .into_iter()
        .filter(|r| metadata_filter.matches(r.signature.as_deref()))
        .filter(|r| !code_only || !crate::util::paths::is_doc_path(&r.path))
        .filter(|r| {
            if !exclude_generated {
                return true;
            }
            let is_generated = *generated_memo
                .entry(r.path.clone())
                .or_insert_with(|| crate::util::generated::is_generated_file(&root.join(&r.path)));
            if is_generated {
                generated_dropped += 1;
            }
            !is_generated
        })
        .take(limit)
        .collect();

    // Without this, a query whose every hit lives in generated code is
    // indistinguishable from a query that matched nothing — the caller has no
    // way to tell that a flag they passed is the reason. Counted lazily, so it
    // is a lower bound on what was suppressed.
    if results.is_empty() && generated_dropped > 0 {
        eprintln!(
            "note: {generated_dropped} result(s) suppressed by --exclude-generated; \
             re-run without it to see them"
        );
    }

    Ok(SearchOutcome {
        results,
        generated_dropped,
        trace_structural,
        trace_bm25,
        trace_semantic,
        semantic_channel_reason,
        drifted,
        diff_retained,
        diff_dropped,
        changed_paths,
    })
}

/// `vex search --workspace`: run the search across every member of the
/// nearest `.vex-workspace.toml`, grouping results by repo. Per-repo
/// ranking only (no cross-repo unified score); `--why` and per-result
/// JSON `signals` are single-repo features and are not emitted here.
fn search_workspace(ctx: &CmdCtx<'_>, req: &SearchReq<'_>) -> Result<()> {
    // Multi-repo Phase 2: per-member cache layouts come from the installed
    // resolver (the unsafe workspace-root hash-less case is rejected in
    // `cli::build_workspace_resolver`). Each member's `local_cache_active`
    // is derived from its own layout so a bootstrap (auto_update) writes the
    // `*` .gitignore into a `local_cache` member's in-tree cache.
    let start_dir = resolve_root(None)?;
    let ws = workspace::Workspace::find_and_load(&start_dir)?;
    let base = ws.base().to_path_buf();

    // One entry per member. `want_prefusion = false` — the workspace envelope
    // carries no per-result signals/trace. `stale` is captured PER MEMBER via
    // the global stale signal, which needs reset-before / take-after so a stale
    // member's reason isn't misattributed to the whole workspace.
    // `semantic_channel` needs none of that discipline: it is returned directly
    // from `produce_results` (`SearchOutcome::semantic_channel_reason`) and
    // never touches global state, so it can't bleed between members. It is the
    // per-member reason the semantic channel did not contribute — a member
    // built without `--semantic` vectors falls back to structural + BM25
    // (`index_lacks_vectors`) while a sibling runs full hybrid, and this is the
    // only place that asymmetry is surfaced (single-repo `emit_diagnostics` is
    // off in the fanout).
    crate::cli::stale_signal::reset();
    let mut per_repo: Vec<RepoOutcome> = Vec::with_capacity(ws.members.len());
    let mut any = false;
    for m in &ws.members {
        let member_cfg = config::load_config(&m.root)?;
        let outcome = produce_results(
            &m.root,
            &member_cfg,
            config::skip_hash_for(&m.root),
            req,
            false,
            false,
        )?;
        let stale = crate::cli::stale_signal::take();
        any |= !outcome.results.is_empty();
        per_repo.push(RepoOutcome {
            repo: m.display_name.clone(),
            results: outcome.results,
            stale,
            semantic_channel: outcome.semantic_channel_reason,
            drifted: outcome.drifted,
        });
    }
    if !any {
        crate::cli::exit_code::signal_no_results();
    }

    // §4 agent-output: the drift advisory is query-scoped, not member-scoped
    // (same identifier queried in every member). Surface it on the workspace
    // envelope's top-level `_meta` only when NO member found a structural
    // definition — if any member has the def, the search DID find it and a
    // "no definition" hint would mislead.
    let all_drifted = !per_repo.is_empty() && per_repo.iter().all(|r| r.drifted);

    match ctx.format {
        OutputFormat::Json => {
            let repos: Vec<_> = per_repo
                .iter()
                .map(|r| {
                    let mut obj = serde_json::json!({ "repo": r.repo, "results": r.results });
                    if let Some(reason) = &r.stale {
                        obj["stale_reason"] = serde_json::json!(reason);
                    }
                    // Surface the per-member semantic degradation reason (the
                    // workspace top-level `_meta` can't carry a per-member
                    // value). `not_requested` is suppressed: it's uniform
                    // across all members and derivable from the absent
                    // `--semantic`, so the field's *presence* means "this
                    // member degraded" — same policy as the text advisory
                    // below and the sibling `stale_reason` (absent when
                    // not applicable). This intentionally diverges from the
                    // single-repo `_meta.vex.dev/semantic_channel`, which
                    // always emits a reason; the workspace repo object is a
                    // reduced sub-schema (no per-result signals either).
                    if let Some(reason) = r.semantic_channel {
                        if reason != crate::protocol::semantic_channel_reason::NOT_REQUESTED {
                            obj["semantic_channel"] = serde_json::json!(reason);
                        }
                    }
                    obj
                })
                .collect();
            let mut meta = output::default_meta_for(&base);
            if all_drifted {
                meta.search_hint = Some(serde_json::json!({
                    "reason": "no_local_definition",
                    "query": req.query,
                    "message": search_drift_message(req.query),
                }));
            }
            output::print_envelope(
                serde_json::json!({
                    "workspace": ws.file.to_string_lossy(),
                    "repos": repos,
                }),
                crate::protocol::capabilities::current(),
                meta,
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            // Human path: emit the drift advisory once for the whole
            // workspace (query-scoped) rather than per member — mirrors the
            // single-repo stderr hint, which the fanout suppresses per member.
            if all_drifted {
                eprintln!("hint: {}", search_drift_message(req.query));
            }
            for r in &per_repo {
                let RepoOutcome {
                    repo,
                    results,
                    stale,
                    semantic_channel,
                    drifted: _,
                } = r;
                println!("── {repo} ──");
                if let Some(reason) = stale {
                    eprintln!("  (stale: {reason})");
                }
                // Only the degradation case is worth a human advisory;
                // `not_requested` is obvious from the absent `--semantic`.
                if *semantic_channel
                    == Some(crate::protocol::semantic_channel_reason::INDEX_LACKS_VECTORS)
                {
                    eprintln!(
                        "  (semantic skipped: index lacks vectors — run `vex index --semantic` in this repo)"
                    );
                }
                if results.is_empty() {
                    println!("  No results for \"{}\"", req.query);
                } else {
                    if results
                        .iter()
                        .any(|r| matches!(r.match_type, crate::search::MatchType::Fuzzy))
                    {
                        eprintln!("(fuzzy match — no exact results for \"{}\")", req.query);
                    }
                    output::print_results(results, &ctx.format);
                }
            }
        }
    }
    Ok(())
}
