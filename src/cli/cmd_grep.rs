//! `vex grep` — parallel regex content scan, no FST involvement.
//! Extracted from `cli/mod.rs` in S1 Group B.2. `--workspace` (multi-repo)
//! fans the same scan over every member of a `.vex-workspace.toml`.

use std::path::Path;

use anyhow::{anyhow, Result};

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{resolve_diff_filter, resolve_root, CmdCtx};
use super::output::print_envelope;
use super::scope;
use crate::grep::GrepMatch;
use crate::protocol::capabilities;
use crate::workspace;

#[allow(clippy::too_many_arguments)]
pub(crate) fn grep(
    ctx: &CmdCtx<'_>,
    pattern: String,
    limit: usize,
    filter_path: Option<String>,
    path: Option<std::path::PathBuf>,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
    workspace: bool,
) -> Result<()> {
    if workspace {
        return grep_workspace(
            ctx,
            &pattern,
            limit,
            filter_path.as_deref(),
            path,
            &scope,
            &diff,
        );
    }

    let root = resolve_root(path)?;
    let matches = grep_in_root(
        &root,
        &pattern,
        limit,
        filter_path.as_deref(),
        &scope,
        &diff,
        ctx.excludes,
    )?;

    // v1.12.0 S8.2 — signal "no matches" for the exit-code contract.
    if matches.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches.iter().map(grep_match_json).collect();
            print_envelope(
                &json,
                capabilities::current(),
                super::output::default_meta_for(&root),
            );
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

/// Scan one repo for `pattern`, applying the scope + diff filters and the
/// result cap. No index is involved — `grep` walks the filesystem.
fn grep_in_root(
    root: &Path,
    pattern: &str,
    limit: usize,
    filter_path: Option<&str>,
    scope: &ScopeArgs,
    diff: &DiffFilterArgs,
    excludes: &[String],
) -> Result<Vec<GrepMatch>> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let changed_paths = resolve_diff_filter(root, diff)?;
    // Over-fetch when scope filters are active so post-filter truncation
    // does not silently drop matches the user expects to see. Same
    // treatment for the 13.7-D3 diff filter.
    let fetch_limit = if path_scope.is_empty() && changed_paths.is_none() {
        limit
    } else {
        usize::MAX
    };
    let matches = crate::grep::search(root, pattern, filter_path, fetch_limit, excludes)?;
    Ok(matches
        .into_iter()
        .filter(|m| {
            path_scope.accept(&m.path)
                && changed_paths.as_ref().is_none_or(|cp| cp.contains(&m.path))
        })
        .take(limit)
        .collect())
}

fn grep_match_json(m: &GrepMatch) -> serde_json::Value {
    serde_json::json!({
        "path": m.path,
        "line": m.line,
        "text": m.text,
    })
}

/// `vex grep --workspace`: scan every member of the nearest
/// `.vex-workspace.toml`, grouping matches by repo. Each member uses its
/// own `.vex.toml` excludes.
#[allow(clippy::too_many_arguments)]
fn grep_workspace(
    ctx: &CmdCtx<'_>,
    pattern: &str,
    limit: usize,
    filter_path: Option<&str>,
    path: Option<std::path::PathBuf>,
    scope: &ScopeArgs,
    diff: &DiffFilterArgs,
) -> Result<()> {
    let start_dir = resolve_root(path)?;
    let ws_file = workspace::find_workspace_file(&start_dir).ok_or_else(|| {
        anyhow!(
            "no {} found at or above {}",
            workspace::WORKSPACE_FILE,
            start_dir.display()
        )
    })?;
    let ws = workspace::Workspace::load(&ws_file)?;
    let base = ws
        .file
        .parent()
        .expect("canonicalized workspace file has a parent directory")
        .to_path_buf();

    let mut per_repo: Vec<(String, Vec<GrepMatch>)> = Vec::with_capacity(ws.members.len());
    let mut any = false;
    for m in &ws.members {
        let member_cfg = crate::util::config::load_config(&m.root)?;
        let matches = grep_in_root(
            &m.root,
            pattern,
            limit,
            filter_path,
            scope,
            diff,
            &member_cfg.exclude,
        )?;
        any |= !matches.is_empty();
        per_repo.push((m.display_name.clone(), matches));
    }
    if !any {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
        OutputFormat::Json => {
            let repos: Vec<_> = per_repo
                .iter()
                .map(|(repo, matches)| {
                    let hits: Vec<_> = matches.iter().map(grep_match_json).collect();
                    serde_json::json!({ "repo": repo, "matches": hits })
                })
                .collect();
            print_envelope(
                serde_json::json!({
                    "workspace": ws.file.to_string_lossy(),
                    "repos": repos,
                }),
                capabilities::current(),
                super::output::default_meta_for(&base),
            );
        }
        // Text and Compact both group under a per-repo header so the repo
        // attribution survives the terse default format (matches the
        // `search --workspace` layout).
        OutputFormat::Text | OutputFormat::Compact => {
            for (repo, matches) in &per_repo {
                println!("── {repo} ──");
                if matches.is_empty() {
                    println!("  No matches for \"{pattern}\"");
                } else {
                    for m in matches {
                        println!("  {}:{}  {}", m.path, m.line, m.text);
                    }
                }
            }
        }
    }
    Ok(())
}
