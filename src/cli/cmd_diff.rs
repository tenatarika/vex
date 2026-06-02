//! `vex diff` — symbol-level diff across the work-tree vs a base ref.
//! Extracted from `cli/mod.rs` in S1 Group B.2.

use anyhow::{Context, Result};

use super::common::{resolve_root, CmdCtx};
use super::output;
use super::scope;

pub(crate) fn diff(
    ctx: &CmdCtx<'_>,
    base: String,
    path: Option<std::path::PathBuf>,
    limit: usize,
    scope: super::args::ScopeArgs,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(path)?
        .canonicalize()
        .context("canonicalize root")?;
    let changes = crate::diff::diff_against_base(&root, &base, ctx.excludes, limit)?;
    let changes: Vec<_> = changes
        .into_iter()
        .filter(|c| path_scope.accept(&c.path))
        .collect();

    // v1.12.0 S8.2 — extends exit-code contract to `vex diff`. An empty
    // diff (no symbol-level changes across the base ref) maps to exit 1
    // so scripts can short-circuit follow-up work without re-running git.
    if changes.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    output::print_diff(&changes, &base, &ctx.format, &root);
    Ok(())
}
