//! `vex callers` / `vex callees` (direct edges) plus `vex paths` and
//! `vex reachable` (multi-hop traversal). Extracted from `cli/mod.rs`
//! across S1 Group B + Group E.

use anyhow::{bail, Context, Result};

use super::args::{self, OutputFormat, ScopeArgs};
use super::common::{resolve_diff_filter, resolve_root, CmdCtx};
use super::index_management::{ensure_index_exists, ensure_index_ready, handle_staleness};
use super::output::{self, print_envelope};
use super::scope;
use crate::protocol::capabilities;
use crate::store::reader::IndexReader;
use crate::util::config;

/// Per-step caller-fetch cap. Used by `paths` and `reachable` to bound
/// memory while traversing the call graph. Picked well above realistic
/// fan-in so the warning below is a real saturation event, not noise.
const CALLERS_FETCH_CAP: usize = 1024;

/// Shared "fetch callers + warn on saturation" helper used by both
/// `paths` and `reachable`. Centralised here so the duplicated closure
/// bodies in the old `cli/mod.rs` arms (architect MUST-FIX #5) collapse
/// to one definition. `context` is a short label embedded in the
/// warning so the reader knows which traversal saturated.
pub(crate) fn callers_of_warned(
    reader: &IndexReader,
    name: &str,
    context: &str,
) -> Vec<crate::callgraph::CallMatch> {
    let callers = crate::store::call_graph::find_callers_fast(reader, name, CALLERS_FETCH_CAP);
    if callers.len() == CALLERS_FETCH_CAP {
        eprintln!(
            "warning: `{name}` has at least {CALLERS_FETCH_CAP} direct callers; \
             {context} may be incomplete past this node"
        );
    }
    callers
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_callgraph(
    ctx: &CmdCtx<'_>,
    name: &str,
    path: Option<std::path::PathBuf>,
    limit: usize,
    is_callers: bool,
    auto_update: bool,
    no_stale_check: bool,
    path_scope: &scope::PathScope,
    diff: &args::DiffFilterArgs,
) -> Result<()> {
    let root = resolve_root(path)?;
    let changed_paths = resolve_diff_filter(&root, diff)?;
    let label = if is_callers { "callers" } else { "callees" };
    let start = std::time::Instant::now();
    // Over-fetch when scope filters are active. Both the persistent FST and
    // live-scan paths accept `limit` as a hard cap, so without the inflation
    // a narrow `--include` would silently truncate matches. The 13.7-D3
    // diff filter is treated the same way — a narrow change-set is just a
    // tighter scope from the perspective of the post-filter.
    let fetch_limit = if path_scope.is_empty() && changed_paths.is_none() {
        limit
    } else {
        usize::MAX
    };

    // Fast path: if a v4 index with a call graph exists for this project,
    // use the persistent FST (~4ms). Otherwise fall back to the live-scan
    // implementation that walks files with tree-sitter (~seconds).
    //
    // Bootstrap/staleness behaviour, gated on `auto_update`:
    //   * missing index + auto-update on → bootstrap, then read the v4 FST.
    //   * stale index  + auto-update on → handle_staleness() refreshes it.
    //   * missing index + auto-update off → silently use live-scan (preserves
    //     pre-10.2 UX for projects without an index).
    let canonical_root = root.canonicalize().ok();
    let index_path = canonical_root.as_ref().map(|r| config::index_path(r));
    if let (Some(croot), Some(idx)) = (canonical_root.as_ref(), index_path.as_ref()) {
        let should_auto = auto_update || ctx.cfg.auto_update.unwrap_or(false);
        if !idx.exists() {
            if should_auto {
                // Discard the IndexAvail return — we only need the side effect
                // of bootstrap. Reader is opened below the same way as before.
                ensure_index_exists(croot, auto_update, false, ctx.local_cache_active, ctx.cfg)?;
                // just-bootstrapped → manifest is fresh, skip handle_staleness
            }
            // else: live-scan path; no warning, this command supports it natively
        } else {
            handle_staleness(croot, auto_update, no_stale_check, ctx.cfg)?;
        }
    }

    let reader = match index_path.as_ref().filter(|p| p.exists()) {
        Some(p) => match crate::store::reader::IndexReader::open(p) {
            Ok(r) => Some(r),
            Err(e) => {
                // Surface the reason for falling back so a corrupt/locked
                // index doesn't masquerade as "no index found". Direct
                // stderr (not tracing::warn!) because the fallback is a
                // load-bearing UX event — without it the user sees the
                // ~seconds live-scan latency and has no clue why.
                // `{e:#}` includes the anyhow chain (e.g. open + path).
                eprintln!(
                    "Warning: index at {} exists but failed to open ({e:#}). Falling back to live callgraph scan.",
                    p.display()
                );
                None
            }
        },
        None => None,
    };

    let matches = match reader.as_ref() {
        Some(r) if r.has_call_graph() => {
            if is_callers {
                crate::store::call_graph::find_callers_fast(r, name, fetch_limit)
            } else {
                crate::store::call_graph::find_callees_fast(r, name, fetch_limit)
            }
        }
        _ => {
            if is_callers {
                crate::callgraph::find_callers(&root, name, fetch_limit, ctx.excludes)?
            } else {
                crate::callgraph::find_callees(&root, name, fetch_limit, ctx.excludes)?
            }
        }
    };
    let matches: Vec<_> = matches
        .into_iter()
        .filter(|m| {
            path_scope.accept(&m.path)
                && changed_paths.as_ref().is_none_or(|cp| cp.contains(&m.path))
        })
        .take(limit)
        .collect();
    let elapsed = start.elapsed();

    // v1.12.0 S8.2 — signal "no callers/callees" for the exit-code
    // contract. Applies to both `vex callers` and `vex callees` since
    // they share this code path.
    if matches.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "path": m.path,
                        "line": m.line,
                    })
                })
                .collect();
            print_envelope(
                &json,
                capabilities::current(),
                super::output::default_meta_for(&root),
            );
        }
        OutputFormat::Text => {
            if matches.is_empty() {
                println!("No {label} of \"{name}\" in {elapsed:.2?}");
            } else {
                println!("{name}: {} {label} in {elapsed:.2?}\n", matches.len());
                for m in &matches {
                    println!("  {:<40} {}:{}", m.name, m.path, m.line);
                }
            }
        }
        OutputFormat::Compact => {
            for m in &matches {
                println!("{} {}:{}", m.name, m.path, m.line);
            }
        }
    }
    Ok(())
}

