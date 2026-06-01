//! `vex implementations` — find types implementing a base class/interface.
//! Extracted from `cli/mod.rs` in S1 Group D.

use std::time::Instant;

use anyhow::Result;

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{resolve_diff_filter, resolve_root, CmdCtx};
use super::index_management::handle_staleness;
use super::output::print_envelope;
use super::scope;
use crate::protocol::capabilities;

#[allow(clippy::too_many_arguments)]
pub(crate) fn implementations(
    ctx: &CmdCtx<'_>,
    name: String,
    path: Option<std::path::PathBuf>,
    limit: usize,
    auto_update: bool,
    no_stale_check: bool,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(path)?;
    handle_staleness(&root, auto_update, no_stale_check, ctx.cfg)?;
    let changed_paths = resolve_diff_filter(&root, &diff)?;
    let start = Instant::now();
    let fetch_limit = if path_scope.is_empty() && changed_paths.is_none() {
        limit
    } else {
        usize::MAX
    };
    let matches = crate::hierarchy::find_implementations(&root, &name, fetch_limit, ctx.excludes)?;
    let matches: Vec<_> = matches
        .into_iter()
        .filter(|m| {
            path_scope.accept(&m.path)
                && changed_paths.as_ref().is_none_or(|cp| cp.contains(&m.path))
        })
        .take(limit)
        .collect();
    let elapsed = start.elapsed();

    match ctx.format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "base": m.base,
                        "relation": m.relation,
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
                println!("No implementations of \"{name}\" in {elapsed:.2?}");
            } else {
                println!(
                    "{name}: {} implementations in {elapsed:.2?}\n",
                    matches.len()
                );
                for m in &matches {
                    println!("  {:<40} ({})  {}:{}", m.name, m.relation, m.path, m.line);
                }
            }
        }
        OutputFormat::Compact => {
            for m in &matches {
                println!("{} {} {} {}:{}", m.relation, m.base, m.name, m.path, m.line);
            }
        }
    }
    Ok(())
}
