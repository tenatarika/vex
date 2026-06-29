//! Index lifecycle + introspection tools: index / update / status / eval /
//! capabilities.
//!
//! Extracted from `main.rs::build_command` in the v1.21 split.

use anyhow::Result;
use serde_json::Value;

use crate::args::{push_gpu, push_workspace};
use crate::params::{opt_bool, opt_f64, opt_str};

pub(crate) fn build_index(
    args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let semantic = opt_bool(args, "semantic", false)?;
    let mut extra = vec!["--path".into(), project_root.to_string()];
    if semantic {
        extra.push("--semantic".into());
    }
    push_gpu(&mut extra, args)?;
    push_workspace(&mut extra, args)?;
    Ok(("index".to_string(), extra))
}

pub(crate) fn build_update(
    args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let semantic = opt_bool(args, "semantic", false)?;
    let mut extra = vec!["--path".into(), project_root.to_string()];
    if semantic {
        extra.push("--semantic".into());
    }
    push_gpu(&mut extra, args)?;
    push_workspace(&mut extra, args)?;
    Ok(("update".to_string(), extra))
}

pub(crate) fn build_status(
    _args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    Ok((
        "status".to_string(),
        vec!["--path".into(), project_root.to_string()],
    ))
}

pub(crate) fn build_eval(
    args: &Value,
    project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    // FU-1: thin wrapper around `vex eval`. Index-less in the sense
    // that it never builds — consumes whatever index already lives
    // at --path. Lives next to `status` / `diff` because all three
    // are indexless / read-only.
    let mut extra = vec!["--path".into(), project_root.to_string()];
    if let Some(bench) = opt_str(args, "bench")? {
        extra.push("--bench".into());
        extra.push(bench.to_string());
    }
    if let Some(min_ndcg) = opt_f64(args, "min_ndcg")? {
        extra.push("--min-ndcg".into());
        extra.push(min_ndcg.to_string());
    }
    // MCP defaults `json` to true (agents want structured output);
    // the CLI defaults to text. Honor an explicit `false` to opt
    // back into the human-readable summary.
    let want_json = opt_bool(args, "json", true)?;
    if want_json {
        extra.push("--json".into());
    }
    Ok(("eval".to_string(), extra))
}

pub(crate) fn build_capabilities(
    _args: &Value,
    _project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    // No project / index dependency — just dispatch to the CLI's
    // `capabilities` subcommand. Argument-free.
    Ok(("capabilities".to_string(), Vec::new()))
}
