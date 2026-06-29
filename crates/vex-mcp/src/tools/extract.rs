//! Extraction-shaped tools: outline / show / check / history / tests_for —
//! pull bodies / kinds / historical versions for a known symbol or file.
//!
//! Extracted from `main.rs::build_command` in the v1.21 split.

use anyhow::Result;
use serde_json::Value;

use crate::args::{
    push_auto_update, push_kind, push_metadata, push_no_stale_check, push_scope,
    push_show_truncate, push_workspace,
};
use crate::params::{
    opt_bool, opt_str, opt_str_array, opt_u64, opt_u64_some, read_canonical_array,
    read_canonical_str, ParamError,
};

pub(crate) fn build_outline(
    args: &Value,
    _project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let path = read_canonical_str(args, "path", "file", deprecated)?
        .ok_or_else(|| ParamError::missing("path"))?;
    Ok(("outline".to_string(), vec![path.to_string()]))
}

pub(crate) fn build_show(
    args: &Value,
    _project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    // Canonical: `symbols: string[]`. Legacy: `symbol: string`
    // (singular, pre-1.7 shape) — still accepted, flagged as
    // deprecated.
    let symbols: Vec<String> = if let Some(items) = opt_str_array(args, "symbols")? {
        items.into_iter().map(String::from).collect()
    } else if let Some(s) = opt_str(args, "symbol")? {
        deprecated.push("symbol".into());
        vec![s.to_string()]
    } else {
        return Err(ParamError::missing_with_alias("symbols", "symbol").into());
    };
    let limit = opt_u64(args, "limit", 1)?;
    let mut extra = symbols;
    extra.extend(["--limit".into(), limit.to_string()]);
    if let Some(filter) = opt_str(args, "filter")? {
        extra.extend(["--filter".into(), filter.to_string()]);
    }
    push_kind(&mut extra, args)?;
    if let Some(cp) = opt_str(args, "context_path")? {
        extra.extend(["--context-path".into(), cp.to_string()]);
    }
    push_show_truncate(&mut extra, args)?;
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    push_metadata(&mut extra, args)?;
    Ok(("show".to_string(), extra))
}

pub(crate) fn build_check(
    args: &Value,
    _project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    let arr = read_canonical_array(args, "symbols", "names", deprecated)?
        .ok_or_else(|| ParamError::missing_with_alias("symbols", "names"))?;
    let symbols: Result<Vec<String>> = arr
        .iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_str().map(String::from).ok_or_else(|| {
                ParamError::wrong_type(&format!("symbols[{i}]"), "a string", v).into()
            })
        })
        .collect();
    let symbols = symbols?;
    if symbols.is_empty() {
        return Err(ParamError("`symbols` array is empty".to_string()).into());
    }
    let mut extra = symbols;
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_workspace(&mut extra, args)?;
    Ok(("check".to_string(), extra))
}

pub(crate) fn build_history(
    args: &Value,
    _project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    // v1.20.0 (D5) — surface `vex history` via MCP so agents can
    // ask "every historical version of this symbol" without
    // shelling out. The CLI's full flag set is exposed for
    // parity (date / author / kind filters, --diff,
    // --exact-presence, --no-index).
    let symbol = read_canonical_str(args, "symbol", "name", deprecated)?
        .ok_or_else(|| ParamError::missing("symbol"))?;
    // The CLI rejects `--diff` + `--exact-presence` together
    // (the diff path groups entries by `(symbol, kind)` which
    // breaks per-row presence mapping). Validate at the MCP
    // boundary so the client gets `-32602 Invalid params` with
    // the canonical shape, not an opaque downstream exit code.
    if opt_bool(args, "diff", false)? && opt_bool(args, "exact_presence", false)? {
        return Err(ParamError(
            "`diff` and `exact_presence` are mutually exclusive — the diff path \
             groups entries by `(symbol, kind)` which would break per-row presence \
             mapping. Choose one."
                .to_string(),
        )
        .into());
    }
    let mut extra = vec![symbol.to_string()];
    if let Some(d) = opt_u64_some(args, "depth")? {
        extra.extend(["--depth".into(), d.to_string()]);
    }
    if let Some(b) = opt_str(args, "branch")? {
        extra.extend(["--branch".into(), b.to_string()]);
    }
    if let Some(l) = opt_u64_some(args, "limit")? {
        extra.extend(["--limit".into(), l.to_string()]);
    }
    if opt_bool(args, "no_index", false)? {
        extra.push("--no-index".into());
    }
    if let Some(s) = opt_str(args, "since")? {
        extra.extend(["--since".into(), s.to_string()]);
    }
    if let Some(u) = opt_str(args, "until")? {
        extra.extend(["--until".into(), u.to_string()]);
    }
    if let Some(a) = opt_str(args, "author")? {
        extra.extend(["--author".into(), a.to_string()]);
    }
    if let Some(k) = opt_str(args, "kind")? {
        extra.extend(["--kind".into(), k.to_string()]);
    }
    if opt_bool(args, "diff", false)? {
        extra.push("--diff".into());
    }
    if opt_bool(args, "exact_presence", false)? {
        extra.push("--exact-presence".into());
    }
    Ok(("history".to_string(), extra))
}

pub(crate) fn build_tests_for(
    args: &Value,
    _project_root: &str,
    deprecated: &mut Vec<String>,
) -> Result<(String, Vec<String>)> {
    // v1.20.0 (D5) — surface Phase 13.10 `vex tests-for` via MCP.
    // The CLI subcommand uses the hyphenated form (`tests-for`)
    // but MCP tool names use underscores per common convention.
    // Canonical key is `target` (matching the CLI positional);
    // `symbol` is accepted as a deprecated alias for parity with
    // every other symbol-keyed tool.
    let target = read_canonical_str(args, "target", "symbol", deprecated)?
        .ok_or_else(|| ParamError::missing("target"))?;
    let max_hops = opt_u64(args, "max_hops", 6)?;
    let limit = opt_u64(args, "limit", 200)?;
    let mut extra = vec![
        target.to_string(),
        "--max-hops".into(),
        max_hops.to_string(),
        "--limit".into(),
        limit.to_string(),
    ];
    if let Some(patterns) = opt_str_array(args, "test_pattern")? {
        for p in patterns {
            extra.extend(["--test-pattern".into(), p.to_string()]);
        }
    }
    if opt_bool(args, "include_fixtures", false)? {
        extra.push("--include-fixtures".into());
    }
    push_auto_update(&mut extra, args)?;
    push_no_stale_check(&mut extra, args)?;
    push_scope(&mut extra, args)?;
    Ok(("tests-for".to_string(), extra))
}
