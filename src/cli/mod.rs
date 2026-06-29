pub mod args;
pub mod cmd_bundle;
pub(crate) mod cmd_callgraph;
pub(crate) mod cmd_check;
pub(crate) mod cmd_diff;
pub(crate) mod cmd_duplicates;
pub(crate) mod cmd_eval;
pub(crate) mod cmd_gpu;
pub(crate) mod cmd_grep;
pub(crate) mod cmd_history;
pub(crate) mod cmd_impact;
pub(crate) mod cmd_implementations;
pub(crate) mod cmd_index;
pub(crate) mod cmd_mcp;
pub(crate) mod cmd_outline;
pub(crate) mod cmd_pattern;
pub(crate) mod cmd_search;
pub(crate) mod cmd_self_update;
pub(crate) mod cmd_show;
pub(crate) mod cmd_similar;
pub(crate) mod cmd_status;
pub(crate) mod cmd_tests_for;
pub(crate) mod cmd_trivial;
pub(crate) mod cmd_update;
pub(crate) mod cmd_usages;
pub(crate) mod cmd_watch;
pub mod common;
pub(crate) mod exit_code;
pub(crate) mod index_management;
pub mod output;
pub mod scope;
pub(crate) mod self_update_flow;
pub mod show_truncate;
pub(crate) mod stale_signal;
pub(crate) mod status_coverage;
pub mod trace;

use anyhow::Result;
use args::{Cli, Commands};

use common::{extract_jobs_hint, extract_path_hint, resolve_format, resolve_root, CmdCtx};

use crate::util::config;

pub fn dispatch(cli: Cli) -> Result<std::process::ExitCode> {
    exit_code::finish(dispatch_inner(cli))
}

