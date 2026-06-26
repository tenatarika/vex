//! Composite `bundle` tool — Phase 13.2 multi-source assembly.
//!
//! Extracted from `main.rs::build_command` in the v1.21 split.

use anyhow::Result;
use serde_json::Value;

use crate::args::{push_auto_update, push_no_stale_check, push_scope};
use crate::params::{opt_str, opt_u64_some, req_str, ParamError};

pub(crate) fn build_bundle(
    args: &Value,
    _project_root: &str,
    _deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    // Phase 13.2 — flat schema (architect-review A4: no JSON-Schema
    // `oneOf`, zero precedent in this codebase, untested MCP-client
    // support for discriminated unions). The MCP layer validates
    // required-field-per-mode here so the agent sees a clear
    // error before the CLI subprocess is spawned.
    let mode = req_str(args, "mode")?;
    let mut extra: Vec<String> = vec!["--mode".into(), mode.into()];
    match mode {
        "symbol" => {
            let symbol = opt_str(args, "symbol")?.ok_or_else(|| {
                ParamError("`mode: symbol` requires the `symbol` field".to_string())
            })?;
            extra.extend(["--symbol".into(), symbol.into()]);
            if let Some(v) = opt_u64_some(args, "callers_max")? {
                extra.extend(["--callers-max".into(), v.to_string()]);
            }
            if let Some(v) = opt_u64_some(args, "callees_max")? {
                extra.extend(["--callees-max".into(), v.to_string()]);
            }
            if let Some(v) = opt_u64_some(args, "similar_max")? {
                extra.extend(["--similar-max".into(), v.to_string()]);
            }
        }
        "pr-impact" => {
            let base = opt_str(args, "base")?.ok_or_else(|| {
                ParamError("`mode: pr-impact` requires the `base` field".to_string())
            })?;
            extra.extend(["--base".into(), base.into()]);
            if let Some(d) = opt_u64_some(args, "depth")? {
                extra.extend(["--depth".into(), d.to_string()]);
            }
            if let Some(m) = opt_u64_some(args, "tests_max")? {
                extra.extend(["--tests-max".into(), m.to_string()]);
            }
        }
        "project" => {
            if let Some(g) = opt_str(args, "path_glob")? {
                extra.extend(["--path-glob".into(), g.into()]);
            }
            if let Some(n) = opt_u64_some(args, "top_n")? {
                extra.extend(["--top-n".into(), n.to_string()]);
            }
        }
        other => {
            return Err(ParamError(format!(
                "unknown bundle mode `{other}` — expected one of `symbol`, `pr-impact`, `project`"
            ))
            .into());
        }
    }
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    Ok(("bundle".to_string(), extra))
}
