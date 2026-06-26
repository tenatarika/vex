//! `push_*` helpers that translate MCP JSON-RPC argument blobs into
//! repeated CLI flags. Each helper mirrors a clap-defined input on the
//! `vex` binary side; mutually-exclusive sets short-circuit with a
//! `ParamError` so MCP clients see an intent-aware `-32602` instead of
//! clap's templated `conflicts_with` output.
//!
//! Extracted from `main.rs` in the v1.21 split — see
//! `.claude/Task/v1.21-vex-mcp-split.md`.

use anyhow::Result;
use serde_json::Value;

use crate::params::{opt_bool, opt_bool_some, opt_str, opt_str_array, opt_u64, ParamError};

/// Whether the caller asked vex to auto-update the index if stale.
/// Defaults to `true` because the bare CLI does the same thing for the
/// commands that accept the flag, and MCP clients are otherwise unable
/// to react to staleness errors mid-conversation.
fn auto_update(args: &Value) -> Result<bool> {
    opt_bool(args, "auto_update", true)
}

pub(crate) fn push_auto_update(extra: &mut Vec<String>, args: &Value) -> Result<()> {
    if auto_update(args)? {
        extra.push("--auto-update".into());
    }
    Ok(())
}

/// Translate the optional `gpu: bool` (and advanced `device: string`) MCP args
/// into the CLI `--gpu` / `--no-gpu` / `--device` flags for `index`/`update`.
/// Tri-state on purpose: an absent `gpu` forwards nothing (so `.vex.toml gpu` /
/// `$VEX_DEVICE` win via the CLI's `Device::resolve`), `gpu: false` forwards
/// `--no-gpu` (overriding config `gpu = true`), and `gpu: true` forwards
/// `--gpu`. `device` (advanced) is mutually exclusive with the `gpu` boolean —
/// passing both forwards conflicting flags that the CLI rejects (clap
/// `conflicts_with`), mirroring `vex index --gpu --device`. See docs/GPU_SUPPORT.md.
pub(crate) fn push_gpu(extra: &mut Vec<String>, args: &Value) -> Result<()> {
    match opt_bool_some(args, "gpu")? {
        Some(true) => extra.push("--gpu".into()),
        Some(false) => extra.push("--no-gpu".into()),
        None => {}
    }
    if let Some(device) = opt_str(args, "device")? {
        extra.extend(["--device".into(), device.to_string()]);
    }
    Ok(())
}

/// Translate the optional `no_stale_check: bool` MCP arg into the CLI
/// `--no-stale-check` flag. Defaults to `false` (i.e. stale check runs)
/// so existing clients see no behavior change. Note: when `auto_update`
/// is also true the CLI already refreshes the index, making this flag
/// redundant; we still forward it because the CLI accepts the
/// combination and the precedence is the CLI's call to make.
pub(crate) fn push_no_stale_check(extra: &mut Vec<String>, args: &Value) -> Result<()> {
    if opt_bool(args, "no_stale_check", false)? {
        extra.push("--no-stale-check".into());
    }
    Ok(())
}

/// Translate the diff-scope MCP args (`since` / `since_branched` /
/// `changed_only`) into the matching CLI flags. The three are mutually
/// exclusive on the CLI side (clap `conflicts_with_all`); we surface
/// the conflict as an MCP-layer error so the agent gets an intent-aware
/// message rather than clap's templated output. Empirical anchor: same
/// "diff-scoped query" pattern that rtk-ai reports cuts PR-review token
/// spend by ~75%.
pub(crate) fn push_diff_scope(extra: &mut Vec<String>, args: &Value) -> Result<()> {
    let since = opt_str(args, "since")?;
    let since_branched = opt_bool(args, "since_branched", false)?;
    let changed_only = opt_bool(args, "changed_only", false)?;

    let active = [since.is_some(), since_branched, changed_only]
        .into_iter()
        .filter(|b| *b)
        .count();
    if active > 1 {
        return Err(ParamError(
            "`since`, `since_branched`, and `changed_only` are mutually exclusive".into(),
        )
        .into());
    }

    if let Some(rev) = since {
        extra.extend(["--since".into(), rev.to_string()]);
    } else if since_branched {
        extra.push("--since-branched".into());
    } else if changed_only {
        extra.push("--changed-only".into());
    }
    Ok(())
}

