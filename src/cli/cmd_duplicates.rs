//! `vex duplicates` — pairwise near-duplicate scan over stored vectors.
//! Sibling of `cmd_similar`. Extracted from `cli/mod.rs` in S1 Group E.

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
pub(crate) fn duplicates(
    path: Option<std::path::PathBuf>,
    threshold: f32,
    limit: usize,
    min_body_lines: usize,
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
    let fetch_limit =
        if filter_path.is_some() || !path_scope.is_empty() || changed_paths.is_some() || why {
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
                    crate::search::explain::explain_pair(&a_body, &b_body, EXPLAIN_MAX_DIFF_LINES)
                })
                .collect(),
        )
    } else {
        None
    };

    output::print_duplicates(&pairs, explanations.as_deref(), format);

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
