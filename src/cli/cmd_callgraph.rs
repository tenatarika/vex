//! `vex callers` and `vex callees` — direct call-graph edges.
//! Extracted from `cli/mod.rs` in S1 Group B. The bigger neighbour
//! commands `paths` / `reachable` remain inline in mod.rs and will move
//! into this file in S1 Group E (per the task plan).

use anyhow::Result;

use super::args::{self, OutputFormat};
use super::common::{resolve_diff_filter, resolve_root};
use super::index_management::{ensure_index_exists, handle_staleness};
use super::scope;
use crate::util::config;

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_callgraph(
    name: &str,
    path: Option<std::path::PathBuf>,
    limit: usize,
    is_callers: bool,
    auto_update: bool,
    no_stale_check: bool,
    local_cache_active: bool,
    cfg: &config::VexConfig,
    format: &OutputFormat,
    excludes: &[String],
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
        let should_auto = auto_update || cfg.auto_update.unwrap_or(false);
        if !idx.exists() {
            if should_auto {
                // Discard the IndexAvail return — we only need the side effect
                // of bootstrap. Reader is opened below the same way as before.
                ensure_index_exists(croot, auto_update, false, local_cache_active, cfg)?;
                // just-bootstrapped → manifest is fresh, skip handle_staleness
            }
            // else: live-scan path; no warning, this command supports it natively
        } else {
            handle_staleness(croot, auto_update, no_stale_check, cfg)?;
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
                crate::callgraph::find_callers(&root, name, fetch_limit, excludes)?
            } else {
                crate::callgraph::find_callees(&root, name, fetch_limit, excludes)?
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

    match &format {
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
            println!("{}", serde_json::to_string_pretty(&json)?);
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
