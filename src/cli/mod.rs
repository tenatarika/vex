pub mod args;
pub mod cmd_bundle;
pub(crate) mod cmd_callgraph;
pub(crate) mod cmd_check;
pub(crate) mod cmd_diff;
pub(crate) mod cmd_eval;
pub(crate) mod cmd_grep;
pub(crate) mod cmd_index;
pub(crate) mod cmd_outline;
pub(crate) mod cmd_self_update;
pub(crate) mod cmd_status;
pub(crate) mod cmd_trivial;
pub(crate) mod cmd_update;
pub(crate) mod cmd_watch;
pub mod common;
pub(crate) mod index_management;
pub mod output;
pub mod scope;
pub mod show_truncate;
pub(crate) mod status_coverage;
pub mod trace;

use std::time::Instant;

use anyhow::{bail, Context, Result};
use args::{Cli, Commands, OutputFormat};

use common::{
    apply_path_filters, build_metadata_filter, check_embedder_match, diff_filter_meta,
    extract_jobs_hint, extract_path_hint, fetch_symbol_body, resolve_diff_filter, resolve_embedder,
    resolve_format, resolve_root, resolve_semantic, EXPLAIN_MAX_DIFF_LINES,
};
use index_management::{ensure_index_ready, handle_staleness};

use crate::search::{fusion, semantic, structural};
use crate::store::reader::IndexReader;
use crate::util::config;

