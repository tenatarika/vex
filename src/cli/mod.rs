pub mod args;
pub mod cmd_bundle;
pub(crate) mod cmd_callgraph;
pub(crate) mod cmd_check;
pub(crate) mod cmd_diff;
pub(crate) mod cmd_eval;
pub(crate) mod cmd_grep;
pub(crate) mod cmd_implementations;
pub(crate) mod cmd_index;
pub(crate) mod cmd_outline;
pub(crate) mod cmd_pattern;
pub(crate) mod cmd_search;
pub(crate) mod cmd_self_update;
pub(crate) mod cmd_show;
pub(crate) mod cmd_status;
pub(crate) mod cmd_trivial;
pub(crate) mod cmd_update;
pub(crate) mod cmd_usages;
pub(crate) mod cmd_watch;
pub mod common;
pub(crate) mod index_management;
pub mod output;
pub mod scope;
pub mod show_truncate;
pub(crate) mod status_coverage;
pub mod trace;

use anyhow::{bail, Context, Result};
use args::{Cli, Commands};

use common::{
    diff_filter_meta, extract_jobs_hint, extract_path_hint, fetch_symbol_body, resolve_diff_filter,
    resolve_format, resolve_root, EXPLAIN_MAX_DIFF_LINES,
};
use index_management::ensure_index_ready;

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
        } => cmd_search::search(
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
            local_cache_active,
            &cfg,
            &format,
        ),
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
        } => cmd_usages::usages(
            name,
            limit,
            filter_path,
            auto_update,
            no_stale_check,
            strict,
            why,
            scope,
            diff,
            local_cache_active,
            &cfg,
            &format,
        ),
        Commands::Pattern {
            pattern,
            lang,
            path,
            limit,
            why,
            scope,
            diff,
        } => cmd_pattern::pattern(
            pattern, lang, path, limit, why, scope, diff, &format, excludes,
        ),
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
        } => cmd_show::show(
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
            local_cache_active,
            &cfg,
            &format,
        ),
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
        } => cmd_implementations::implementations(
            name,
            path,
            limit,
            auto_update,
            no_stale_check,
            scope,
            diff,
            &cfg,
            &format,
            excludes,
        ),
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