/// Translate the Phase 13.3 `show` truncation MCP args
/// (`signature_only` / `head` / `no_body` / `collapsed`) into the
/// matching CLI flags. The four are mutually exclusive on the CLI
/// side (clap `conflicts_with_all`); we surface the conflict as an
/// MCP-layer error so the agent sees a clear message rather than
/// clap's templated output.
pub(crate) fn push_show_truncate(extra: &mut Vec<String>, args: &Value) -> Result<()> {
    let signature_only = opt_bool(args, "signature_only", false)?;
    // Parse `head` strictly: silently accepting `head: 0`, `head: -1`, or
    // `head: 5.5` would let invalid input through. Pre-H8 we already
    // failed loudly on wrong type; post-H8 wrong type comes from the
    // shared `opt_u64` helper (which emits a ParamError → -32602), and
    // `head: 0` is still rejected here as a value-domain check.
    let head_raw = &args["head"];
    let head: Option<u64> = if head_raw.is_null() {
        None
    } else {
        let n = opt_u64(args, "head", 0)?;
        if n == 0 {
            return Err(
                ParamError("`head` must be a positive integer (got: 0)".to_string()).into(),
            );
        }
        Some(n)
    };
    let no_body = opt_bool(args, "no_body", false)?;
    let collapsed = opt_bool(args, "collapsed", false)?;

    let active = [signature_only, head.is_some(), no_body, collapsed]
        .into_iter()
        .filter(|b| *b)
        .count();
    if active > 1 {
        return Err(ParamError(
            "`signature_only`, `head`, `no_body`, and `collapsed` are mutually exclusive".into(),
        )
        .into());
    }

    if signature_only {
        extra.push("--signature-only".into());
    } else if let Some(n) = head {
        extra.extend(["--head".into(), n.to_string()]);
    } else if no_body {
        extra.push("--no-body".into());
    } else if collapsed {
        extra.push("--collapsed".into());
    }
    Ok(())
}

/// Push the `kind: string[]` MCP arg as one `--kind <value>` pair per
/// element. Mirrors `push_scope_field`. Mirrors clap's repeatable
/// `Vec<String>` accumulator on the CLI side.
pub(crate) fn push_kind(extra: &mut Vec<String>, args: &Value) -> Result<()> {
    let Some(items) = opt_str_array(args, "kind")? else {
        return Ok(());
    };
    for s in items {
        extra.extend(["--kind".into(), s.to_string()]);
    }
    Ok(())
}

/// Pull `include: string[]` and `exclude: string[]` off the JSON-RPC args
/// and append them as repeated `--include` / `--exclude` flags. Mirrors
/// the CLI scope filter and shares the same gitignore-style glob syntax.
/// Non-array or missing values are silently ignored so agents that emit
/// the field as `null`/`""` don't fail; non-string elements inside an
/// otherwise valid array are logged at warn — silently dropping them was
/// hiding the fact that a filter never engaged.
pub(crate) fn push_scope(extra: &mut Vec<String>, args: &Value) -> Result<()> {
    push_scope_field(extra, args, "include", "--include")?;
    push_scope_field(extra, args, "exclude", "--exclude")?;
    Ok(())
}

fn push_scope_field(extra: &mut Vec<String>, args: &Value, key: &str, flag: &str) -> Result<()> {
    let Some(items) = opt_str_array(args, key)? else {
        return Ok(());
    };
    for s in items {
        extra.extend([flag.into(), s.to_string()]);
    }
    Ok(())
}

/// Translate MCP metadata fields (visibility / async / static /
/// sealed) into the matching CLI flags. 11.6.
pub(crate) fn push_metadata(extra: &mut Vec<String>, args: &Value) -> Result<()> {
    let async_only = opt_bool(args, "async_only", false)?;
    let no_async = opt_bool(args, "no_async", false)?;
    let static_only = opt_bool(args, "static_only", false)?;
    let sealed_only = opt_bool(args, "sealed_only", false)?;
    // Early-bail on the mutually-exclusive pair so the caller sees an
    // intent-aware JSON-RPC error instead of clap's parser dumping
    // its `conflicts_with` template into the response body.
    if async_only && no_async {
        return Err(ParamError("`async_only` and `no_async` are mutually exclusive".into()).into());
    }
    if let Some(vis) = opt_str(args, "visibility")? {
        extra.extend(["--visibility".into(), vis.to_string()]);
    }
    if async_only {
        extra.push("--async-only".into());
    }
    if no_async {
        extra.push("--no-async".into());
    }
    if static_only {
        extra.push("--static-only".into());
    }
    if sealed_only {
        extra.push("--sealed-only".into());
    }
    Ok(())
}