pub fn dispatch(cli: Cli) -> Result<()> {
    // Load project config from .vex.toml — anchored to project root, not cwd
    let root_hint = extract_path_hint(&cli.command);
    let config_root = resolve_root(root_hint)?;
    let cfg = config::load_config(&config_root)?;
    let format = resolve_format(cli.format, &cfg);
    let excludes = &cfg.exclude;

    // Install the cache-root override (CLI > env > config > platform default).
    // Done once here so every config::index_path/index_dir call downstream
    // sees the resolved value without us threading it through 20+ call sites.
    let resolved_cache = config::resolve_cache_root(cli.cache_dir.as_deref(), &cfg);
    let local_cache_active = resolved_cache.skip_hash_subdir;
    config::set_cache_override(resolved_cache.root, resolved_cache.skip_hash_subdir);

    // Configure the global rayon pool before any par_iter runs.
    //   * Indexing commands (Index/Update/Watch) always init — that is
    //     where the 80% default earns its keep on long runs.
    //   * Non-indexing commands only init when the user has an explicit
    //     setting (CLI/env/config). Otherwise we leave rayon at its lazy
    //     default so a fast `vex check` / `vex search` does not spawn
    //     parked worker threads it never uses.
    let jobs_cli = extract_jobs_hint(&cli.command);
    let is_indexing_cmd = matches!(
        cli.command,
        Commands::Index { .. } | Commands::Update { .. } | Commands::Watch { .. }
    );
    if is_indexing_cmd {
        config::init_rayon_pool(config::resolve_jobs(jobs_cli, &cfg));
    } else if let Some(n) = config::resolve_explicit_jobs(jobs_cli, &cfg) {
        config::init_rayon_pool(n);
    }

    match cli.command {
        Commands::Index {
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
        } => cmd_index::index(
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
            local_cache_active,
            &cfg,
            &format,
            excludes,
        ),
        Commands::Search {
            query,
            limit,
            semantic,
            no_semantic,
            filter_path,
            kind,
            context_path,
            auto_update,
            no_stale_check,
            no_bm25,
            meta,
            why,
            scope,
            diff,
        } => {
            let semantic = resolve_semantic(semantic, no_semantic, &cfg);
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
                local_cache_active,
                &cfg,
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
            let fetch_limit =
                if filter_path.is_some() || !path_scope.is_empty() || changed_paths.is_some() {
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
            let want_prefusion = why || matches!(format, OutputFormat::Json);
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
                let embedder_id = resolve_embedder(None, &cfg);
                // Warn (but don't fail) when the manifest doesn't record an
                // embedder — typically a pre-9.1 index or a deleted manifest.
                // We fall back to assuming DEFAULT_EMBEDDER; if the user
                // configured something else they'll see the warning and can
                // rebuild explicitly.
                let manifest =
                    crate::index::manifest::Manifest::load(&config::manifest_path(&root))?;
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

            match &format {
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
                    meta.diff_filter = diff_filter_meta(
                        &diff,
                        changed_paths.as_ref(),
                        diff_retained,
                        diff_dropped,
                    );
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
                        output::print_results(&results, &format);
                    }
                }
            }
            Ok(())
        }
        Commands::Usages {
            name,
            limit,
            filter_path,
            auto_update,
            no_stale_check,
            strict,
            why,
            scope,
            diff,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(None)?.canonicalize()?;
            let changed_paths = resolve_diff_filter(&root, &diff)?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                false,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;
            let ref_reader = reader
                .ref_reader()
                .context("no refs in index — re-run `vex index` to rebuild")?;
            let file_paths = reader.file_paths();
            // Mode label is fixed up front so `--why` can record which
            // path the lookup took even when the post-filter list is
            // empty.
            // Phase 14.4: `mode` carries the new label; `mode_legacy` keeps
            // the v1.8.x value (`text_scan`) for back-compat with consumers
            // that learned the contract before the rename. Both collapse to
            // `"strict"` on the strict path. `mode_legacy` slated for removal
            // in v1.12.
            let trace_mode: &'static str = if strict { "strict" } else { "fst_lookup" };
            let trace_mode_legacy: &'static str = if strict { "strict" } else { "text_scan" };

            // `--strict` reads from the v5 reference_edges section
            // (binder-resolved refs only). The legacy FST still backs
            // the non-strict path because it captures identifiers in
            // every supported language, including the 16 without a
            // scope binder yet.
            let entries: Vec<crate::store::refs_fst::RefEntry> = if strict {
                if !reader.has_ref_edges() {
                    anyhow::bail!(
                        "--strict needs a v5 index with reference_edges (this index is v{} or has no resolved refs). Re-run `vex index` to rebuild.",
                        reader.header().version
                    );
                }
                let sym_fst = reader
                    .symbol_fst_reader()
                    .context("symbol FST missing — re-run `vex index` to rebuild for --strict")?;
                let sym_indices = sym_fst.find(&name);
                let mut out = Vec::new();
                for sym_idx in sym_indices {
                    for edge in reader.find_ref_edges_by_symbol(sym_idx) {
                        out.push(crate::store::refs_fst::RefEntry {
                            file_id: edge.from_file_id,
                            line: edge.line,
                        });
                    }
                }
                out
            } else {
                ref_reader.find(&name)
            };
            // Capture the un-filtered hit count up front for `--why`.
            // Doing it here (rather than after the chain) keeps the
            // filter-loss visible in the trace even when `total` ends
            // at zero — the user can tell "no refs at all" from "refs
            // dropped by the filter".
            let hits_before_filter = entries.len();
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|e| {
                    let path = match file_paths.get(e.file_id as usize) {
                        Some(p) => p.as_str(),
                        None => return false,
                    };
                    let filter_ok = filter_path.as_deref().is_none_or(|fp| path.contains(fp));
                    let scope_ok = path_scope.accept(path);
                    // Phase 13.7-D3: apply diff filter alongside path filters
                    // so the trace's `total` reflects the post-diff count
                    // exactly like it already reflects the post-scope count.
                    let diff_ok = changed_paths.as_ref().is_none_or(|cp| cp.contains(path));
                    filter_ok && scope_ok && diff_ok
                })
                .collect();
            let total = entries.len();
            let diff_retained = total;
            let diff_dropped = hits_before_filter.saturating_sub(total);
            let entries: Vec<_> = entries.into_iter().take(limit).collect();

            // Prefix-suggestion fallback runs ONLY when no exact hits
            // and only against the FST-lookup path (strict-mode doesn't
            // have a prefix counterpart today). We resolve it once
            // here so both the Text print and the `--why` trace use
            // the same vector — without double-querying the FST.
            let prefix_suggestions = if entries.is_empty() && !strict {
                Some(ref_reader.find_by_prefix(&name))
            } else {
                None
            };

            match &format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|e| {
                            let path = file_paths
                                .get(e.file_id as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("?");
                            serde_json::json!({
                                "path": path,
                                "line": e.line,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if entries.is_empty() {
                        println!("No usages found for \"{name}\"");

                        if let Some(prefix_results) = prefix_suggestions.as_deref() {
                            if !prefix_results.is_empty() {
                                println!("\nDid you mean:");
                                for (n, refs) in prefix_results.iter().take(5) {
                                    println!("  {n} ({} usages)", refs.len());
                                }
                            }
                        }
                    } else {
                        println!("{name}: {total} usages (showing {})", entries.len());
                        for e in &entries {
                            let path = file_paths
                                .get(e.file_id as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("?");
                            println!("  {path}:{}", e.line);
                        }
                    }
                }
            }

            // 11.10: structured trace on stderr for `--why`. Captured
            // post-print so stdout stays a pure result list.
            if why {
                let trace = crate::cli::trace::UsagesTrace {
                    mode: trace_mode,
                    mode_legacy: trace_mode_legacy,
                    hits_before_filter,
                    hits_after_filter: total,
                    prefix_suggestions: prefix_suggestions.as_ref().map(|v| v.len()),
                    filter_applied: crate::cli::trace::FilterSnapshot {
                        filter: filter_path.clone(),
                        include: scope.include.clone(),
                        exclude: scope.exclude.clone(),
                    },
                };
                crate::cli::trace::emit_why_trace(&trace)?;
                if let Some(df) =
                    diff_filter_meta(&diff, changed_paths.as_ref(), diff_retained, diff_dropped)
                {
                    crate::cli::trace::emit_diff_filter(&df)?;
                }
            }

            Ok(())
        }
        Commands::Pattern {
            pattern,
            lang,
            path,
            limit,
            why,
            scope,
            diff,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?;
            // Resolve diff filter against the project root. Pattern uses a
            // non-canonicalized root; that's fine for git, which accepts any
            // dir inside the work tree.
            let changed_paths = resolve_diff_filter(&root, &diff)?;
            let language = crate::parse::language::Language::from_extension(&lang)
                .or(match lang.as_str() {
                    "rust" => Some(crate::parse::language::Language::Rust),
                    "python" => Some(crate::parse::language::Language::Python),
                    "go" => Some(crate::parse::language::Language::Go),
                    "java" => Some(crate::parse::language::Language::Java),
                    "csharp" | "cs" => Some(crate::parse::language::Language::CSharp),
                    "ruby" | "rb" => Some(crate::parse::language::Language::Ruby),
                    "swift" => Some(crate::parse::language::Language::Swift),
                    "kotlin" | "kt" => Some(crate::parse::language::Language::Kotlin),
                    "typescript" | "ts" | "tsx" => {
                        Some(crate::parse::language::Language::TypeScript)
                    }
                    "sql" => Some(crate::parse::language::Language::Sql),
                    "markdown" | "md" => Some(crate::parse::language::Language::Markdown),
                    "cpp" | "c++" | "cxx" | "c" => Some(crate::parse::language::Language::Cpp),
                    "php" | "phtml" => Some(crate::parse::language::Language::Php),
                    "bash" | "sh" | "shell" => Some(crate::parse::language::Language::Bash),
                    "lua" => Some(crate::parse::language::Language::Lua),
                    "css" => Some(crate::parse::language::Language::Css),
                    "html" | "htm" => Some(crate::parse::language::Language::Html),
                    "yaml" | "yml" => Some(crate::parse::language::Language::Yaml),
                    "toml" => Some(crate::parse::language::Language::Toml),
                    _ => None,
                })
                .with_context(|| format!("unknown language: {lang}"))?;

            let start = Instant::now();
            // Over-fetch when scope filters are active so post-filter truncation
            // does not silently drop matches the user expects to see. Diff
            // filter is treated identically — see Search handler note.
            let fetch_limit = if path_scope.is_empty() && changed_paths.is_none() {
                limit
            } else {
                usize::MAX
            };

            let (raw_matches, trace) =
                crate::pattern::scan_with_mode(&root, &pattern, language, fetch_limit, excludes)?;

            // Apply scope first, then diff filter. Track counts for the
            // `--why` diff_filter trace.
            let pre_diff: Vec<_> = raw_matches
                .into_iter()
                .filter(|m| path_scope.accept(&m.path))
                .collect();
            let pre_diff_count = pre_diff.len();
            let post_diff: Vec<_> = if let Some(ref cp) = changed_paths {
                pre_diff
                    .into_iter()
                    .filter(|m| cp.contains(&m.path))
                    .collect()
            } else {
                pre_diff
            };
            let diff_retained = post_diff.len();
            let diff_dropped = pre_diff_count.saturating_sub(diff_retained);
            let matches: Vec<_> = post_diff.into_iter().take(limit).collect();
            let elapsed = start.elapsed();

            match &format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = matches
                        .iter()
                        .map(|m| {
                            let mut obj = serde_json::json!({
                                "path": m.path,
                                "line": m.line,
                                "text": m.matched_text.lines().next().unwrap_or(""),
                            });
                            if !m.captures.is_empty() {
                                obj["captures"] = serde_json::json!(m
                                    .captures
                                    .iter()
                                    .map(|(k, v)| serde_json::json!({k: v}))
                                    .collect::<Vec<_>>());
                            }
                            obj
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if matches.is_empty() {
                        println!("No matches for pattern in {elapsed:.2?}");
                    } else {
                        println!("{} matches in {elapsed:.2?}\n", matches.len());
                        for m in &matches {
                            let first_line = m.matched_text.lines().next().unwrap_or("");
                            println!("{}:{}", m.path, m.line);
                            println!("  {first_line}");
                            for (name, value) in &m.captures {
                                println!("  ${name} = {value}");
                            }
                            println!();
                        }
                    }
                }
            }

            if why {
                // stderr keeps stdout a pure result stream — mirrors
                // `vex search --why` so `vex pattern 'pat' --why | jq` works.
                crate::cli::trace::emit_why_trace(&trace)?;
                if let Some(df) =
                    diff_filter_meta(&diff, changed_paths.as_ref(), diff_retained, diff_dropped)
                {
                    crate::cli::trace::emit_diff_filter(&df)?;
                }
            }

            Ok(())
        }
        Commands::Update {
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
        } => cmd_update::update(
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
            &cfg,
            &format,
            excludes,
        ),
        Commands::Outline { file, kind } => {
            cmd_outline::cmd_outline(&file, kind.as_deref(), &format)
        }
        Commands::Watch {
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
        } => cmd_watch::watch(
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
            &cfg,
            excludes,
        ),
        Commands::Show {
            symbols,
            limit,
            context,
            filter_path,
            kind,
            context_path,
            auto_update,
            no_stale_check,
            signature_only,
            head,
            no_body,
            collapsed,
            meta,
            scope,
        } => {
            // Phase 13.3 — resolve the truncation mode once. Clap's
            // `conflicts_with_all` already guarantees at most one flag
            // is set; this just maps the booleans into an `Option`.
            let truncation_mode: Option<show_truncate::TruncationMode> = if signature_only {
                Some(show_truncate::TruncationMode::SignatureOnly)
            } else if head.is_some() {
                Some(show_truncate::TruncationMode::Head)
            } else if no_body {
                Some(show_truncate::TruncationMode::NoBody)
            } else if collapsed {
                Some(show_truncate::TruncationMode::Collapsed)
            } else {
                None
            };
            if collapsed {
                // Single emission via stderr — tracing isn't always
                // initialized (e.g. under the CLI integration tests),
                // and emitting twice would risk drift if a test asserts
                // on exact-string output. The integration test pins the
                // `pending` substring on stderr, so this stays
                // observable for both human and automated callers.
                eprintln!(
                    "warning: --collapsed pending language-aware implementation; emitting full body"
                );
            }
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let metadata_filter = build_metadata_filter(&meta)?;
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                false,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;
            let fetch_limit = if filter_path.is_some() || !path_scope.is_empty() {
                reader.symbol_count()
            } else {
                limit
            };
            let mut json_items: Vec<serde_json::Value> = Vec::new();
            let mut printed = 0usize;

            let rerank_ctx = crate::search::rerank::RerankContext {
                kind_hints: crate::search::rerank::KindSelector::parse_many(&kind)?,
                context_path: context_path.as_deref(),
            };

            for symbol in &symbols {
                let results = structural::search_with_fuzzy(&reader, symbol, fetch_limit);
                let results = crate::search::rerank::rerank(symbol, &rerank_ctx, results);
                let results: Vec<_> =
                    apply_path_filters(results, filter_path.as_deref(), &path_scope)
                        .into_iter()
                        .filter(|r| metadata_filter.matches(r.signature.as_deref()))
                        .take(limit)
                        .collect();

                if results.is_empty() {
                    match &format {
                        OutputFormat::Json => {}
                        OutputFormat::Text | OutputFormat::Compact => {
                            if printed > 0 {
                                println!();
                            }
                            println!("No symbol found: \"{symbol}\"");
                            printed += 1;
                        }
                    }
                    continue;
                }

                for result in &results {
                    let content = std::fs::read_to_string(&result.path)
                        .with_context(|| format!("read {}", result.path))?;

                    let ext = std::path::Path::new(&result.path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("");

                    let body = if result.kind == "heading" {
                        crate::parse::body::extract_heading_body(&content, result.line, context)?
                    } else if let Some(lang) = crate::parse::language::Language::from_extension(ext)
                    {
                        crate::parse::body::extract_symbol_body_ts(
                            &content,
                            result.line,
                            lang,
                            context,
                        )?
                    } else {
                        crate::parse::body::extract_symbol_body(&content, result.line, context)?
                    };

                    // Phase 13.3 — apply optional truncation to the
                    // extracted body. The struct returned by the
                    // helpers carries metadata that we surface in the
                    // JSON envelope per result; text/compact output
                    // stays clean (just the truncated body).
                    let truncation = truncation_mode.map(|mode| match mode {
                        show_truncate::TruncationMode::SignatureOnly => {
                            show_truncate::signature_only(&body.body)
                        }
                        show_truncate::TruncationMode::Head => {
                            // `head` Option already validated as Some
                            // when mode is Head.
                            let n = head.unwrap_or(usize::MAX);
                            show_truncate::head_n(&body.body, n)
                        }
                        show_truncate::TruncationMode::NoBody => show_truncate::no_body(&body.body),
                        show_truncate::TruncationMode::Collapsed => {
                            show_truncate::collapsed(&body.body)
                        }
                    });
                    let display_body: &str = truncation
                        .as_ref()
                        .map(|t| t.body.as_str())
                        .unwrap_or(body.body.as_str());

                    match &format {
                        OutputFormat::Json => {
                            let mut item = serde_json::json!({
                                "name": result.name,
                                "kind": result.kind,
                                "path": result.path,
                                "start_line": body.start_line,
                                "end_line": body.end_line,
                                "lines": body.lines,
                                "body": display_body,
                            });
                            if let Some(t) = &truncation {
                                item["truncation"] = serde_json::json!({
                                    "mode": t.mode.as_str(),
                                    "original_lines": t.original_lines,
                                    "kept_lines": t.kept_lines,
                                });
                            }
                            json_items.push(item);
                        }
                        OutputFormat::Text => {
                            if printed > 0 {
                                println!();
                            }
                            println!(
                                "── {} ({}) {}:{}-{}",
                                result.name,
                                result.kind,
                                result.path,
                                body.start_line,
                                body.end_line
                            );
                            for (n, line) in display_body.lines().enumerate() {
                                println!("{:>4} | {}", body.start_line + n, line);
                            }
                            printed += 1;
                        }
                        OutputFormat::Compact => {
                            if printed > 0 {
                                println!();
                            }
                            println!(
                                "# {}:{}-{} ({})",
                                result.path, body.start_line, body.end_line, result.kind
                            );
                            println!("{}", display_body);
                            printed += 1;
                        }
                    }
                }
            }

            match &format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&json_items)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if printed == 0 {
                        println!("No symbols found");
                    }
                }
            }
            Ok(())
        }
        Commands::Status { path, coverage } => {
            cmd_status::status(path, coverage, &format, excludes)
        }
        Commands::Grep {
            pattern,
            limit,
            filter_path,
            path,
            scope,
            diff,
        } => cmd_grep::grep(
            pattern,
            limit,
            filter_path,
            path,
            scope,
            diff,
            &format,
            excludes,
        ),
        Commands::Implementations {
            name,
            path,
            limit,
            auto_update,
            no_stale_check,
            scope,
            diff,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?;
            handle_staleness(&root, auto_update, no_stale_check, &cfg)?;
            let changed_paths = resolve_diff_filter(&root, &diff)?;
            let start = Instant::now();
            let fetch_limit = if path_scope.is_empty() && changed_paths.is_none() {
                limit
            } else {
                usize::MAX
            };
            let matches =
                crate::hierarchy::find_implementations(&root, &name, fetch_limit, excludes)?;
            let matches: Vec<_> = matches
                .into_iter()
                .filter(|m| {
                    path_scope.accept(&m.path)
                        && changed_paths.as_ref().is_none_or(|cp| cp.contains(&m.path))
                })
                .take(limit)
                .collect();
            let elapsed = start.elapsed();

            match &format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = matches
                        .iter()
                        .map(|m| {
                            serde_json::json!({
                                "name": m.name,
                                "base": m.base,
                                "relation": m.relation,
                                "path": m.path,
                                "line": m.line,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text => {
                    if matches.is_empty() {
                        println!("No implementations of \"{name}\" in {elapsed:.2?}");
                    } else {
                        println!(
                            "{name}: {} implementations in {elapsed:.2?}\n",
                            matches.len()
                        );
                        for m in &matches {
                            println!("  {:<40} ({})  {}:{}", m.name, m.relation, m.path, m.line);
                        }
                    }
                }
                OutputFormat::Compact => {
                    for m in &matches {
                        println!("{} {} {} {}:{}", m.relation, m.base, m.name, m.path, m.line);
                    }
                }
            }
            Ok(())
        }
        Commands::Callers {
            name,
            path,
            limit,
            auto_update,
            no_stale_check,
            scope,
            diff,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            cmd_callgraph::cmd_callgraph(
                &name,
                path,
                limit,
                true,
                auto_update,
                no_stale_check,
                local_cache_active,
                &cfg,
                &format,
                excludes,
                &path_scope,
                &diff,
            )
        }
        Commands::Callees {
            name,
            path,
            limit,
            auto_update,
            no_stale_check,
            scope,
            diff,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            cmd_callgraph::cmd_callgraph(
                &name,
                path,
                limit,
                false,
                auto_update,
                no_stale_check,
                local_cache_active,
                &cfg,
                &format,
                excludes,
                &path_scope,
                &diff,
            )
        }
        Commands::Diff {
            base,
            path,
            limit,
            scope,
        } => cmd_diff::diff(base, path, limit, scope, &format, excludes),
        Commands::Paths {
            from,
            to,
            max_hops,
            max_paths,
            path,
            auto_update,
            no_stale_check,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                false,
                local_cache_active,
                &cfg,
            )?;
            let reader = IndexReader::open(&index_path).context("open index")?;
            if !reader.has_call_graph() {
                bail!(
                    "no call graph in index — `vex paths` requires a v4 index built without \
                     `--no-call-graph`. Rebuild with `vex index` (or `vex index --auto-update`)."
                );
            }
            // Closure binds the reader so the BFS layer stays generic
            // over an in-memory mock for unit tests. The per-step fetch
            // cap (1024) is well above any realistic codebase fan-in;
            // when we see a node that hits it, we surface a warning so
            // a real saturation event is visible instead of silently
            // dropping callers.
            use crate::callgraph::CALLERS_FETCH_CAP;
            let callers_of = |name: &str| {
                let callers =
                    crate::store::call_graph::find_callers_fast(&reader, name, CALLERS_FETCH_CAP);
                if callers.len() == CALLERS_FETCH_CAP {
                    eprintln!(
                        "warning: `{name}` has at least {CALLERS_FETCH_CAP} direct callers; \
                         multi-hop traversal may have dropped some — results below this node \
                         are incomplete"
                    );
                }
                callers
            };
            let paths =
                crate::callgraph::bfs::find_paths(callers_of, &from, &to, max_hops, max_paths);
            let paths: Vec<_> = paths
                .into_iter()
                .filter(|p| {
                    // Apply scope filter on the *intermediate* steps — if
                    // every intermediate path is excluded, drop the chain.
                    // `from`/`to` themselves have line=0 / no resolved
                    // path, so they're exempt from the include rule.
                    p.steps
                        .iter()
                        .filter(|s| !s.path.is_empty())
                        .all(|s| path_scope.accept(&s.path))
                })
                .collect();
            output::print_paths(&paths, &from, &to, &format);
            Ok(())
        }
        Commands::Reachable {
            target,
            max_hops,
            limit,
            path,
            auto_update,
            no_stale_check,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                false,
                local_cache_active,
                &cfg,
            )?;
            let reader = IndexReader::open(&index_path).context("open index")?;
            if !reader.has_call_graph() {
                bail!(
                    "no call graph in index — `vex reachable` requires a v4 index built without \
                     `--no-call-graph`. Rebuild with `vex index`."
                );
            }
            // When a scope filter is active a 4x over-fetch is not
            // enough — a narrow include could legitimately reject most
            // of the BFS frontier and leave the final list short. The
            // traversal is already bounded by `max_hops`, so we let it
            // run unbounded internally and apply `take(limit)` after
            // the filter.
            let fetch_limit = if path_scope.is_empty() {
                limit
            } else {
                usize::MAX
            };
            const CALLERS_FETCH_CAP: usize = 1024;
            let callers_of = |name: &str| {
                let callers =
                    crate::store::call_graph::find_callers_fast(&reader, name, CALLERS_FETCH_CAP);
                if callers.len() == CALLERS_FETCH_CAP {
                    eprintln!(
                        "warning: `{name}` has at least {CALLERS_FETCH_CAP} direct callers; \
                         reachable set may be incomplete past this node"
                    );
                }
                callers
            };
            let matches =
                crate::callgraph::bfs::find_reachable(callers_of, &target, max_hops, fetch_limit);
            let matches: Vec<_> = matches
                .into_iter()
                .filter(|m| path_scope.accept(&m.path))
                .take(limit)
                .collect();
            output::print_reachable(&matches, &target, &format);
            Ok(())
        }
        Commands::Check {
            names,
            path,
            auto_update,
            no_stale_check,
        } => cmd_check::check(
            names,
            path,
            auto_update,
            no_stale_check,
            local_cache_active,
            &cfg,
            &format,
        ),

        Commands::Similar {
            name,
            path,
            limit,
            threshold,
            filter_path,
            explain,
            auto_update,
            no_stale_check,
            why,
            scope,
            diff,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?.canonicalize()?;
            let changed_paths = resolve_diff_filter(&root, &diff)?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                true,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;
            if !reader.has_vectors() {
                bail!("No embeddings in index. Run `vex index --semantic` first.");
            }

            let hnsw = config::hnsw_path(&root);
            // Over-fetch when a path filter is active so `take(limit)`
            // after the filter doesn't truncate prematurely. ALSO
            // over-fetch when `--why` is on so `candidates_before_filter`
            // reports the un-truncated HNSW return list — without this,
            // `find_similar`'s internal `truncate(limit)` would cap the
            // count and the trace would silently misreport
            // "nothing dropped" when in fact `--limit` ate results.
            let fetch_limit = if filter_path.is_some()
                || !path_scope.is_empty()
                || changed_paths.is_some()
                || why
            {
                reader.symbol_count()
            } else {
                limit
            };
            let matches = crate::search::similar::find_similar(
                &reader,
                &hnsw,
                &name,
                fetch_limit,
                threshold,
            )?;
            // `find_similar` returns an empty Vec when the seed name
            // doesn't resolve to a stored vector — `resolve_seed_match`
            // here distinguishes "no match for seed" from "seed found
            // but post-threshold filter dropped everything". Cheap
            // (single FST lookup) and runs only when --why is on; the
            // explicit `if/else` makes the gating intent obvious vs. a
            // short-circuit `!why || ...` which inverts the readable
            // meaning of `seed_resolved`.
            let seed_resolved = if why {
                crate::search::similar::resolve_seed_match(&reader, &name).is_some()
            } else {
                false
            };
            let candidates_before_filter = matches.len();
            let filtered: Vec<_> = matches
                .into_iter()
                .filter(|m| {
                    let filter_ok = filter_path.as_deref().is_none_or(|fp| m.path.contains(fp));
                    let diff_ok = changed_paths.as_ref().is_none_or(|cp| cp.contains(&m.path));
                    filter_ok && path_scope.accept(&m.path) && diff_ok
                })
                .collect();
            // Capture post-filter count BEFORE the `take(limit)`
            // truncation so the trace can distinguish "filter dropped
            // N" from "--limit truncated N" — mirrors `usages.total`.
            let candidates_after_filter = filtered.len();
            let matches: Vec<_> = filtered.into_iter().take(limit).collect();

            // Build per-result explanations on demand. The seed body is
            // resolved once via `similar::resolve_seed_match`, which uses
            // the same symbol-FST lookup `find_similar` already ran —
            // sharing the entry point keeps both paths in sync.
            let explanations: Option<Vec<_>> = if explain && !matches.is_empty() {
                let seed_body = match crate::search::similar::resolve_seed_match(&reader, &name) {
                    Some(s) => fetch_symbol_body(&s.path, s.line, &s.kind),
                    None => {
                        // Should not happen — `find_similar` already
                        // resolved the seed seconds ago. If it does
                        // (e.g. index mutated between calls), surface
                        // it so the empty `jaccard 0.00 +N -0` rows
                        // aren't misread as a real result.
                        eprintln!(
                            "warning: --explain could not resolve seed symbol `{name}` \
                             after find_similar succeeded; reasoning will be incomplete"
                        );
                        String::new()
                    }
                };
                Some(
                    matches
                        .iter()
                        .map(|m| {
                            let body = fetch_symbol_body(&m.path, m.line, &m.kind);
                            crate::search::explain::explain_pair(
                                &seed_body,
                                &body,
                                EXPLAIN_MAX_DIFF_LINES,
                            )
                        })
                        .collect(),
                )
            } else {
                None
            };

            output::print_similar(&matches, &name, explanations.as_deref(), &format);

            // 11.10: structured trace on stderr for `--why`.
            // `candidates_after_filter` is the pre-`--limit` count so
            // it composes with `candidates_before_filter` to surface
            // filter-drop separately from `--limit` truncation.
            if why {
                let trace = crate::cli::trace::SimilarTrace {
                    seed_resolved,
                    threshold_applied: threshold,
                    candidates_before_filter,
                    candidates_after_filter,
                    filter_applied: crate::cli::trace::FilterSnapshot {
                        filter: filter_path.clone(),
                        include: scope.include.clone(),
                        exclude: scope.exclude.clone(),
                    },
                };
                crate::cli::trace::emit_why_trace(&trace)?;
                let diff_retained = candidates_after_filter;
                let diff_dropped = candidates_before_filter.saturating_sub(candidates_after_filter);
                if let Some(df) =
                    diff_filter_meta(&diff, changed_paths.as_ref(), diff_retained, diff_dropped)
                {
                    crate::cli::trace::emit_diff_filter(&df)?;
                }
            }

            Ok(())
        }
        Commands::Duplicates {
            path,
            threshold,
            limit,
            min_body_lines,
            filter_path,
            explain,
            auto_update,
            no_stale_check,
            why,
            scope,
            diff,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?.canonicalize()?;
            let changed_paths = resolve_diff_filter(&root, &diff)?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                true,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;
            if !reader.has_vectors() {
                bail!("No embeddings in index. Run `vex index --semantic` first.");
            }

            let hnsw = config::hnsw_path(&root);
            // When --filter or --include/--exclude are active we must
            // over-fetch because the final `take(limit)` runs AFTER the
            // path filter; a narrow filter can drop most pairs. Mirrors
            // `vex similar --filter` (uses `symbol_count()`) but here the
            // upper bound is the pair population, so usize::MAX is the
            // right cap. Also over-fetch when `--why` is on so
            // `pairs_before_filter` reports the un-truncated scanner
            // output — without this, `find_duplicates`' internal
            // `truncate(limit)` would silently misreport
            // "nothing dropped" when `--limit` ate results.
            let fetch_limit = if filter_path.is_some()
                || !path_scope.is_empty()
                || changed_paths.is_some()
                || why
            {
                usize::MAX
            } else {
                limit
            };
            let pairs = crate::search::similar::find_duplicates(
                &reader,
                &hnsw,
                threshold,
                min_body_lines,
                fetch_limit,
            )?;
            let pairs_before_filter = pairs.len();
            let filtered_pairs: Vec<_> = pairs
                .into_iter()
                .filter(|(a, b)| {
                    let filter_ok = filter_path
                        .as_deref()
                        .is_none_or(|fp| a.path.contains(fp) || b.path.contains(fp));
                    // Pair semantics for the diff filter: keep the pair when
                    // EITHER symbol's file is in the change set — mirrors the
                    // existing `accept_pair` mode and matches how a reviewer
                    // thinks ("did this PR introduce a near-duplicate of
                    // anything?").
                    let diff_ok = changed_paths
                        .as_ref()
                        .is_none_or(|cp| cp.contains(&a.path) || cp.contains(&b.path));
                    filter_ok && path_scope.accept_pair(&a.path, &b.path) && diff_ok
                })
                .collect();
            // Pre-`--limit` count for symmetric trace reporting (see
            // `similar` handler's `candidates_after_filter` comment).
            let pairs_after_filter = filtered_pairs.len();
            let pairs: Vec<_> = filtered_pairs.into_iter().take(limit).collect();

            let explanations: Option<Vec<_>> = if explain && !pairs.is_empty() {
                Some(
                    pairs
                        .iter()
                        .map(|(a, b)| {
                            let a_body = fetch_symbol_body(&a.path, a.line, &a.kind);
                            let b_body = fetch_symbol_body(&b.path, b.line, &b.kind);
                            crate::search::explain::explain_pair(
                                &a_body,
                                &b_body,
                                EXPLAIN_MAX_DIFF_LINES,
                            )
                        })
                        .collect(),
                )
            } else {
                None
            };

            output::print_duplicates(&pairs, explanations.as_deref(), &format);

            // 11.10: structured trace on stderr for `--why`.
            // `pairs_after_filter` is the pre-`--limit` count for
            // symmetry with `pairs_before_filter`.
            if why {
                let trace = crate::cli::trace::DuplicatesTrace {
                    threshold_applied: threshold,
                    min_body_lines_applied: min_body_lines,
                    pairs_before_filter,
                    pairs_after_filter,
                    filter_applied: crate::cli::trace::FilterSnapshot {
                        filter: filter_path.clone(),
                        include: scope.include.clone(),
                        exclude: scope.exclude.clone(),
                    },
                };
                crate::cli::trace::emit_why_trace(&trace)?;
                let diff_retained = pairs_after_filter;
                let diff_dropped = pairs_before_filter.saturating_sub(pairs_after_filter);
                if let Some(df) =
                    diff_filter_meta(&diff, changed_paths.as_ref(), diff_retained, diff_dropped)
                {
                    crate::cli::trace::emit_diff_filter(&df)?;
                }
            }

            Ok(())
        }

        Commands::Completions { shell } => cmd_trivial::completions(shell),
        Commands::Init => cmd_trivial::init(),
        Commands::Capabilities => cmd_trivial::capabilities(),

        Commands::Eval {
            bench,
            min_ndcg,
            path,
            json,
        } => cmd_eval::cmd_eval(
            path,
            bench,
            min_ndcg,
            json,
            local_cache_active,
            &cfg,
            &format,
        ),

        Commands::Bundle {
            mode,
            symbol,
            base,
            depth,
            path_glob,
            top_n,
            directory_tree_only,
            directory_tree_top,
            callers_max,
            callees_max,
            similar_max,
            tests_max,
            path,
            auto_update,
            no_stale_check,
            scope,
        } => {
            // Inc 2 — open the index for `--mode symbol`. We pass
            // `needs_semantic=false` to `ensure_index_ready`: similar
            // results are best-effort (degrade to empty when the index
            // has no vectors), not a hard requirement. Other modes plug
            // into the same plumbing in Inc 3 / Inc 4.
            let root = cmd_bundle::resolve_bundle_root(path)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                /*needs_semantic=*/ false,
                local_cache_active,
                &cfg,
            )?;
            let reader = IndexReader::open(&index_path).context("open index")?;
            let hnsw_path = config::hnsw_path(&root);
            let args = cmd_bundle::BundleArgs {
                mode,
                symbol: symbol.as_deref(),
                base: base.as_deref(),
                depth,
                path_glob: path_glob.as_deref(),
                top_n,
                callers_max,
                callees_max,
                similar_max,
                tests_max,
                directory_tree_only,
                directory_tree_top,
            };
            let ctx = cmd_bundle::BundleCtx {
                root,
                scope: &scope,
                reader: &reader,
                hnsw_path,
                excludes,
            };
            cmd_bundle::cmd_bundle(args, ctx)
        }

        Commands::SelfUpdate { check, yes } => {
            // Named at the call site so a future refactor cannot silently
            // swap the two boolean positional args.
            cmd_self_update::cmd_self_update(/*check_only=*/ check, /*no_confirm=*/ yes)
        }
    }
}
