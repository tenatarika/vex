//! Per-tool argv builders. Each MCP tool gets its own `build_<name>` fn
//! returning `(cli_subcommand, extra_args)`; `build_command` below is
//! the thin dispatcher that picks one based on the JSON-RPC tool name.
//!
//! Extracted from `main.rs::build_command` in the v1.21 split — see
//! `.claude/Task/v1.21-vex-mcp-split.md`.

use anyhow::Result;
use serde_json::Value;

mod admin;
mod bundle;
mod extract;
mod graph;
mod search;

/// Output of `build_command`. Carries the resolved vex subcommand plus the
/// argv to spawn, and a list of legacy MCP arg names the caller used so
/// the JSON-RPC response can surface a deprecation notice via `_meta`.
#[derive(Debug)]
pub(crate) struct BuiltCommand {
    pub(crate) subcommand: String,
    pub(crate) extra_args: Vec<String>,
    pub(crate) deprecated_args: Vec<String>,
}

pub(crate) fn build_command(tool: &str, args: &Value, project_root: &str) -> Result<BuiltCommand> {
    let mut deprecated: Vec<String> = Vec::new();
    let (subcommand, extra_args) = match tool {
        // Search-shaped: ranked / fuzzy / semantic / grep / pattern lookups.
        "search" => search::build_search(args, project_root, &mut deprecated)?,
        "find_symbol" => search::build_find_symbol(args, project_root, &mut deprecated)?,
        "find_similar" => search::build_find_similar(args, project_root, &mut deprecated)?,
        "similar" => search::build_similar(args, project_root, &mut deprecated)?,
        "duplicates" => search::build_duplicates(args, project_root, &mut deprecated)?,
        "grep" => search::build_grep(args, project_root, &mut deprecated)?,
        "pattern" => search::build_pattern(args, project_root, &mut deprecated)?,

        // Extraction: pull bodies / kinds / historical versions.
        "outline" => extract::build_outline(args, project_root, &mut deprecated)?,
        "show" => extract::build_show(args, project_root, &mut deprecated)?,
        "check" => extract::build_check(args, project_root, &mut deprecated)?,
        "history" => extract::build_history(args, project_root, &mut deprecated)?,
        "tests_for" => extract::build_tests_for(args, project_root, &mut deprecated)?,

        // Graph / reference traversal.
        "usages" => graph::build_usages(args, project_root, &mut deprecated)?,
        "impact" => graph::build_impact(args, project_root, &mut deprecated)?,
        "implementations" => graph::build_implementations(args, project_root, &mut deprecated)?,
        "subtypes" => graph::build_subtypes(args, project_root, &mut deprecated)?,
        "callers" => graph::build_callers(args, project_root, &mut deprecated)?,
        "callees" => graph::build_callees(args, project_root, &mut deprecated)?,
        "paths" => graph::build_paths(args, project_root, &mut deprecated)?,
        "reachable" => graph::build_reachable(args, project_root, &mut deprecated)?,
        "diff" => graph::build_diff(args, project_root, &mut deprecated)?,

        // Index lifecycle + introspection.
        "index" => admin::build_index(args, project_root, &mut deprecated)?,
        "update" => admin::build_update(args, project_root, &mut deprecated)?,
        "status" => admin::build_status(args, project_root, &mut deprecated)?,
        "eval" => admin::build_eval(args, project_root, &mut deprecated)?,
        "capabilities" => admin::build_capabilities(args, project_root, &mut deprecated)?,

        // Composite assembly.
        "bundle" => bundle::build_bundle(args, project_root, &mut deprecated)?,

        _ => anyhow::bail!("unknown tool: {tool}"),
    };
    Ok(BuiltCommand {
        subcommand,
        extra_args,
        deprecated_args: deprecated,
    })
}
