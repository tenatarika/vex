//! `vex similar` — semantic nearest-neighbour search over the HNSW
//! vector index. Extracted from `cli/mod.rs` in S1 Group E.

use anyhow::{bail, Context, Result};

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{
    diff_filter_meta, fetch_symbol_body, resolve_diff_filter, resolve_root, EXPLAIN_MAX_DIFF_LINES,
};
use super::index_management::ensure_index_ready;
use super::{output, scope};
use crate::store::reader::IndexReader;
use crate::util::config;

#[allow(clippy::too_many_arguments)]
pub(crate) fn similar(
    name: String,
    path: Option<std::path::PathBuf>,
    limit: usize,
    threshold: f32,
    filter_path: Option<String>,
    explain: bool,
    auto_update: bool,
    no_stale_check: bool,
    why: bool,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
    local_cache_active: bool,
    cfg: &config::VexConfig,
    format: &OutputFormat,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(path)?.canonicalize()?;
    let changed_paths = resolve_diff_filter(&root, &diff)?;
    let index_path = ensure_index_ready(
        &root,
        auto_update,
        no_stale_check,
        true,
        local_cache_active,
        cfg,
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
    let fetch_limit =
        if filter_path.is_some() || !path_scope.is_empty() || changed_paths.is_some() || why {
            reader.symbol_count()
        } else {
            limit
        };
    let matches =
        crate::search::similar::find_similar(&reader, &hnsw, &name, fetch_limit, threshold)?;
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
                    crate::search::explain::explain_pair(&seed_body, &body, EXPLAIN_MAX_DIFF_LINES)
                })
                .collect(),
        )
    } else {
        None
    };

    output::print_similar(&matches, &name, explanations.as_deref(), format);

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