/// `vex paths from..to` — multi-hop caller chains. (S1 Group E.)
#[allow(clippy::too_many_arguments)]
pub(crate) fn paths(
    ctx: &CmdCtx<'_>,
    from: String,
    to: String,
    max_hops: usize,
    max_paths: usize,
    path: Option<std::path::PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
    scope: ScopeArgs,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(path)?.canonicalize()?;
    let index_path = ensure_index_ready(
        &root,
        auto_update,
        no_stale_check,
        false,
        ctx.local_cache_active,
        ctx.cfg,
    )?;
    let reader = IndexReader::open(&index_path).context("open index")?;
    if !reader.has_call_graph() {
        bail!(
            "no call graph in index — `vex paths` requires a v4 index built without \
             `--no-call-graph`. Rebuild with `vex index` (or `vex index --auto-update`)."
        );
    }
    let callers_of = |name: &str| callers_of_warned(&reader, name, "multi-hop traversal");
    let paths = crate::callgraph::bfs::find_paths(callers_of, &from, &to, max_hops, max_paths);
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
    // v1.12.0 S8.2 — extends exit-code contract to `vex paths`.
    if paths.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    output::print_paths(&paths, &from, &to, &ctx.format, &root);
    Ok(())
}

/// `vex reachable target` — every symbol that transitively calls
/// `target`. (S1 Group E.)
#[allow(clippy::too_many_arguments)]
pub(crate) fn reachable(
    ctx: &CmdCtx<'_>,
    target: String,
    max_hops: usize,
    limit: usize,
    path: Option<std::path::PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
    scope: ScopeArgs,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(path)?.canonicalize()?;
    let index_path = ensure_index_ready(
        &root,
        auto_update,
        no_stale_check,
        false,
        ctx.local_cache_active,
        ctx.cfg,
    )?;
    let reader = IndexReader::open(&index_path).context("open index")?;
    if !reader.has_call_graph() {
        bail!(
            "no call graph in index — `vex reachable` requires a v4 index built without \
             `--no-call-graph`. Rebuild with `vex index`."
        );
    }
    // When a scope filter is active a 4x over-fetch is not enough — a
    // narrow include could legitimately reject most of the BFS frontier
    // and leave the final list short. The traversal is already bounded
    // by `max_hops`, so we let it run unbounded internally and apply
    // `take(limit)` after the filter.
    let fetch_limit = if path_scope.is_empty() {
        limit
    } else {
        usize::MAX
    };
    let callers_of = |name: &str| callers_of_warned(&reader, name, "reachable set");
    let matches = crate::callgraph::bfs::find_reachable(callers_of, &target, max_hops, fetch_limit);
    let matches: Vec<_> = matches
        .into_iter()
        .filter(|m| path_scope.accept(&m.path))
        .take(limit)
        .collect();
    // v1.12.0 S8.2 — extends exit-code contract to `vex reachable`.
    if matches.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    output::print_reachable(&matches, &target, &ctx.format, &root);
    Ok(())
}
