//! Graph- and reference-shaped tools: usages, callers, callees, paths,
//! reachable, impact, implementations, diff.
//!
//! Extracted from `main.rs::build_command` in the v1.21 split.

use anyhow::Result;
use serde_json::Value;

use crate::args::{
    push_auto_update, push_diff_scope, push_no_stale_check, push_scope, push_workspace,
};
use crate::params::{opt_bool, opt_u64, opt_u64_some, read_canonical_str, req_str, ParamError};

pub(crate) fn build_usages(
    args: &Value,
    _project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let symbol = read_canonical_str(args, "symbol", "name", deprecated)?
        .ok_or_else(|| ParamError::missing("symbol"))?;
    let limit = opt_u64(args, "limit", 50)?;
    let mut extra = vec![symbol.to_string(), "--limit".into(), limit.to_string()];
    if opt_bool(args, "strict", false)? {
        extra.push("--strict".into());
    }
    // `--workspace` is a clap conflict with `--why` (single-repo only).
    // Workspace wins: skip `--why` in workspace mode, mirroring the CLI.
    let workspace = opt_bool(args, "workspace", false)?;
    if workspace {
        extra.push("--workspace".into());
    }
    // 11.10: structured trace via stderr — picked up by
    // `extract_why_trace` and surfaced under `_meta.why`.
    if !workspace && opt_bool(args, "why", false)? {
        extra.push("--why".into());
    }
    // v1.20.0 (D2): opt-in escape hatches for the new
    // non-strict noise filter. Defaults strip def-site + doc
    // mentions; clients that need the old wide-net behaviour
    // (e.g. searching for a name across CHANGELOG + README)
    // set these to true.
    if opt_bool(args, "include_self", false)? {
        extra.push("--include-self".into());
    }
    if opt_bool(args, "include_docs", false)? {
        extra.push("--include-docs".into());
    }
    // `filter_path` canonical; `filter` back-compat alias (§3.3). Spawn the
    // established `--filter` CLI flag for mixed-version safety.
    if let Some(filter) = read_canonical_str(args, "filter_path", "filter", deprecated)? {
        extra.extend(["--filter".into(), filter.to_string()]);
    }
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_diff_scope(&mut extra, args)?;
    Ok(("usages".to_string(), extra))
}

pub(crate) fn build_impact(
    args: &Value,
    _project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    // v1.20.0 (F1) — one-call delete-safety report. Composes
    // strict refs + FST refs + grep \b<Name>\b + call-graph
    // callers into a single verdict.
    // v1.20.1 — opt-in `exclude_docs` strips text-channel hits in
    // prose-format paths (D4 parity with `vex search --code-only`).
    // v1.21.0 — `depth: u64` opts into the `transitive_callers` BFS
    // channel for indirect callers up to N hops.
    let symbol = read_canonical_str(args, "symbol", "name", deprecated)?
        .ok_or_else(|| ParamError::missing("symbol"))?;
    let mut extra = vec![symbol.to_string()];
    if opt_bool(args, "exclude_docs", false)? {
        extra.push("--exclude-docs".into());
    }
    if let Some(d) = opt_u64_some(args, "depth")? {
        // Validate against the descriptor's advertised `[1, 16]`
        // range up front so MCP clients sending `depth: 0` or
        // `depth: 100` see a clean `-32602 Invalid params` instead
        // of a silent CLI-side clamp. The CLI also clamps as a
        // belt-and-suspenders safety net.
        if !(1..=16).contains(&d) {
            return Err(ParamError(format!("`depth` must be between 1 and 16 (got: {d})")).into());
        }
        extra.extend(["--depth".into(), d.to_string()]);
    }
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_workspace(&mut extra, args)?;
    Ok(("impact".to_string(), extra))
}

pub(crate) fn build_implementations(
    args: &Value,
    project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let symbol = read_canonical_str(args, "symbol", "name", deprecated)?
        .ok_or_else(|| ParamError::missing("symbol"))?;
    let limit = opt_u64(args, "limit", 50)?;
    let mut extra = vec![
        symbol.to_string(),
        "--path".into(),
        project_root.to_string(),
        "--limit".into(),
        limit.to_string(),
    ];
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_diff_scope(&mut extra, args)?;
    Ok(("implementations".to_string(), extra))
}

