//! Search-shaped tools: ranked / fuzzy / semantic / grep / pattern lookups.
//!
//! Extracted from `main.rs::build_command` in the v1.21 split.

use anyhow::Result;
use serde_json::Value;

use crate::args::{
    push_auto_update, push_diff_scope, push_kind, push_metadata, push_no_stale_check, push_scope,
    push_workspace,
};
use crate::params::{opt_bool, opt_f64, opt_str, opt_u64, read_canonical_str, req_str, ParamError};

pub(crate) fn build_search(
    args: &Value,
    _project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let query = req_str(args, "query")?;
    let limit = opt_u64(args, "limit", 20)?;
    let semantic = opt_bool(args, "semantic", false)?;
    let mut extra = vec![query.to_string(), "--limit".into(), limit.to_string()];
    if semantic {
        extra.push("--semantic".into());
    }
    // `--workspace` is a clap conflict with `--why` (workspace results are
    // grouped-by-repo; single-repo `--why` doesn't apply). Workspace wins:
    // skip `--why` in workspace mode, mirroring the CLI's own contract.
    let workspace = opt_bool(args, "workspace", false)?;
    if workspace {
        extra.push("--workspace".into());
    }
    if !workspace && opt_bool(args, "why", false)? {
        extra.push("--why".into());
    }
    if let Some(filter) = opt_str(args, "filter")? {
        extra.extend(["--filter".into(), filter.to_string()]);
    }
    push_kind(&mut extra, args)?;
    if let Some(cp) = opt_str(args, "context_path")? {
        extra.extend(["--context-path".into(), cp.to_string()]);
    }
    if opt_bool(args, "no_bm25", false)? {
        extra.push("--no-bm25".into());
    }
    // v1.20.0 (D4) — opt-in code-intent filter; strips hits in
    // `*.md`/`*.markdown`/`*.txt`/`*.rst`/`*.adoc` files.
    if opt_bool(args, "code_only", false)? {
        extra.push("--code-only".into());
    }
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_metadata(&mut extra, args)?;
    push_diff_scope(&mut extra, args)?;
    Ok(("search".to_string(), extra))
}

pub(crate) fn build_find_symbol(
    args: &Value,
    _project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let symbol = read_canonical_str(args, "symbol", "name", deprecated)?
        .ok_or_else(|| ParamError::missing("symbol"))?;
    let mut extra = vec![symbol.to_string(), "--limit".into(), "10".into()];
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_metadata(&mut extra, args)?;
    Ok(("search".to_string(), extra))
}

pub(crate) fn build_find_similar(
    args: &Value,
    _project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let query = req_str(args, "query")?;
    let mut extra = vec![
        query.to_string(),
        "--semantic".into(),
        "--limit".into(),
        "10".into(),
    ];
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_metadata(&mut extra, args)?;
    Ok(("search".to_string(), extra))
}

pub(crate) fn build_similar(
    args: &Value,
    project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let symbol = read_canonical_str(args, "symbol", "name", deprecated)?
        .ok_or_else(|| ParamError::missing("symbol"))?;
    let limit = opt_u64(args, "limit", 10)?;
    let threshold = opt_f64(args, "threshold")?.unwrap_or(0.5);
    let mut extra = vec![
        symbol.to_string(),
        "--path".into(),
        project_root.to_string(),
        "--limit".into(),
        limit.to_string(),
        "--threshold".into(),
        threshold.to_string(),
    ];
    if let Some(filter) = opt_str(args, "filter")? {
        extra.extend(["--filter".into(), filter.to_string()]);
    }
    if opt_bool(args, "explain", false)? {
        extra.push("--explain".into());
    }
    if opt_bool(args, "why", false)? {
        extra.push("--why".into());
    }
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_diff_scope(&mut extra, args)?;
    Ok(("similar".to_string(), extra))
}

pub(crate) fn build_duplicates(
    args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let threshold = opt_f64(args, "threshold")?.unwrap_or(0.9);
    let limit = opt_u64(args, "limit", 50)?;
    let min_body_lines = opt_u64(args, "min_body_lines", 5)?;
    let mut extra = vec![
        "--path".into(),
        project_root.to_string(),
        "--threshold".into(),
        threshold.to_string(),
        "--limit".into(),
        limit.to_string(),
        "--min-body-lines".into(),
        min_body_lines.to_string(),
    ];
    if let Some(filter) = opt_str(args, "filter")? {
        extra.extend(["--filter".into(), filter.to_string()]);
    }
    if opt_bool(args, "explain", false)? {
        extra.push("--explain".into());
    }
    if opt_bool(args, "why", false)? {
        extra.push("--why".into());
    }
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_diff_scope(&mut extra, args)?;
    Ok(("duplicates".to_string(), extra))
}

pub(crate) fn build_grep(
    args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let pattern = req_str(args, "pattern")?;
    let limit = opt_u64(args, "limit", 50)?;
    let mut extra = vec![
        pattern.to_string(),
        "--limit".into(),
        limit.to_string(),
        "--path".into(),
        project_root.to_string(),
    ];
    if let Some(filter) = opt_str(args, "filter")? {
        extra.extend(["--filter".into(), filter.to_string()]);
    }
    push_scope(&mut extra, args)?;
    push_workspace(&mut extra, args)?;
    Ok(("grep".to_string(), extra))
}

pub(crate) fn build_pattern(
    args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let pattern = req_str(args, "pattern")?;
    let lang = req_str(args, "lang")?;
    let limit = opt_u64(args, "limit", 50)?;
    let mut extra = vec![
        pattern.to_string(),
        "--lang".into(),
        lang.to_string(),
        "--path".into(),
        project_root.to_string(),
        "--limit".into(),
        limit.to_string(),
    ];
    // 11.4 Inc 8: surface the ScanTrace via --why so MCP agents
    // can observe which mode the prefilter selected and why.
    if opt_bool(args, "why", false)? {
        extra.push("--why".into());
    }
    push_scope(&mut extra, args)?;
    push_diff_scope(&mut extra, args)?;
    Ok(("pattern".to_string(), extra))
}
