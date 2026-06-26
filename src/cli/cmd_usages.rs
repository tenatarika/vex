//! `vex usages` — find references to a symbol via the refs FST (default)
//! or the v5 reference_edges section (`--strict`). Extracted from
//! `cli/mod.rs` in S1 Group D.2.
//!
//! Phase 2 (v1.21): the ref-fetching + def-site / scope / docs filtering
//! routes through [`StrictRefsChannel`] / [`FstRefsChannel`] in the
//! `channel` module so the binder / FST split shares one implementation
//! with `vex impact`. Only `filter_path` and `diff` (the cmd_usages-
//! specific filters that don't fit the channel abstraction) are applied
//! locally, and the prefix-suggestion fallback stays here because it's
//! a query-shape concern, not a reference-resolution one.

use std::collections::HashMap;

use anyhow::{Context, Result};

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{diff_filter_meta, resolve_diff_filter, resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::scope;
use crate::channel::{
    build_def_sites, Channel, ChannelContext, FstRefsChannel, HitLocation, StrictRefsChannel,
};
use crate::protocol::capabilities;
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
    include_self: bool,
    include_docs: bool,
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

    let file_paths = reader.file_paths();
    // Build def_sites only when the FST channel will consult it
    // (non-strict mode honours `--include-self`). Empty map in
    // strict mode — the binder excludes def-sites by construction.
    let def_sites = if !strict {
        build_def_sites(&reader, &name)
    } else {
        HashMap::new()
    };

    let channel_ctx = ChannelContext {
        reader: &reader,
        root: &root,
        symbol: &name,
        file_paths: &file_paths,
        def_sites: &def_sites,
        path_scope: &path_scope,
        excludes: ctx.excludes,
        // v1.20.0 (D2) — non-strict defaults strip def-site + doc
        // mentions. `--include-self` / `--include-docs` opt back in.
        // Strict mode ignores both knobs: the scope-binder doesn't
        // surface def-sites or process doc files.
        filter_def_sites: !include_self,
        exclude_docs: !include_docs,
        // cmd_usages never runs the call-graph channels, so depth is
        // a no-op here. Pass `1` (the impact default) for clarity.
        depth: 1,
    };

    // `--strict` reads from the v5 reference_edges section
    // (binder-resolved refs only). The legacy FST still backs
    // the non-strict path because it captures identifiers in
    // every supported language, including the 16 without a
    // scope binder yet.
    let output = if strict {
        StrictRefsChannel.run(&channel_ctx)?
    } else {
        FstRefsChannel.run(&channel_ctx)?
    };

    // Preserve the v1.20.x bail messages so existing scripts +
    // docs still match. The channel reports the same conditions
    // via `available: false`; we convert into an `anyhow::bail!`
    // here so the CLI exit-code contract is unchanged.
    if !output.available {
        if strict {
            anyhow::bail!(
                "--strict needs a v5 index with reference_edges (this index is v{} or has no resolved refs). Re-run `vex index` to rebuild.",
                reader.header().version
            );
        }
        anyhow::bail!("no refs in index — re-run `vex index` to rebuild");
    }

    // Capture the un-filtered hit count and channel-reported drop
    // counters before applying cmd_usages-specific filters.
    let hits_before_filter = output.pre_filter_count;
    let def_site_dropped = output.dropped.def_site;
    let docs_dropped = output.dropped.docs;

    // Apply `filter_path` (substring) and `diff` (changed-path set)
    // on the channel's surviving hits. These two filters are
    // command-specific and stay outside the channel abstraction;
    // see `reference_v1_20_deferred_debt.md` Phase 2 spec.
    let post_filter: Vec<HitLocation> = output
        .hits
        .into_iter()
        .filter(|h| {
            let filter_ok = filter_path.as_deref().is_none_or(|fp| h.path.contains(fp));
            // Phase 13.7-D3: apply diff filter alongside path filters
            // so the trace's `total` reflects the post-diff count
            // exactly like it already reflects the post-scope count.
            let diff_ok = changed_paths.as_ref().is_none_or(|cp| cp.contains(&h.path));
            filter_ok && diff_ok
        })
        .collect();
    let total = post_filter.len();
    let diff_retained = total;
    // `diff_dropped` reports residual drops not attributed to
    // def_site or docs. With the Phase 2 channel migration this
    // residual now ALSO includes channel-side `dropped.scope`
    // (path_scope glob misses) — preserving the v1.20.x shape of
    // the `--why` trace where "diff_dropped" means
    // "everything not def_site / not docs".
    let diff_dropped = hits_before_filter
        .saturating_sub(total)
        .saturating_sub(def_site_dropped)
        .saturating_sub(docs_dropped);
    let entries: Vec<HitLocation> = post_filter.into_iter().take(limit).collect();

    // Prefix-suggestion fallback runs ONLY when no exact hits
    // and only against the FST-lookup path (strict-mode doesn't
    // have a prefix counterpart today). Stays outside the channel
    // abstraction — it's a query-shape concern (did-you-mean), not
    // a reference-resolution one.
    let prefix_suggestions = if entries.is_empty() && !strict {
        reader.ref_reader().map(|rr| rr.find_by_prefix(&name))
    } else {
        None
    };

    // v1.12.0 S8.2 — signal "no usages found" once for the exit-code
    // contract. Applies in both JSON and text formats.
    if entries.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    // Build the `--why` trace up-front so the JSON envelope (below) can
    // attach it to `_meta.vex.dev/why_trace`. Pre-v1.15.1 this was only
    // emitted on stderr (`VEX_WHY:` prefix) and never reached the JSON
    // consumer — agents that piped `--format json | jq` couldn't see it.
    let why_trace = if why {
        Some(crate::cli::trace::UsagesTrace {
            mode: trace_mode,
            mode_legacy: trace_mode_legacy,
            hits_before_filter,
            hits_after_filter: total,
            prefix_suggestions: prefix_suggestions.as_ref().map(|v| v.len()),
            def_site_dropped,
            docs_dropped,
            filter_applied: crate::cli::trace::FilterSnapshot {
                filter: filter_path.clone(),
                include: scope.include.clone(),
                exclude: scope.exclude.clone(),
            },
        })
    } else {
        None
    };

    match ctx.format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = entries
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "path": h.path,
                        "line": h.line,
                    })
                })
                .collect();
            // v1.15.1: thread `_meta` through `default_meta_for` so the
            // stale-fallback signal (`vex.dev/stale` + `stale_reason`)
            // populates here too. Pre-fix this used `MetaEnvelope::default()`
            // which always returned None — `usages` was one of the
            // commands the field-test report flagged for the
            // "successful-looking empty result set" trap.
            let mut meta = super::output::default_meta_for(&root);
            meta.diff_filter =
                diff_filter_meta(&diff, changed_paths.as_ref(), diff_retained, diff_dropped);
            if let Some(ref t) = why_trace {
                meta.why_trace = serde_json::to_value(t).ok();
            }
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
                for h in &entries {
                    println!("  {}:{}", h.path, h.line);
                }
            }
        }
    }

    // 11.10: structured trace on stderr for `--why`. Captured
    // post-print so stdout stays a pure result list. Retained alongside
    // the v1.15.1 envelope attachment so existing scripts that grep for
    // `VEX_WHY:` on stderr keep working.
    if let Some(trace) = why_trace.as_ref() {
        crate::cli::trace::emit_why_trace(trace)?;
        if let Some(df) =
            diff_filter_meta(&diff, changed_paths.as_ref(), diff_retained, diff_dropped)
        {
            crate::cli::trace::emit_diff_filter(&df)?;
        }
    }

    Ok(())
}

// is_doc_path unit tests live next to the function itself in
// `src/util/paths.rs` (moved here in v1.20.0 D4 so cmd_search and
// cmd_usages can share one filter).
