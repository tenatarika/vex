//! `vex usages` — find references to a symbol via the refs FST (default)
//! or the v5 reference_edges section (`--strict`). Extracted from
//! `cli/mod.rs` in S1 Group D.2.

use anyhow::{Context, Result};

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{diff_filter_meta, resolve_diff_filter, resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::scope;
use crate::protocol::{capabilities, MetaEnvelope};
use crate::store::reader::IndexReader;

#[allow(clippy::too_many_arguments)]
pub(crate) fn usages(
    ctx: &CmdCtx<'_>,
    name: String,
    limit: usize,
    filter_path: Option<String>,
    auto_update: bool,
    no_stale_check: bool,
    strict: bool,
    why: bool,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(None)?.canonicalize()?;
    let changed_paths = resolve_diff_filter(&root, &diff)?;
    let index_path = ensure_index_ready(
        &root,
        auto_update,
        no_stale_check,
        false,
        ctx.local_cache_active,
        ctx.cfg,
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

    // v1.12.0 S8.2 — signal "no usages found" once for the exit-code
    // contract. Applies in both JSON and text formats.
    if entries.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
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
            let meta = MetaEnvelope {
                diff_filter: diff_filter_meta(
                    &diff,
                    changed_paths.as_ref(),
                    diff_retained,
                    diff_dropped,
                ),
                ..MetaEnvelope::default()
            };
            print_envelope(&json, capabilities::current(), meta);
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