fn dispatch_inner(cli: Cli) -> Result<()> {
    // Load project config from .vex.toml — anchored to project root, not cwd
    let root_hint = extract_path_hint(&cli.command);
    let config_root = resolve_root(root_hint)?;
    let cfg = config::load_config(&config_root)?;
    let format = resolve_format(cli.format, &cfg);

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

    // Threaded into every extracted cmd_* handler so the per-arm
    // signatures stay short and the dispatch table reads as
    // destructure-and-delegate. Architect MUST-FIX #3 from the S1 review.
    let ctx = CmdCtx {
        cfg: &cfg,
        format,
        excludes: &cfg.exclude,
        local_cache_active,
    };

    match cli.command {
        Commands::Index {
            path,
            semantic,
            no_semantic,
            drop_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
            no_wait,
            history,
            history_depth,
            gpu,
            no_gpu,
            device,
            workspace,
        } => cmd_index::index(
            &ctx,
            path,
            semantic,
            no_semantic,
            drop_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
            no_wait,
            history,
            history_depth,
            gpu,
            no_gpu,
            device,
            workspace,
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
            code_only,
            meta,
            why,
            scope,
            diff,
            workspace,
        } => cmd_search::search(
            &ctx,
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
            code_only,
            meta,
            why,
            scope,
            diff,
            workspace,
        ),
        Commands::Impact {
            name,
            path,
            auto_update,
            no_stale_check,
            exclude_docs,
            depth,
            scope,
            workspace,
        } => cmd_impact::impact(
            &ctx,
            name,
            path,
            auto_update,
            no_stale_check,
            exclude_docs,
            depth,
            scope,
            workspace,
        ),
        Commands::Usages {
            name,
            limit,
            filter_path,
            auto_update,
            no_stale_check,
            strict,
            why,
            include_self,
            include_docs,
            scope,
            diff,
            workspace,
        } => cmd_usages::usages(
            &ctx,
            name,
            limit,
            filter_path,
            auto_update,
            no_stale_check,
            strict,
            why,
            include_self,
            include_docs,
            scope,
            diff,
            workspace,
        ),
        Commands::Pattern {
            pattern,
            lang,
            path,
            limit,
            why,
            scope,
            diff,
        } => cmd_pattern::pattern(&ctx, pattern, lang, path, limit, why, scope, diff),
        Commands::Update {
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
            no_wait,
            history,
            no_history,
            gpu,
            no_gpu,
            device,
        } => cmd_update::update(
            &ctx,
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
            no_wait,
            history,
            no_history,
            gpu,
            no_gpu,
            device,
        ),
        Commands::Outline { file, kind } => {
            cmd_outline::cmd_outline(&file, kind.as_deref(), &ctx.format)
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
            &ctx,
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
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
            &ctx,
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
        ),
        Commands::Status { path, coverage } => cmd_status::status(&ctx, path, coverage),
        Commands::Gpu { device, enable } => cmd_gpu::gpu(&ctx, device, enable),
        Commands::Grep {
            pattern,
            limit,
            filter_path,
            path,
            scope,
            diff,
            workspace,
        } => cmd_grep::grep(
            &ctx,
            pattern,
            limit,
            filter_path,
            path,
            scope,
            diff,
            workspace,
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
            &ctx,
            name,
            path,
            limit,
            auto_update,
            no_stale_check,
            scope,
            diff,
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
                &ctx,
                &name,
                path,
                limit,
                true,
                auto_update,
                no_stale_check,
                true, // include_stdlib: irrelevant for callers (filter is callees-only)
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
            include_stdlib,
            scope,
            diff,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            cmd_callgraph::cmd_callgraph(
                &ctx,
                &name,
                path,
                limit,
                false,
                auto_update,
                no_stale_check,
                include_stdlib,
                &path_scope,
                &diff,
            )
        }
        Commands::Diff {
            base,
            path,
            limit,
            scope,
        } => cmd_diff::diff(&ctx, base, path, limit, scope),
        Commands::Paths {
            from,
            to,
            max_hops,
            max_paths,
            path,
            auto_update,
            no_stale_check,
            scope,
        } => cmd_callgraph::paths(
            &ctx,
            from,
            to,
            max_hops,
            max_paths,
            path,
            auto_update,
            no_stale_check,
            scope,
        ),
        Commands::Reachable {
            target,
            max_hops,
            limit,
            path,
            auto_update,
            no_stale_check,
            scope,
        } => cmd_callgraph::reachable(
            &ctx,
            target,
            max_hops,
            limit,
            path,
            auto_update,
            no_stale_check,
            scope,
        ),
        Commands::TestsFor {
            target,
            max_hops,
            limit,
            path,
            auto_update,
            no_stale_check,
            test_pattern,
            include_fixtures,
            scope,
        } => cmd_tests_for::tests_for(
            &ctx,
            target,
            max_hops,
            limit,
            path,
            auto_update,
            no_stale_check,
            test_pattern,
            include_fixtures,
            scope,
        ),
        Commands::Check {
            names,
            path,
            auto_update,
            no_stale_check,
            workspace,
        } => cmd_check::check(&ctx, names, path, auto_update, no_stale_check, workspace),

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
        } => cmd_similar::similar(
            &ctx,
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
        ),
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
        } => cmd_duplicates::duplicates(
            &ctx,
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
        ),

        Commands::Completions { shell } => cmd_trivial::completions(shell),
        Commands::Init {
            agents_md,
            agents_md_only,
        } => cmd_trivial::init(agents_md, agents_md_only),
        Commands::Capabilities => cmd_trivial::capabilities(),

        Commands::History {
            symbol,
            path,
            depth,
            branch,
            limit,
            no_index,
            since,
            until,
            author,
            kind,
            diff,
            exact_presence,
            exact_presence_max_commits,
        } => cmd_history::history(
            &ctx,
            cmd_history::HistoryArgs {
                symbol,
                path,
                depth,
                branch,
                limit,
                no_index,
                since,
                until,
                author,
                kind,
                diff,
                exact_presence,
                exact_presence_max_commits,
            },
        ),

        Commands::Mcp { action } => match action {
            args::McpAction::Install {
                agent,
                server_name,
                binary_path,
                project_root,
                dry_run,
                force,
            } => cmd_mcp::install(
                &agent,
                server_name,
                binary_path,
                project_root,
                dry_run,
                force,
            ),
            args::McpAction::Uninstall { agent, server_name } => {
                cmd_mcp::uninstall(&agent, server_name)
            }
            args::McpAction::List { agent } => cmd_mcp::list(agent.as_deref()),
        },

        Commands::Eval {
            bench,
            min_ndcg,
            path,
            json,
        } => cmd_eval::cmd_eval(&ctx, path, bench, min_ndcg, json),

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
        } => cmd_bundle::bundle(
            &ctx,
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
        ),

        Commands::SelfUpdate { check, yes } => {
            // Named at the call site so a future refactor cannot silently
            // swap the two boolean positional args.
            cmd_self_update::cmd_self_update(/*check_only=*/ check, /*no_confirm=*/ yes)
        }
    }
}
