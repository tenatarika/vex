//! `vex diff` — symbol-level diff across the work-tree vs a base ref.
//! Extracted from `cli/mod.rs` in S1 Group B.2.

use anyhow::{Context, Result};

use super::args::OutputFormat;
use super::common::resolve_root;
use super::output;
use super::scope;

pub(crate) fn diff(
    base: String,
    path: Option<std::path::PathBuf>,
    limit: usize,
    scope: super::args::ScopeArgs,
    format: &OutputFormat,
    excludes: &[String],
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(path)?
        .canonicalize()
        .context("canonicalize root")?;
    let changes = crate::diff::diff_against_base(&root, &base, excludes, limit)?;
    let changes: Vec<_> = changes
        .into_iter()
        .filter(|c| path_scope.accept(&c.path))
        .collect();
    output::print_diff(&changes, &base, format);
    Ok(())
}
