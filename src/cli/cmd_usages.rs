//! `vex usages` — find references to a symbol via the refs FST (default)
//! or the v5 reference_edges section (`--strict`). Extracted from
//! `cli/mod.rs` in S1 Group D.2.

use anyhow::{Context, Result};

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{diff_filter_meta, resolve_diff_filter, resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::scope;
use crate::protocol::capabilities;
use crate::store::reader::IndexReader;

use crate::util::paths::is_doc_path;

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

    // v1.20.0 (D2) — non-strict noise filter. The FST refs index
    // returns every occurrence of an identifier, including the
    // symbol's own definition line and prose mentions in
    // README/CHANGELOG files. Both are noise for "find all callers"
    // queries; strict mode never had this problem because the
    // scope-binder excludes def-sites and skips doc files entirely.
    //
    // Build the (file_path, line) coordinates of every symbol named
    // `name` so the filter below can strip those rows. Empty when
    // `--include-self` is set, when in strict mode, or when the
    // symbol-FST is missing (pre-v1.8 index).
    //
    // Path normalisation invariant: both `read_string(sym.file_offset)`
    // (here) and `file_paths.get(e.file_id)` (below) read from the same
    // index built by `crate::store::writer`, which routes every
    // relative path through `util::paths::to_rel_posix` at write time.
    // The strings are therefore byte-identical on every platform —
    // no extra normalisation is needed for the `HashSet` lookup to
    // hit on Windows. See memory note `reference_windows_path_normalize`.
    let def_sites: std::collections::HashSet<(String, u32)> = if !strict && !include_self {
        let mut set = std::collections::HashSet::new();
        if let Some(sym_fst) = reader.symbol_fst_reader() {
            for sym_idx in sym_fst.find(&name) {
                if let Some(sym) = reader.symbol(sym_idx as usize) {
                    let file_path = reader.read_string(sym.file_offset).to_string();
                    set.insert((file_path, sym.line));
                }
            }
        }
        set
    } else {
        std::collections::HashSet::new()
    };
    let filter_docs = !strict && !include_docs;
    let mut def_site_dropped = 0usize;
    let mut docs_dropped = 0usize;
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
            // Path/scope/diff narrowing happens FIRST. A row that
            // never would have been displayed in the first place
            // doesn't get attributed to `def_site_dropped` or
            // `docs_dropped`; it falls into `diff_dropped` via the
            // arithmetic below. This keeps the `--why` counters
            // honest: `def_site_dropped` = "rows that would have
            // been displayed but were the def-site"; same for docs.
            if !(filter_ok && scope_ok && diff_ok) {
                return false;
            }
            // D2 — strip def-site + doc rows for the non-strict path.
            // (Corner case: a symbol defined in a `*.md` file is
            // also a doc; the `is_doc` check fires first so the row
            // is counted as `docs_dropped`, not `def_site_dropped`.
            // To resurrect such a row a user needs BOTH `--include-self`
            // AND `--include-docs`. Documented in LIMITATIONS §usages-noise.)
            if def_sites.contains(&(path.to_string(), e.line)) {
                def_site_dropped += 1;
                return false;
            }
            if filter_docs && is_doc_path(path) {
                docs_dropped += 1;
                return false;
            }
            true
        })
        .collect();
    let total = entries.len();
    let diff_retained = total;
    // diff_dropped reports rows lost to path / scope / diff narrowing
    // ONLY — def-site and doc filters are reported separately in the
    // `--why` trace so users can attribute the drop to the right
    // filter.
    let diff_dropped = hits_before_filter
        .saturating_sub(total)
        .saturating_sub(def_site_dropped)
        .saturating_sub(docs_dropped);
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
