//! `vex callers` / `vex callees` (direct edges) plus `vex paths` and
//! `vex reachable` (multi-hop traversal). Extracted from `cli/mod.rs`
//! across S1 Group B + Group E.

use std::path::Path;

use anyhow::{bail, Context, Result};

use super::args::{self, OutputFormat, ScopeArgs};
use super::common::{resolve_diff_filter, resolve_root, CmdCtx};
use super::index_management::{ensure_index_exists, ensure_index_ready, handle_staleness};
use super::output::{self, print_envelope};
use super::scope;
use crate::callgraph::CallMatch;
use crate::protocol::capabilities;
use crate::store::reader::IndexReader;
use crate::util::config::{self, VexConfig};
use crate::workspace;

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
    include_stdlib: bool,
    path_scope: &scope::PathScope,
    diff: &args::DiffFilterArgs,
    workspace: bool,
) -> Result<()> {
    let label = if is_callers { "callers" } else { "callees" };

    if workspace {
        return callgraph_workspace(
            ctx,
            name,
            path,
            limit,
            is_callers,
            auto_update,
            no_stale_check,
            include_stdlib,
            path_scope,
            diff,
            label,
        );
    }

    let root = resolve_root(path)?;
    let start = std::time::Instant::now();
    let matches = callgraph_matches(
        &root,
        ctx.cfg,
        ctx.excludes,
        ctx.local_cache_active,
        name,
        is_callers,
        limit,
        auto_update,
        no_stale_check,
        include_stdlib,
        path_scope,
        diff,
    )?;
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

/// Resolve a symbol's direct callers / callees in one repo. Uses the
/// persistent v4 call-graph FST when present, else live-scans the
/// filesystem with tree-sitter. Applies the scope / diff / stdlib filters
/// and the result cap. Shared by the single-repo and workspace paths.
#[allow(clippy::too_many_arguments)]
fn callgraph_matches(
    root: &Path,
    cfg: &VexConfig,
    excludes: &[String],
    local_cache_active: bool,
    name: &str,
    is_callers: bool,
    limit: usize,
    auto_update: bool,
    no_stale_check: bool,
    include_stdlib: bool,
    path_scope: &scope::PathScope,
    diff: &args::DiffFilterArgs,
) -> Result<Vec<CallMatch>> {
    let changed_paths = resolve_diff_filter(root, diff)?;
    // Over-fetch when scope filters are active. Both the persistent FST and
    // live-scan paths accept `limit` as a hard cap, so without the inflation
    // a narrow `--include` would silently truncate matches. The 13.7-D3
    // diff filter is treated the same way.
    let fetch_limit = if path_scope.is_empty() && changed_paths.is_none() {
        limit
    } else {
        usize::MAX
    };

    // Fast path: a v4 index with a call graph → persistent FST (~4ms).
    // Otherwise fall back to the live tree-sitter scan (~seconds).
    // Bootstrap/staleness gated on `auto_update` (see single-repo docs).
    let canonical_root = root.canonicalize().ok();
    let index_path = canonical_root.as_ref().map(|r| config::index_path(r));
    if let (Some(croot), Some(idx)) = (canonical_root.as_ref(), index_path.as_ref()) {
        let should_auto = auto_update || cfg.auto_update.unwrap_or(false);
        if !idx.exists() {
            if should_auto {
                ensure_index_exists(croot, auto_update, false, local_cache_active, cfg)?;
            }
        } else {
            handle_staleness(croot, auto_update, no_stale_check, cfg)?;
        }
    }

    let reader = match index_path.as_ref().filter(|p| p.exists()) {
        Some(p) => match crate::store::reader::IndexReader::open(p) {
            Ok(r) => Some(r),
            Err(e) => {
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
                crate::callgraph::find_callers(root, name, fetch_limit, excludes)?
            } else {
                crate::callgraph::find_callees(root, name, fetch_limit, excludes)?
            }
        }
    };
    // v1.15.1: default-on stdlib/macro filter for `vex callees` (callers
    // are user-defined by construction, so the filter is a no-op there).
    let apply_stdlib_filter = !is_callers && !include_stdlib;
    Ok(matches
        .into_iter()
        .filter(|m| {
            path_scope.accept(&m.path)
                && changed_paths.as_ref().is_none_or(|cp| cp.contains(&m.path))
                && (!apply_stdlib_filter
                    || !crate::callgraph::stdlib_filter::is_likely_stdlib_or_macro(&m.name))
        })
        .take(limit)
        .collect())
}

/// `vex callers/callees --workspace`: resolve edges in every member,
/// grouped by repo. Edges resolve per-repo — a caller in repo B of a
/// symbol defined in repo A is not seen (see `docs/LIMITATIONS.md` §7).
#[allow(clippy::too_many_arguments)]
fn callgraph_workspace(
    ctx: &CmdCtx<'_>,
    name: &str,
    path: Option<std::path::PathBuf>,
    limit: usize,
    is_callers: bool,
    auto_update: bool,
    no_stale_check: bool,
    include_stdlib: bool,
    path_scope: &scope::PathScope,
    diff: &args::DiffFilterArgs,
    label: &str,
) -> Result<()> {
    if ctx.local_cache_active {
        bail!(
            "workspace mode does not support local_cache / a hash-less cache dir — \
             members would collide into one index dir; use the platform cache"
        );
    }

    let start_dir = resolve_root(path)?;
    let ws = workspace::Workspace::find_and_load(&start_dir)?;
    let base = ws.base().to_path_buf();

    crate::cli::stale_signal::reset();
    let mut per_repo: Vec<(String, Vec<CallMatch>, Option<String>)> =
        Vec::with_capacity(ws.members.len());
    let mut any = false;
    for m in &ws.members {
        let member_cfg = config::load_config(&m.root)?;
        let matches = callgraph_matches(
            &m.root,
            &member_cfg,
            &member_cfg.exclude,
            false,
            name,
            is_callers,
            limit,
            auto_update,
            no_stale_check,
            include_stdlib,
            path_scope,
            diff,
        )?;
        let stale = crate::cli::stale_signal::take();
        any |= !matches.is_empty();
        per_repo.push((m.display_name.clone(), matches, stale));
    }
    if !any {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
        OutputFormat::Json => {
            let repos: Vec<_> = per_repo
                .iter()
                .map(|(repo, matches, stale)| {
                    let edges: Vec<_> = matches
                        .iter()
                        .map(|m| {
                            serde_json::json!({ "name": m.name, "path": m.path, "line": m.line })
                        })
                        .collect();
                    let mut obj = serde_json::json!({ "repo": repo, label: edges });
                    if let Some(reason) = stale {
                        obj["stale_reason"] = serde_json::json!(reason);
                    }
                    obj
                })
                .collect();
            print_envelope(
                serde_json::json!({
                    "workspace": ws.file.to_string_lossy(),
                    "repos": repos,
                }),
                capabilities::current(),
                super::output::default_meta_for(&base),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            for (repo, matches, stale) in &per_repo {
                println!("── {repo} ──");
                if let Some(reason) = stale {
                    eprintln!("  (stale: {reason})");
                }
                if matches.is_empty() {
                    println!("  No {label} of \"{name}\"");
                } else {
                    for m in matches {
                        println!("  {:<40} {}:{}", m.name, m.path, m.line);
                    }
                }
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
    workspace: bool,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;

    if workspace {
        return reachable_workspace(
            ctx,
            &target,
            max_hops,
            limit,
            path,
            auto_update,
            no_stale_check,
            &path_scope,
        );
    }

    let root = resolve_root(path)?.canonicalize()?;
    let outcome = reachable_in_root(
        &root,
        ctx.cfg,
        ctx.local_cache_active,
        &target,
        max_hops,
        limit,
        auto_update,
        no_stale_check,
        &path_scope,
    )?;
    if !outcome.available {
        bail!(
            "{}",
            outcome.unavailable_reason.unwrap_or_else(|| {
                "no call graph in index — `vex reachable` requires a v4 index built without \
                 `--no-call-graph`. Rebuild with `vex index`."
                    .into()
            })
        );
    }
    // v1.12.0 S8.2 — extends exit-code contract to `vex reachable`.
    if outcome.matches.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    output::print_reachable(&outcome.matches, &target, &ctx.format, &root);
    Ok(())
}

/// One repo's reachable-set outcome. `available = false` carries the
/// "no call graph" reason so a workspace fanout reports it per-repo
/// instead of aborting.
struct ReachableOutcome {
    available: bool,
    unavailable_reason: Option<String>,
    matches: Vec<crate::callgraph::bfs::ReachableMatch>,
}

#[allow(clippy::too_many_arguments)]
fn reachable_in_root(
    root: &Path,
    cfg: &VexConfig,
    local_cache_active: bool,
    target: &str,
    max_hops: usize,
    limit: usize,
    auto_update: bool,
    no_stale_check: bool,
    path_scope: &scope::PathScope,
) -> Result<ReachableOutcome> {
    let index_path = ensure_index_ready(
        root,
        auto_update,
        no_stale_check,
        false,
        local_cache_active,
        cfg,
    )?;
    let reader = IndexReader::open(&index_path).context("open index")?;
    if !reader.has_call_graph() {
        return Ok(ReachableOutcome {
            available: false,
            unavailable_reason: Some(
                "no call graph in index — `vex reachable` requires a v4 index built without \
                 `--no-call-graph`. Rebuild with `vex index`."
                    .to_string(),
            ),
            matches: Vec::new(),
        });
    }
    // A narrow scope filter could reject most of the BFS frontier, so
    // over-fetch unbounded (the traversal is bounded by `max_hops`) and
    // `take(limit)` after the filter.
    let fetch_limit = if path_scope.is_empty() {
        limit
    } else {
        usize::MAX
    };
    let callers_of = |name: &str| callers_of_warned(&reader, name, "reachable set");
    let matches: Vec<crate::callgraph::bfs::ReachableMatch> =
        crate::callgraph::bfs::find_reachable(callers_of, target, max_hops, fetch_limit)
            .into_iter()
            .filter(|m| path_scope.accept(&m.path))
            .take(limit)
            .collect();
    Ok(ReachableOutcome {
        available: true,
        unavailable_reason: None,
        matches,
    })
}

/// `vex reachable --workspace`: per-repo reachable set, grouped by repo.
/// Edges resolve per-repo (see `docs/LIMITATIONS.md` §7).
#[allow(clippy::too_many_arguments)]
fn reachable_workspace(
    ctx: &CmdCtx<'_>,
    target: &str,
    max_hops: usize,
    limit: usize,
    path: Option<std::path::PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
    path_scope: &scope::PathScope,
) -> Result<()> {
    if ctx.local_cache_active {
        bail!(
            "workspace mode does not support local_cache / a hash-less cache dir — \
             members would collide into one index dir; use the platform cache"
        );
    }

    let start_dir = resolve_root(path)?;
    let ws = workspace::Workspace::find_and_load(&start_dir)?;
    let base = ws.base().to_path_buf();

    crate::cli::stale_signal::reset();
    let mut per_repo: Vec<(String, ReachableOutcome, Option<String>)> =
        Vec::with_capacity(ws.members.len());
    let mut any = false;
    for m in &ws.members {
        let member_cfg = config::load_config(&m.root)?;
        let outcome = reachable_in_root(
            &m.root,
            &member_cfg,
            false,
            target,
            max_hops,
            limit,
            auto_update,
            no_stale_check,
            path_scope,
        )?;
        let stale = crate::cli::stale_signal::take();
        any |= outcome.available && !outcome.matches.is_empty();
        per_repo.push((m.display_name.clone(), outcome, stale));
    }
    if !any {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
        OutputFormat::Json => {
            let repos: Vec<_> = per_repo
                .iter()
                .map(|(repo, outcome, stale)| {
                    let reachable: Vec<_> = outcome
                        .matches
                        .iter()
                        .map(|m| {
                            serde_json::json!({ "name": m.name, "path": m.path, "line": m.line })
                        })
                        .collect();
                    let mut obj = serde_json::json!({ "repo": repo, "reachable": reachable });
                    if !outcome.available {
                        obj["unavailable"] = serde_json::json!(outcome.unavailable_reason);
                    }
                    if let Some(reason) = stale {
                        obj["stale_reason"] = serde_json::json!(reason);
                    }
                    obj
                })
                .collect();
            print_envelope(
                serde_json::json!({
                    "workspace": ws.file.to_string_lossy(),
                    "repos": repos,
                }),
                capabilities::current(),
                super::output::default_meta_for(&base),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            for (repo, outcome, stale) in &per_repo {
                println!("── {repo} ──");
                if let Some(reason) = stale {
                    eprintln!("  (stale: {reason})");
                }
                if !outcome.available {
                    println!(
                        "  unavailable: {}",
                        outcome
                            .unavailable_reason
                            .as_deref()
                            .unwrap_or("no call graph")
                    );
                } else if outcome.matches.is_empty() {
                    println!("  Nothing reaches \"{target}\"");
                } else {
                    for m in &outcome.matches {
                        println!("  {:<40} {}:{}", m.name, m.path, m.line);
                    }
                }
            }
        }
    }
    Ok(())
}