/// P3 (`docs/HIERARCHY-EDGES.md` §7, §8) — transitive-descendant
/// counterpart to `build_implementations`. Same argv shape plus an
/// optional `depth` passthrough (BFS hop cap on the CLI side). `depth` is
/// range-validated to `[1, 4096]` up front (mirroring `build_impact`) so a
/// client sending `depth: 0` gets a clean `-32602` instead of a silently
/// empty result, and an absurd value can't drive a needless full-graph
/// walk. The BFS is independently bounded by the cycle guard, so this cap
/// is defense-in-depth / UX, not a safety requirement.
pub(crate) fn build_subtypes(
    args: &Value,
    project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let symbol = read_canonical_str(args, "symbol", "name", deprecated)?
        .ok_or_else(|| ParamError::missing("symbol"))?;
    let limit = opt_u64(args, "limit", 50)?;
    let mut extra = vec![
        symbol.to_string(),
        "--path".into(),
        project_root.to_string(),
        "--limit".into(),
        limit.to_string(),
    ];
    if let Some(d) = opt_u64_some(args, "depth")? {
        if !(1..=4096).contains(&d) {
            return Err(
                ParamError(format!("`depth` must be between 1 and 4096 (got: {d})")).into(),
            );
        }
        extra.extend(["--depth".into(), d.to_string()]);
    }
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_diff_scope(&mut extra, args)?;
    Ok(("subtypes".to_string(), extra))
}

pub(crate) fn build_callers(
    args: &Value,
    project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let symbol = read_canonical_str(args, "symbol", "name", deprecated)?
        .ok_or_else(|| ParamError::missing("symbol"))?;
    let limit = opt_u64(args, "limit", 50)?;
    let mut extra = vec![
        symbol.to_string(),
        "--path".into(),
        project_root.to_string(),
        "--limit".into(),
        limit.to_string(),
    ];
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_diff_scope(&mut extra, args)?;
    push_workspace(&mut extra, args)?;
    Ok(("callers".to_string(), extra))
}

pub(crate) fn build_callees(
    args: &Value,
    project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let symbol = read_canonical_str(args, "symbol", "name", deprecated)?
        .ok_or_else(|| ParamError::missing("symbol"))?;
    let limit = opt_u64(args, "limit", 50)?;
    let mut extra = vec![
        symbol.to_string(),
        "--path".into(),
        project_root.to_string(),
        "--limit".into(),
        limit.to_string(),
    ];
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_diff_scope(&mut extra, args)?;
    push_workspace(&mut extra, args)?;
    Ok(("callees".to_string(), extra))
}

pub(crate) fn build_paths(
    args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let from = req_str(args, "from")?;
    let to = req_str(args, "to")?;
    let max_hops = opt_u64(args, "max_hops", 6)?;
    let max_paths = opt_u64(args, "max_paths", 50)?;
    let mut extra = vec![
        from.to_string(),
        to.to_string(),
        "--path".into(),
        project_root.to_string(),
        "--max-hops".into(),
        max_hops.to_string(),
        "--max-paths".into(),
        max_paths.to_string(),
    ];
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    Ok(("paths".to_string(), extra))
}

pub(crate) fn build_reachable(
    args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let target = req_str(args, "target")?;
    let max_hops = opt_u64(args, "max_hops", 6)?;
    let limit = opt_u64(args, "limit", 200)?;
    let mut extra = vec![
        target.to_string(),
        "--path".into(),
        project_root.to_string(),
        "--max-hops".into(),
        max_hops.to_string(),
        "--limit".into(),
        limit.to_string(),
    ];
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_workspace(&mut extra, args)?;
    Ok(("reachable".to_string(), extra))
}

pub(crate) fn build_diff(
    args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let base = req_str(args, "base")?;
    let limit = opt_u64(args, "limit", 500)?;
    let mut extra = vec![
        "--base".into(),
        base.to_string(),
        "--path".into(),
        project_root.to_string(),
        "--limit".into(),
        limit.to_string(),
    ];
    push_scope(&mut extra, args)?;
    Ok(("diff".to_string(), extra))
}
