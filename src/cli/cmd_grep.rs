//! `vex grep` — parallel regex content scan, no FST involvement.
//! Extracted from `cli/mod.rs` in S1 Group B.2.

use anyhow::Result;

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{resolve_diff_filter, resolve_root, CmdCtx};
use super::output::print_envelope;
use super::scope;
use crate::protocol::{capabilities, MetaEnvelope};

pub(crate) fn grep(
    ctx: &CmdCtx<'_>,
    pattern: String,
    limit: usize,
    filter_path: Option<String>,
    path: Option<std::path::PathBuf>,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(path)?;
    let changed_paths = resolve_diff_filter(&root, &diff)?;
    // Over-fetch when scope filters are active so post-filter truncation
    // does not silently drop matches the user expects to see. Same
    // treatment for the 13.7-D3 diff filter.
    let fetch_limit = if path_scope.is_empty() && changed_paths.is_none() {
        limit
    } else {
        usize::MAX
    };
    let matches = crate::grep::search(
        &root,
        &pattern,
        filter_path.as_deref(),
        fetch_limit,
        ctx.excludes,
    )?;
    let matches: Vec<_> = matches
        .into_iter()
        .filter(|m| {
            path_scope.accept(&m.path)
                && changed_paths.as_ref().is_none_or(|cp| cp.contains(&m.path))
        })
        .take(limit)
        .collect();

    match ctx.format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "path": m.path,
                        "line": m.line,
                        "text": m.text,
                    })
                })
                .collect();
            print_envelope(&json, capabilities::current(), MetaEnvelope::default());
        }
        OutputFormat::Text => {
            if matches.is_empty() {
                println!("No matches for \"{pattern}\"");
            } else {
                println!("{} matches\n", matches.len());
                for m in &matches {
                    println!("{}:{}", m.path, m.line);
                    println!("  {}", m.text);
                }
            }
        }
        OutputFormat::Compact => {
            for m in &matches {
                println!("{}:{}  {}", m.path, m.line, m.text);
            }
        }
    }
    Ok(())
}
