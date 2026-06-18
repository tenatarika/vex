//! `vex tests-for <Symbol>` — walk the persistent call graph BACKWARDS
//! from `<TARGET>` (same BFS as `vex reachable`), then post-filter the
//! result set down to test functions:
//!   1. path matches the test-glob set (default OR `--test-pattern`),
//!   2. (unless `--include-fixtures`) name passes Signal-B heuristic,
//!   3. stamp `framework` label from the path bucket.
//!
//! Empty result set → exit 1 (mirrors `vex reachable`).

use anyhow::{bail, Context, Result};

use super::args::ScopeArgs;
use super::cmd_callgraph::callers_of_warned;
use super::common::{resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::{self, TestsForRow};
use super::scope;
use crate::callgraph::test_patterns::{
    build_test_globset, framework_for_path, looks_like_test_name,
};
use crate::store::reader::IndexReader;

#[allow(clippy::too_many_arguments)]
pub(crate) fn tests_for(
    ctx: &CmdCtx<'_>,
    target: String,
    max_hops: usize,
    limit: usize,
    path: Option<std::path::PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
    test_pattern: Vec<String>,
    include_fixtures: bool,
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
            "no call graph in index — `vex tests-for` requires a v4 index built without \
             `--no-call-graph`. Rebuild with `vex index`."
        );
    }

    let test_globset = build_test_globset(&test_pattern)?;

    // Same over-fetch reasoning as `reachable`: post-filter is narrow,
    // so the BFS runs unbounded internally and we apply `take(limit)`
    // after path + signal-B trimming.
    let fetch_limit = usize::MAX;
    let callers_of = |name: &str| callers_of_warned(&reader, name, "tests-for set");
    let raw = crate::callgraph::bfs::find_reachable(callers_of, &target, max_hops, fetch_limit);

    let mut rows: Vec<TestsForRow> = raw
        .iter()
        .filter(|m| path_scope.accept(&m.path))
        .filter(|m| test_globset.is_match(&m.path))
        .filter(|m| include_fixtures || looks_like_test_name(&m.name))
        .map(|m| TestsForRow {
            framework: framework_for_path(&m.path),
            name: m.name.clone(),
            path: m.path.clone(),
            line: m.line,
            depth: m.depth,
        })
        .collect();

    // `--include-fixtures` semantic: in addition to weakening Signal-B,
    // expand one hop forward from each surviving test row and surface
    // any callee that lives in a test path. This is the "test cluster"
    // view — fixtures used by tests are part of the answer to "which
    // tests cover this code?". Bounded by `limit`; deduped by name.
    if include_fixtures && rows.len() < limit {
        let mut seen: std::collections::HashSet<String> =
            rows.iter().map(|r| r.name.clone()).collect();
        seen.insert(target.clone());
        let mut extra: Vec<TestsForRow> = Vec::new();
        'outer: for r in &rows {
            // Stop fetching callees once we already have enough rows
            // to fill the user's `--limit` — extra FST lookups would
            // just be discarded by the `take(limit)` below.
            if rows.len() + extra.len() >= limit {
                break;
            }
            let callees = crate::store::call_graph::find_callees_fast(
                &reader,
                &r.name,
                crate::callgraph::CALLERS_FETCH_CAP,
            );
            for c in callees {
                if rows.len() + extra.len() >= limit {
                    break 'outer;
                }
                if !seen.insert(c.name.clone()) {
                    continue;
                }
                if !path_scope.accept(&c.path) || !test_globset.is_match(&c.path) {
                    continue;
                }
                extra.push(TestsForRow {
                    framework: framework_for_path(&c.path),
                    name: c.name,
                    path: c.path,
                    line: c.line,
                    depth: r.depth + 1,
                });
            }
        }
        rows.extend(extra);
    }

    let rows: Vec<TestsForRow> = rows.into_iter().take(limit).collect();

    if rows.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    output::print_tests_for(&rows, &target, &ctx.format, &root);
    Ok(())
}
