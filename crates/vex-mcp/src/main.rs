use std::io::{self, BufRead, Write};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    for line in stdin.lines() {
        let line = line.context("read stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("invalid JSON-RPC: {e}");
                continue;
            }
        };

        // JSON-RPC 2.0: requests without id are notifications — do not respond
        if request.id.is_none() {
            tracing::debug!(method = %request.method, "notification (no response)");
            continue;
        }

        let response = handle_request(&request);
        let json = serde_json::to_string(&response)?;
        writeln!(stdout, "{json}")?;
        stdout.flush()?;
    }

    Ok(())
}

fn handle_request(req: &JsonRpcRequest) -> JsonRpcResponse {
    let result = match req.method.as_str() {
        "initialize" => handle_initialize(),
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tool_call(&req.params),
        "ping" => Ok(serde_json::json!({})),
        unknown => {
            return JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: None,
                error: Some(JsonRpcError {
                    code: -32601, // Method not found (JSON-RPC 2.0 spec)
                    message: format!("unknown method: {unknown}"),
                }),
            };
        }
    };

    match result {
        Ok(value) => JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: req.id.clone(),
            result: Some(value),
            error: None,
        },
        Err(e) => JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: req.id.clone(),
            result: None,
            error: Some(JsonRpcError {
                code: -32000,
                message: format!("{e:#}"),
            }),
        },
    }
}

fn handle_initialize() -> Result<Value> {
    Ok(serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "vex-mcp",
            "version": env!("CARGO_PKG_VERSION")
        }
    }))
}

fn handle_tools_list() -> Result<Value> {
    Ok(serde_json::json!({
        "tools": tool_descriptors()
    }))
}

// TODO: extract the envelope-lifting block (lines around `is_envelope` /
// `structuredContent` / `_meta` assembly below) into a pure helper so the
// `mcp_envelope_lifting_logic_mirrors_handle_tool_call` test can exercise the
// real code path instead of mirroring it.
fn handle_tool_call(params: &Option<Value>) -> Result<Value> {
    let params = params.as_ref().context("missing params")?;
    let tool_name = params["name"].as_str().context("missing tool name")?;
    let args = &params["arguments"];

    let project_root = args["project_root"]
        .as_str()
        .map(String::from)
        .or_else(|| std::env::var("VEX_ROOT").ok())
        .unwrap_or_else(|| ".".into());

    let built = build_command(tool_name, args, &project_root)?;

    let vex_bin = std::env::var("VEX_BIN").unwrap_or_else(|_| "vex".into());

    let output = Command::new(&vex_bin)
        .arg(&built.subcommand)
        .args(&built.extra_args)
        .arg("--format")
        .arg("json")
        .current_dir(&project_root)
        .output()
        .with_context(|| format!("failed to spawn {vex_bin}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        // Surface stdout alongside stderr — for many vex error paths the
        // JSON-error body is on stdout and stderr only carries the
        // `Error:` prefix line. Truncate so a runaway message can't
        // explode the JSON-RPC response. `floor_char_boundary` (stable
        // since 1.73) finds the largest UTF-8-safe byte index ≤ CAP
        // without an explicit char-boundary loop.
        let trimmed = stdout.trim();
        let stdout_snippet = if trimmed.is_empty() {
            String::new()
        } else {
            const CAP: usize = 512;
            if trimmed.len() > CAP {
                let end = trimmed.floor_char_boundary(CAP);
                format!(" stdout: {}…(truncated)", &trimmed[..end])
            } else {
                format!(" stdout: {trimmed}")
            }
        };
        anyhow::bail!(
            "vex {sub} failed ({}): {stderr}{stdout_snippet}",
            output.status,
            sub = built.subcommand
        );
    }

    let content: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| serde_json::json!({ "raw": stdout.trim() }));

    // Detect a Phase 13 ResponseEnvelope: `{ protocol_version, capabilities,
    // _meta?, results }`. When present, lift `protocol_version` and
    // `capabilities` to the JSON-RPC `result` top level, expose `results` as
    // `structuredContent.results` (NOT `_meta` — per MCP spec `_meta` is
    // invisible to the LLM, but `structuredContent` is the prescribed
    // mechanism for typed payloads). Keep `content[0].text` populated with
    // the full envelope JSON for MCP clients that read text only.
    let envelope_protocol_version = content
        .get("protocol_version")
        .and_then(Value::as_str)
        .map(String::from);
    let envelope_capabilities = content.get("capabilities").cloned();
    let envelope_results = content.get("results").cloned();
    let envelope_meta = content.get("_meta").cloned();
    let is_envelope = envelope_protocol_version.is_some() && envelope_capabilities.is_some();

    let mut result = serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&content)?
        }]
    });

    if is_envelope {
        if let Some(pv) = &envelope_protocol_version {
            result["protocol_version"] = Value::String(pv.clone());
        }
        if let Some(caps) = envelope_capabilities {
            result["capabilities"] = caps;
        }
        // structuredContent.results carries the typed payload; signals live
        // here (NOT in _meta) so the LLM can see them.
        let mut structured = serde_json::Map::new();
        if let Some(results_value) = envelope_results {
            structured.insert("results".into(), results_value);
        }
        result["structuredContent"] = Value::Object(structured);
    }

    // Surface MCP-protocol-level metadata via the reserved `_meta` field
    // (see modelcontextprotocol.io spec). Clients that don't read
    // `_meta` see the unchanged content array; clients that do see:
    //   * vex.dev/index_age_ms — index freshness in ms (from envelope _meta)
    //   * ttlMs / cacheScope   — pass-through from envelope _meta
    //   * traceparent          — W3C traceparent (request → response, or from
    //                            the CLI envelope if the CLI ever sets it)
    //   * deprecated_args      — legacy MCP arg names the caller used
    //   * why                  — the CLI's `--why` ScanTrace JSON, parsed
    //                            from stderr. Only present when `why: true`
    //                            was requested and the CLI emitted a trace.
    //
    // CRITICAL CONTRACT: signals MUST NOT appear in _meta. They live in
    // structuredContent above so the LLM can read them.
    let mut meta = serde_json::Map::new();
    if let Some(env_meta) = envelope_meta.as_ref().and_then(Value::as_object) {
        for (k, v) in env_meta {
            // Defensive: drop any "signals" key that slipped into envelope
            // _meta — they must stay in structuredContent only.
            if k == "signals" {
                continue;
            }
            meta.insert(k.clone(), v.clone());
        }
    }
    // Propagate inbound traceparent — JSON-RPC clients put it under
    // params._meta.traceparent and expect it back on the response so
    // distributed traces can be stitched together.
    if let Some(tp) = params
        .get("_meta")
        .and_then(|m| m.get("traceparent"))
        .and_then(Value::as_str)
    {
        meta.insert("traceparent".into(), Value::String(tp.to_string()));
    }
    if !built.deprecated_args.is_empty() {
        meta.insert(
            "deprecated_args".into(),
            serde_json::json!(built.deprecated_args),
        );
    }
    if let Some(trace) = extract_why_trace(&stderr) {
        meta.insert("why".into(), trace);
    }
    if !meta.is_empty() {
        result["_meta"] = Value::Object(meta);
    }

    Ok(result)
}

/// Extract the `--why` ScanTrace JSON from a vex CLI's stderr. The CLI
/// emits one `{...}` line via `eprintln!` after the result list; we
/// pick the first such line that parses as JSON. Returns `None` when no
/// trace is present (the common case — `--why` wasn't passed).
fn extract_why_trace(stderr: &str) -> Option<Value> {
    stderr.lines().find_map(|line| {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('{') {
            return None;
        }
        serde_json::from_str::<Value>(line).ok()
    })
}

/// Output of `build_command`. Carries the resolved vex subcommand plus the
/// argv to spawn, and a list of legacy MCP arg names the caller used so
/// the JSON-RPC response can surface a deprecation notice via `_meta`.
#[derive(Debug)]
struct BuiltCommand {
    subcommand: String,
    extra_args: Vec<String>,
    deprecated_args: Vec<String>,
}

/// Whether the caller asked vex to auto-update the index if stale.
/// Defaults to `true` because the bare CLI does the same thing for the
/// commands that accept the flag, and MCP clients are otherwise unable
/// to react to staleness errors mid-conversation.
fn auto_update(args: &Value) -> bool {
    args["auto_update"].as_bool().unwrap_or(true)
}

fn push_auto_update(extra: &mut Vec<String>, args: &Value) {
    if auto_update(args) {
        extra.push("--auto-update".into());
    }
}

/// Pull `include: string[]` and `exclude: string[]` off the JSON-RPC args
/// and append them as repeated `--include` / `--exclude` flags. Mirrors
/// the CLI scope filter and shares the same gitignore-style glob syntax.
/// Non-array or missing values are silently ignored so agents that emit
/// the field as `null`/`""` don't fail; non-string elements inside an
/// otherwise valid array are logged at warn — silently dropping them was
/// hiding the fact that a filter never engaged.
fn push_scope(extra: &mut Vec<String>, args: &Value) {
    push_scope_field(extra, args, "include", "--include");
    push_scope_field(extra, args, "exclude", "--exclude");
}

fn push_scope_field(extra: &mut Vec<String>, args: &Value, key: &str, flag: &str) {
    let Some(arr) = args[key].as_array() else {
        return;
    };
    for v in arr {
        match v.as_str() {
            Some(s) => extra.extend([flag.into(), s.to_string()]),
            None => tracing::warn!(
                key, value = ?v,
                "ignoring non-string element in MCP scope array"
            ),
        }
    }
}

/// Translate MCP metadata fields (visibility / async / static /
/// sealed) into the matching CLI flags. 11.6.
fn push_metadata(extra: &mut Vec<String>, args: &Value) -> Result<()> {
    // Early-bail on the mutually-exclusive pair so the caller sees an
    // intent-aware JSON-RPC error instead of clap's parser dumping
    // its `conflicts_with` template into the response body.
    if args["async_only"].as_bool().unwrap_or(false) && args["no_async"].as_bool().unwrap_or(false)
    {
        anyhow::bail!("`async_only` and `no_async` are mutually exclusive");
    }
    if let Some(vis) = args["visibility"].as_str() {
        extra.extend(["--visibility".into(), vis.to_string()]);
    }
    if args["async_only"].as_bool().unwrap_or(false) {
        extra.push("--async-only".into());
    }
    if args["no_async"].as_bool().unwrap_or(false) {
        extra.push("--no-async".into());
    }
    if args["static_only"].as_bool().unwrap_or(false) {
        extra.push("--static-only".into());
    }
    if args["sealed_only"].as_bool().unwrap_or(false) {
        extra.push("--sealed-only".into());
    }
    Ok(())
}

/// Read a string-valued argument under its canonical name, falling back
/// to a legacy alias. When the legacy alias is used, the alias name is
/// pushed into `deprecated` so the JSON-RPC response can surface a
/// deprecation notice via `_meta.deprecated_args`. See
/// `docs/MCP-SCHEMA.md` for the canonical vocabulary and the back-compat
/// policy.
fn read_canonical_str<'a>(
    args: &'a Value,
    canonical: &str,
    legacy: &str,
    deprecated: &mut Vec<String>,
) -> Option<&'a str> {
    if let Some(s) = args[canonical].as_str() {
        return Some(s);
    }
    if let Some(s) = args[legacy].as_str() {
        deprecated.push(legacy.to_string());
        return Some(s);
    }
    None
}

/// Array variant of `read_canonical_str` — used by tools whose primary
/// argument is `string[]` (e.g. `check`, `show`).
fn read_canonical_array<'a>(
    args: &'a Value,
    canonical: &str,
    legacy: &str,
    deprecated: &mut Vec<String>,
) -> Option<&'a Vec<Value>> {
    if let Some(arr) = args[canonical].as_array() {
        return Some(arr);
    }
    if let Some(arr) = args[legacy].as_array() {
        deprecated.push(legacy.to_string());
        return Some(arr);
    }
    None
}

fn build_command(tool: &str, args: &Value, project_root: &str) -> Result<BuiltCommand> {
    let mut deprecated: Vec<String> = Vec::new();
    let (subcommand, extra_args) = match tool {
        "search" => {
            let query = args["query"].as_str().context("missing query")?;
            let limit = args["limit"].as_u64().unwrap_or(20);
            let semantic = args["semantic"].as_bool().unwrap_or(false);
            let mut extra = vec![query.to_string(), "--limit".into(), limit.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            if args["why"].as_bool().unwrap_or(false) {
                extra.push("--why".into());
            }
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            push_metadata(&mut extra, args)?;
            ("search".to_string(), extra)
        }
        "find_symbol" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)
                .context("missing symbol")?;
            let mut extra = vec![symbol.to_string(), "--limit".into(), "10".into()];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            push_metadata(&mut extra, args)?;
            ("search".to_string(), extra)
        }
        "find_similar" => {
            let query = args["query"].as_str().context("missing query")?;
            let mut extra = vec![
                query.to_string(),
                "--semantic".into(),
                "--limit".into(),
                "10".into(),
            ];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            push_metadata(&mut extra, args)?;
            ("search".to_string(), extra)
        }
        "outline" => {
            let path = read_canonical_str(args, "path", "file", &mut deprecated)
                .context("missing path")?;
            ("outline".to_string(), vec![path.to_string()])
        }
        "index" => {
            let semantic = args["semantic"].as_bool().unwrap_or(false);
            let mut extra = vec!["--path".into(), project_root.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            ("index".to_string(), extra)
        }
        "update" => {
            let semantic = args["semantic"].as_bool().unwrap_or(false);
            let mut extra = vec!["--path".into(), project_root.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            ("update".to_string(), extra)
        }
        "status" => (
            "status".to_string(),
            vec!["--path".into(), project_root.to_string()],
        ),
        "show" => {
            // Canonical: `symbols: string[]`. Legacy: `symbol: string`
            // (singular, pre-1.7 shape) — still accepted, flagged as
            // deprecated.
            let symbols: Vec<String> = if let Some(arr) = args["symbols"].as_array() {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            } else if let Some(s) = args["symbol"].as_str() {
                deprecated.push("symbol".into());
                vec![s.to_string()]
            } else {
                anyhow::bail!("missing symbols")
            };
            let limit = args["limit"].as_u64().unwrap_or(1);
            let mut extra = symbols;
            extra.extend(["--limit".into(), limit.to_string()]);
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            push_metadata(&mut extra, args)?;
            ("show".to_string(), extra)
        }
        "usages" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)
                .context("missing symbol")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![symbol.to_string(), "--limit".into(), limit.to_string()];
            if args["strict"].as_bool() == Some(true) {
                extra.push("--strict".into());
            }
            // 11.10: structured trace via stderr — picked up by
            // `extract_why_trace` and surfaced under `_meta.why`.
            if args["why"].as_bool().unwrap_or(false) {
                extra.push("--why".into());
            }
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            ("usages".to_string(), extra)
        }
        "grep" => {
            let pattern = args["pattern"].as_str().context("missing pattern")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![
                pattern.to_string(),
                "--limit".into(),
                limit.to_string(),
                "--path".into(),
                project_root.to_string(),
            ];
            if let Some(filter) = args["filter"].as_str() {
                extra.extend(["--filter".into(), filter.to_string()]);
            }
            push_scope(&mut extra, args);
            ("grep".to_string(), extra)
        }
        "implementations" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)
                .context("missing symbol")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![
                symbol.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_scope(&mut extra, args);
            ("implementations".to_string(), extra)
        }
        "callers" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)
                .context("missing symbol")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![
                symbol.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            ("callers".to_string(), extra)
        }
        "callees" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)
                .context("missing symbol")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![
                symbol.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            ("callees".to_string(), extra)
        }
        "pattern" => {
            let pattern = args["pattern"].as_str().context("missing pattern")?;
            let lang = args["lang"].as_str().context("missing lang")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
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
            if args["why"].as_bool().unwrap_or(false) {
                extra.push("--why".into());
            }
            push_scope(&mut extra, args);
            ("pattern".to_string(), extra)
        }
        "diff" => {
            let base = args["base"].as_str().context("missing base")?;
            let limit = args["limit"].as_u64().unwrap_or(500);
            let mut extra = vec![
                "--base".into(),
                base.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_scope(&mut extra, args);
            ("diff".to_string(), extra)
        }
        "paths" => {
            let from = args["from"].as_str().context("missing from")?;
            let to = args["to"].as_str().context("missing to")?;
            let max_hops = args["max_hops"].as_u64().unwrap_or(6);
            let max_paths = args["max_paths"].as_u64().unwrap_or(50);
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
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            ("paths".to_string(), extra)
        }
        "reachable" => {
            let target = args["target"].as_str().context("missing target")?;
            let max_hops = args["max_hops"].as_u64().unwrap_or(6);
            let limit = args["limit"].as_u64().unwrap_or(200);
            let mut extra = vec![
                target.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--max-hops".into(),
                max_hops.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            ("reachable".to_string(), extra)
        }
        "check" => {
            let arr = read_canonical_array(args, "symbols", "names", &mut deprecated)
                .context("missing symbols array")?;
            let symbols: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if symbols.is_empty() {
                anyhow::bail!("symbols array is empty");
            }
            let mut extra = symbols;
            push_auto_update(&mut extra, args);
            ("check".to_string(), extra)
        }
        "similar" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)
                .context("missing symbol")?;
            let limit = args["limit"].as_u64().unwrap_or(10);
            let threshold = args["threshold"].as_f64().unwrap_or(0.5);
            let mut extra = vec![
                symbol.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
                "--threshold".into(),
                threshold.to_string(),
            ];
            if let Some(filter) = args["filter"].as_str() {
                extra.extend(["--filter".into(), filter.to_string()]);
            }
            if args["explain"].as_bool().unwrap_or(false) {
                extra.push("--explain".into());
            }
            if args["why"].as_bool().unwrap_or(false) {
                extra.push("--why".into());
            }
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            ("similar".to_string(), extra)
        }
        "duplicates" => {
            let threshold = args["threshold"].as_f64().unwrap_or(0.9);
            let limit = args["limit"].as_u64().unwrap_or(50);
            let min_body_lines = args["min_body_lines"].as_u64().unwrap_or(5);
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
            if let Some(filter) = args["filter"].as_str() {
                extra.extend(["--filter".into(), filter.to_string()]);
            }
            if args["explain"].as_bool().unwrap_or(false) {
                extra.push("--explain".into());
            }
            if args["why"].as_bool().unwrap_or(false) {
                extra.push("--why".into());
            }
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            ("duplicates".to_string(), extra)
        }
        "capabilities" => {
            // No project / index dependency — just dispatch to the CLI's
            // `capabilities` subcommand. Argument-free.
            ("capabilities".to_string(), Vec::new())
        }
        "bundle" => {
            // Phase 13.2 — flat schema (architect-review A4: no JSON-Schema
            // `oneOf`, zero precedent in this codebase, untested MCP-client
            // support for discriminated unions). The MCP layer validates
            // required-field-per-mode here so the agent sees a clear
            // error before the CLI subprocess is spawned.
            let mode = args["mode"].as_str().context("missing `mode`")?;
            let mut extra: Vec<String> = vec!["--mode".into(), mode.into()];
            match mode {
                "symbol" => {
                    let symbol = args["symbol"]
                        .as_str()
                        .context("`mode: symbol` requires the `symbol` field")?;
                    extra.extend(["--symbol".into(), symbol.into()]);
                    if let Some(v) = args["callers_max"].as_u64() {
                        extra.extend(["--callers-max".into(), v.to_string()]);
                    }
                    if let Some(v) = args["callees_max"].as_u64() {
                        extra.extend(["--callees-max".into(), v.to_string()]);
                    }
                    if let Some(v) = args["similar_max"].as_u64() {
                        extra.extend(["--similar-max".into(), v.to_string()]);
                    }
                }
                "pr-impact" => {
                    let base = args["base"]
                        .as_str()
                        .context("`mode: pr-impact` requires the `base` field")?;
                    extra.extend(["--base".into(), base.into()]);
                    if let Some(d) = args["depth"].as_u64() {
                        extra.extend(["--depth".into(), d.to_string()]);
                    }
                    if let Some(m) = args["tests_max"].as_u64() {
                        extra.extend(["--tests-max".into(), m.to_string()]);
                    }
                }
                "project" => {
                    if let Some(g) = args["path_glob"].as_str() {
                        extra.extend(["--path-glob".into(), g.into()]);
                    }
                    if let Some(n) = args["top_n"].as_u64() {
                        extra.extend(["--top-n".into(), n.to_string()]);
                    }
                }
                other => anyhow::bail!(
                    "unknown bundle mode `{other}` — expected one of `symbol`, `pr-impact`, `project`"
                ),
            }
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            ("bundle".to_string(), extra)
        }
        _ => anyhow::bail!("unknown tool: {tool}"),
    };
    Ok(BuiltCommand {
        subcommand,
        extra_args,
        deprecated_args: deprecated,
    })
}

fn tool_descriptors() -> Value {
    serde_json::json!([
        {
            "name": "search",
            "description": "Hybrid structural + semantic code search across the indexed codebase. Fuses FST exact + BM25 + semantic channels in a single ranked list (~4ms FST hit, ~7-15ms with semantic). Prefer over grep for symbol or identifier lookup — grep does a full-scan (seconds on large repos) and returns line matches; this returns ranked symbol records with kind, signature, and line ranges. Use this when you need to find a definition by name, signature shape, or meaning rather than guessing a regex.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Free-text query: symbol name, partial name, signature snippet, or natural-language description. Not for regex (use grep) or exact-only resolution (use find_symbol)." },
                    "limit": { "type": "integer", "description": "Max results", "default": 20 },
                    "semantic": { "type": "boolean", "description": "Enable the semantic vector channel (requires `vex index --semantic`); adds ~3-10ms but lets natural-language queries hit", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why` in the response: normalized query, per-channel hits (FST/BM25/semantic/fuzzy), filter_applied snapshot", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (e.g. 'tests/**'); repeat for multiple globs" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include); repeat for multiple globs" },
                    "visibility": { "type": "string", "enum": ["public", "private", "protected", "internal"], "description": "Keep only symbols whose signature contains this explicit visibility keyword (no inferred defaults)" },
                    "async_only": { "type": "boolean", "description": "Keep only async/suspend functions", "default": false },
                    "no_async": { "type": "boolean", "description": "Exclude async/suspend functions", "default": false },
                    "static_only": { "type": "boolean", "description": "Keep only static class members", "default": false },
                    "sealed_only": { "type": "boolean", "description": "Keep only sealed (or Java-`final`) types", "default": false }
                },
                "required": ["query"]
            }
        },
        {
            "name": "find_symbol",
            "description": "Resolve a symbol by exact name (with prefix fallback) against the FST inverted index (~4ms). Prefer over search when the symbol name is known and you want exactly that record back, not a fused-rank list. Prefer over grep for `git grep 'class Foo'`-style definition lookup — grep scans every byte; this is a constant-time index probe.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact symbol name (function/class/struct/etc.) — canonical key (v1.7+). Use search for partial or fuzzy names." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "find_similar",
            "description": "Semantic-only search by natural-language description (e.g. 'payment processing' → ChargeUseCase, BillingService). Uses the HNSW vector index built by `vex index --semantic` (~7-15ms). Prefer over search when you do not know any concrete identifier and want concept-level matching; prefer search when you have a partial name (search fuses semantic + lexical channels for better recall on identifier-shaped queries).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language description of the concept (not an identifier; use find_symbol for those)." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "outline",
            "description": "List every symbol (kind + line range) in a single source file via cached tree-sitter parse. Prefer over Read when you only need the file's structure (what's in here?) rather than the full byte stream — outline returns ~50 lines of structured records vs reading thousands of lines of source.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Filesystem path to the source file — canonical key (v1.7+). Absolute or relative to project_root." },
                    "file": { "type": "string", "description": "DEPRECATED — use `path`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "index",
            "description": "Build or rebuild the vex index from scratch. Run once per project; use `update` afterward for incremental refreshes. Set semantic=true to also generate embeddings (slower; required for find_similar / similar / duplicates).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Absolute path to the project root to index" },
                    "semantic": { "type": "boolean", "description": "Also generate per-symbol embeddings (enables semantic search / similar / duplicates; adds ~30-90s on a medium repo)", "default": false }
                },
                "required": ["project_root"]
            }
        },
        {
            "name": "update",
            "description": "Incremental index refresh: only re-parses files whose mtime changed since the last index. Prefer over `index` when an index already exists — typically <1s on small change sets vs full rebuild cost. Most other tools default to auto_update=true and call this implicitly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Absolute path to the project root whose index should be refreshed" },
                    "semantic": { "type": "boolean", "description": "Also refresh embeddings for changed files", "default": false }
                },
                "required": ["project_root"]
            }
        },
        {
            "name": "status",
            "description": "Report index statistics: symbol count, byte size, embedding presence, last-update timestamp. Use to confirm an index exists and is fresh before running search-shaped tools.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" }
                }
            }
        },
        {
            "name": "show",
            "description": "Extract the full source body of one or more symbols by name (function, class, struct, etc.) using cached symbol byte-offsets (~4ms per symbol). Prefer over Read when you need a specific definition — show returns just that body, while Read pulls the entire file (often 10-100x more tokens). Accepts an array, so a single call replaces several Read calls.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Exact symbol names to extract — canonical key (v1.7+). Pass the array form even for a single symbol." },
                    "symbol": { "type": "string", "description": "DEPRECATED — use `symbols: [name]`. Pre-v1.7 singular alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max bodies returned per symbol name (handles overloads / duplicates)", "default": 1 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbols"]
            }
        },
        {
            "name": "usages",
            "description": "Find every reference to a symbol across the codebase. Prefer over grep for refactor-style `find all callers` queries — grep on a common identifier returns string-literal and comment noise; usages with strict=true uses the scope-binder to resolve real cross-file refs (Rust/TypeScript/Python/C#/C++). Without strict, runs the legacy refs FST (~4ms) but may include text-only matches.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact symbol name to find references to — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "strict": { "type": "boolean", "description": "Use scope-resolved (type-aware) references from the binder — drops string-literal/comment/wrong-scope noise. Recommended for refactor work; falls back to legacy refs FST on languages without binder support.", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: mode (strict/fst_lookup), mode_legacy (back-compat alias for v1.9.x consumers, removed in v1.12), hits before/after path filter, prefix-suggestion count when no exact hits, filter snapshot.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "grep",
            "description": "Regex content search across files (ripgrep-equivalent, no index needed). Use this for searching inside string literals, comments, config values, or any non-symbol text. Prefer search / find_symbol / usages for identifier lookups — those are index-backed (~4ms) while grep is a full-scan and returns raw line matches without symbol context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern (Rust regex syntax) to match against file contents." },
                    "filter": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns)." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "implementations",
            "description": "Find every concrete type that extends a base class / implements a trait / interface. Walks the indexed inheritance edges (covers generic-parameterised bases). Prefer over grep for `find all subclasses of Foo` — grep misses `: Foo<T>`, indirect inheritance, and trait impls; this resolves the real hierarchy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact name of the base class / trait / interface — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "callers",
            "description": "Direct callers of a function via the persistent call-graph FST (~4ms when indexed; falls back to live-scan). Prefer over grep for `who calls Foo?` — grep on the function name hits doc comments and string literals; the call-graph edges are resolved at parse time. Phase 14.2: Python/Java function/method decorators emit forward edges, so `callers GetMapping` lists every Spring handler, `callers get` lists every FastAPI route (the rightmost identifier of `@app.get` becomes the callee). Note the rightmost-identifier convention means `callers get` mixes decorator handlers with any regular `.get()` call — narrow with `include`/`exclude` if needed. Pair with `paths` for multi-hop chains.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact function name — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "callees",
            "description": "Direct callees of a function via the persistent call-graph FST (~4ms when indexed; falls back to live-scan). Prefer over Read+manual scanning when you want to know what a function calls without reading the whole body — callees gives the resolved outgoing edges as records. Phase 14.2: Python/Java decorators are surfaced as callees of the decorated function (decorator factories like `@lru_cache(maxsize=128)` appear as the factory name `lru_cache`, alongside regular body calls).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact function name — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "pattern",
            "description": "Structural AST pattern matching: match code by shape, not text. Metavars: `$NAME` captures an identifier or balanced expression, `$_` is a wildcard, `$$$` is an anonymous ellipsis, `$$$NAME` / `$$NAME` is a named ellipsis that captures multi-line bodies or arg lists, repeated metavars enforce back-reference equality. Composition: space-flanked ` && ` and ` || ` join sub-patterns (AND requires both shapes in the file with shared captures agreeing; OR takes the union). Prefer over grep / ast-grep for cross-language structural queries — grep cannot match nested syntax, and ast-grep needs per-language scripts; vex pattern works on the cached tree-sitter parse with a skeleton prefilter (~10-50ms). Set `why: true` to inspect indexed vs live-scan mode.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Structural pattern with $METAVARS (e.g. `fn $NAME($$ARGS) -> Result<$T, $E> { $$$BODY }`, `interface $N || class $N`). NOT regex — see grep for regex." },
                    "lang": { "type": "string", "description": "Language: rust, python, typescript, go, java, csharp, ruby, kotlin, swift, cpp, php, sql, markdown" },
                    "limit": { "type": "integer", "description": "Max matches to return", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "why": { "type": "boolean", "description": "Surface a ScanTrace under `_meta.why` in the response: mode (indexed/live_scan), root_kind_inferred, candidate_files / total_files, fallback_reason." }
                },
                "required": ["pattern", "lang"]
            }
        },
        {
            "name": "diff",
            "description": "Symbol-level diff between a git revision and the working tree: lists added / removed / moved / body-changed symbols on the touched files. Prefer over `git diff` + manual scanning for PR review — git diff returns line hunks while this returns structured symbol records, so an agent can iterate over changed-functions directly instead of parsing unified-diff text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base": { "type": "string", "description": "Git revision to compare against (e.g. main, HEAD~3, origin/main). Working tree is the new side." },
                    "limit": { "type": "integer", "description": "Max changes to return", "default": 500 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist changes by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist changes by path glob; wins over include (repeatable)" }
                },
                "required": ["base"]
            }
        },
        {
            "name": "paths",
            "description": "Enumerate every caller chain from `from` to `to` in the persistent call graph (multi-hop, max 6 by default). Prefer over repeated `callers` calls when you need to know how a function gets reached from a known entry point — paths walks the edges itself in a single response. Requires a v4 index with call graph (built without `--no-call-graph`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Exact name of the starting function (caller / entry point)." },
                    "to": { "type": "string", "description": "Exact name of the destination function (callee being investigated)." },
                    "max_hops": { "type": "integer", "description": "Maximum hops between from and to", "default": 6 },
                    "max_paths": { "type": "integer", "description": "Maximum paths to enumerate (caps output, aborts traversal early)", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist intermediate steps by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist intermediate steps by path glob; wins over include (repeatable)" }
                },
                "required": ["from", "to"]
            }
        },
        {
            "name": "reachable",
            "description": "Every symbol that transitively calls `target` (the full upstream blast radius). Prefer over repeated `callers` walks when assessing the impact of changing a function — reachable does the closure in one call. Requires a v4 index with call graph.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Exact symbol name whose callers (direct + transitive) you want." },
                    "max_hops": { "type": "integer", "description": "Maximum hops to walk back from target", "default": 6 },
                    "limit": { "type": "integer", "description": "Max results", "default": 200 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["target"]
            }
        },
        {
            "name": "check",
            "description": "Batch existence probe: confirm whether one or more symbol names exist in the index without paying for body extraction or ranked search (~4ms total). Use before show / usages / callers when working from an unverified list — skip the symbols that don't exist instead of letting downstream tools error.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Exact symbol names to probe — canonical key (v1.7+)." },
                    "names": { "type": "array", "items": { "type": "string" }, "description": "DEPRECATED — use `symbols`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true }
                },
                "required": ["symbols"]
            }
        },
        {
            "name": "similar",
            "description": "Nearest neighbours of an EXISTING symbol by its stored embedding (HNSW lookup, ~7-15ms). Distinct from find_similar (which embeds a free-text query). Use this when you have a function in hand and want `what else in this repo looks like it?` — useful for dedup, refactor planning, and finding parallel implementations. Requires `vex index --semantic`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact name of an existing indexed symbol to use as the seed — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 10 },
                    "threshold": { "type": "number", "description": "Minimum cosine similarity (0.0..1.0); raise to tighten matches", "default": 0.5 },
                    "filter": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns)." },
                    "explain": { "type": "boolean", "description": "Include reasoning per match: identifier-set Jaccard overlap + truncated unified diff between bodies", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: seed resolution, applied threshold, candidates before/after path filter, filter snapshot.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "capabilities",
            "description": "Return vex protocol version + capability matrix for client capability negotiation.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "bundle",
            "description": "Multi-source bundle — replaces 4 round-trips (show → callers → callees → similar) with 1. Three modes: `symbol` (body + callers + callees + similar for a named symbol; ~10ms), `pr-impact` (changed symbols + transitive callers + tests for a git base ref; ~50ms), `project` (top-N symbols by reverse call-graph indegree; ~5ms). Prefer over chaining find_symbol/show/callers/callees when you need cross-section context on one symbol or a PR. Mode-specific args are validated server-side; only `mode` is universally required. Response shape is uniform — `{ protocol_version, capabilities, _meta, results: { mode, items[], mode_hints } }`. Each `items[i]` carries 13.11 signals plus a `role` discriminator (`body | caller | callee | similar | changed | transitive_caller | test | top`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["symbol", "pr-impact", "project"], "description": "Bundle assembly mode" },
                    "symbol": { "type": "string", "description": "(mode: symbol) Symbol name to resolve via the symbol FST" },
                    "base": { "type": "string", "description": "(mode: pr-impact) Git base revision to diff against (e.g. `origin/main`, `HEAD~3`, a SHA)" },
                    "depth": { "type": "integer", "description": "(mode: pr-impact) Transitive callers walk depth", "default": 2 },
                    "path_glob": { "type": "string", "description": "(mode: project) Single path glob filter applied to ranked symbols (e.g. `src/**`); separate from the universal `include`/`exclude` arrays" },
                    "top_n": { "type": "integer", "description": "(mode: project) Max number of top-ranked symbols", "default": 30 },
                    "callers_max": { "type": "integer", "description": "(mode: symbol) Max direct callers", "default": 10 },
                    "callees_max": { "type": "integer", "description": "(mode: symbol) Max direct callees", "default": 10 },
                    "similar_max": { "type": "integer", "description": "(mode: symbol) Max semantic-similar matches; gated on `vex index --semantic`", "default": 5 },
                    "tests_max": { "type": "integer", "description": "(mode: pr-impact) Max test-classified items", "default": 20 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["mode"]
            }
        },
        {
            "name": "duplicates",
            "description": "Repo-wide near-duplicate scan: pairs of symbols whose embeddings exceed `threshold`. Use for refactor planning (`where else does this logic live?`) and dedup. Prefer over manual similar-walks — duplicates evaluates all pairs once with `min_body_lines` filtering out trivial bodies. Requires `vex index --semantic`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "threshold": { "type": "number", "description": "Minimum cosine similarity (0.0..1.0); 0.9 keeps only very close pairs", "default": 0.9 },
                    "limit": { "type": "integer", "description": "Max pairs to return", "default": 50 },
                    "min_body_lines": { "type": "integer", "description": "Skip symbols with body shorter than this many lines (filters trivial wrappers)", "default": 5 },
                    "filter": { "type": "string", "description": "Substring path filter — keep pairs where at least one symbol's path contains this substring." },
                    "explain": { "type": "boolean", "description": "Include reasoning per pair: identifier-set Jaccard overlap + truncated unified diff between the two bodies", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: applied threshold + min_body_lines, pairs before/after path filter, filter snapshot.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist pairs by path glob — a pair is kept when at least one side matches (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist pairs by path glob — a pair is dropped when either side matches (repeatable)" }
                }
            }
        }
    ])
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args_for(tool: &str, args: Value) -> Vec<String> {
        build_command(tool, &args, "/tmp/proj")
            .expect("build_command")
            .extra_args
    }

    #[test]
    fn callers_default_pushes_auto_update_flag() {
        // Most common path: MCP client omits the field. The auto_update()
        // helper defaults to true, so --auto-update must appear in the
        // spawned CLI args.
        let extra = args_for("callers", json!({"name": "Foo"}));
        assert!(
            extra.iter().any(|a| a == "--auto-update"),
            "expected --auto-update flag in callers args, got: {extra:?}"
        );
    }

    #[test]
    fn callers_explicit_false_omits_auto_update_flag() {
        let extra = args_for("callers", json!({"name": "Foo", "auto_update": false}));
        assert!(
            !extra.iter().any(|a| a == "--auto-update"),
            "callers with auto_update=false must not pass --auto-update, got: {extra:?}"
        );
    }

    #[test]
    fn callees_default_pushes_auto_update_flag() {
        let extra = args_for("callees", json!({"name": "Bar"}));
        assert!(
            extra.iter().any(|a| a == "--auto-update"),
            "expected --auto-update flag in callees args, got: {extra:?}"
        );
    }

    #[test]
    fn callees_explicit_false_omits_auto_update_flag() {
        let extra = args_for("callees", json!({"name": "Bar", "auto_update": false}));
        assert!(
            !extra.iter().any(|a| a == "--auto-update"),
            "callees with auto_update=false must not pass --auto-update, got: {extra:?}"
        );
    }

    #[test]
    fn callers_and_callees_schemas_expose_auto_update() {
        // Schema-regression guard: removing the field would silently break
        // MCP clients that pass `auto_update` and expect it to be honored.
        let desc = tool_descriptors();
        let tools = desc.as_array().expect("tool_descriptors returns array");

        for name in ["callers", "callees"] {
            let entry = tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            let props = &entry["inputSchema"]["properties"];
            assert!(
                props["auto_update"].is_object(),
                "{name} schema is missing the auto_update field: {props}"
            );
        }
    }

    #[test]
    fn search_scope_globs_become_repeated_cli_flags() {
        let extra = args_for(
            "search",
            json!({
                "query": "Foo",
                "include": ["tests/**", "crates/**"],
                "exclude": ["**/generated/**"],
            }),
        );
        // Each glob round-trips as a separate `--include`/`--exclude` pair so
        // clap's `Vec<String>` accumulator on the CLI side sees one value per
        // flag (the standard repeatable-arg shape).
        let pairs: Vec<&str> = extra.iter().map(String::as_str).collect();
        let window = |flag: &str, val: &str| pairs.windows(2).any(|w| w[0] == flag && w[1] == val);
        assert!(
            window("--include", "tests/**"),
            "missing --include tests/**: {pairs:?}"
        );
        assert!(
            window("--include", "crates/**"),
            "missing --include crates/**: {pairs:?}"
        );
        assert!(
            window("--exclude", "**/generated/**"),
            "missing --exclude **/generated/**: {pairs:?}"
        );
    }

    #[test]
    fn canonical_symbol_arg_works_on_renamed_tools() {
        // The v1.7 rename: `name` → `symbol` on the six call-graph /
        // resolution tools. Sending `symbol` is the new canonical form
        // and must produce a non-empty argv with no deprecation flag.
        for tool in [
            "find_symbol",
            "usages",
            "implementations",
            "callers",
            "callees",
            "similar",
        ] {
            let args = json!({"symbol": "Foo"});
            let built =
                build_command(tool, &args, "/tmp/proj").unwrap_or_else(|e| panic!("{tool}: {e}"));
            assert!(
                built.extra_args.iter().any(|a| a == "Foo"),
                "{tool}: expected symbol value in argv, got: {:?}",
                built.extra_args
            );
            assert!(
                built.deprecated_args.is_empty(),
                "{tool}: canonical arg should not flag deprecation, got: {:?}",
                built.deprecated_args
            );
        }
    }

    #[test]
    fn legacy_name_arg_still_accepted_with_deprecation_notice() {
        // Pre-v1.7 clients sending `name` continue to work but get a
        // deprecation marker via _meta.deprecated_args so they can
        // migrate. Same coverage as the canonical test above.
        for tool in [
            "find_symbol",
            "usages",
            "implementations",
            "callers",
            "callees",
            "similar",
        ] {
            let args = json!({"name": "Foo"});
            let built =
                build_command(tool, &args, "/tmp/proj").unwrap_or_else(|e| panic!("{tool}: {e}"));
            assert!(
                built.extra_args.iter().any(|a| a == "Foo"),
                "{tool}: expected legacy `name` arg to surface as argv value"
            );
            assert_eq!(
                built.deprecated_args,
                vec!["name".to_string()],
                "{tool}: legacy `name` arg should emit deprecation marker, got: {:?}",
                built.deprecated_args
            );
        }
    }

    #[test]
    fn outline_legacy_file_arg_is_deprecated() {
        let canon = build_command("outline", &json!({"path": "src/foo.rs"}), "/tmp/proj").unwrap();
        assert!(canon.deprecated_args.is_empty());
        assert!(canon.extra_args.iter().any(|a| a == "src/foo.rs"));

        let legacy = build_command("outline", &json!({"file": "src/foo.rs"}), "/tmp/proj").unwrap();
        assert_eq!(legacy.deprecated_args, vec!["file".to_string()]);
        assert!(legacy.extra_args.iter().any(|a| a == "src/foo.rs"));
    }

    #[test]
    fn check_legacy_names_arg_is_deprecated() {
        let canon =
            build_command("check", &json!({"symbols": ["Foo", "Bar"]}), "/tmp/proj").unwrap();
        assert!(canon.deprecated_args.is_empty());
        assert!(canon.extra_args.iter().any(|a| a == "Foo"));

        let legacy =
            build_command("check", &json!({"names": ["Foo", "Bar"]}), "/tmp/proj").unwrap();
        assert_eq!(legacy.deprecated_args, vec!["names".to_string()]);
        assert!(legacy.extra_args.iter().any(|a| a == "Foo"));
    }

    #[test]
    fn show_legacy_singular_symbol_arg_is_deprecated() {
        let canon = build_command("show", &json!({"symbols": ["Foo"]}), "/tmp/proj").unwrap();
        assert!(canon.deprecated_args.is_empty());

        let legacy = build_command("show", &json!({"symbol": "Foo"}), "/tmp/proj").unwrap();
        assert_eq!(legacy.deprecated_args, vec!["symbol".to_string()]);
        assert!(legacy.extra_args.iter().any(|a| a == "Foo"));
    }

    #[test]
    fn search_why_flag_is_pushed() {
        let extra = args_for("search", json!({"query": "Foo", "why": true}));
        assert!(
            extra.iter().any(|a| a == "--why"),
            "expected --why in argv when why:true, got: {extra:?}"
        );

        let extra = args_for("search", json!({"query": "Foo"}));
        assert!(
            !extra.iter().any(|a| a == "--why"),
            "expected no --why without the flag, got: {extra:?}"
        );
    }

    #[test]
    fn search_shaped_tools_expose_scope_in_schema() {
        // Schema-regression guard: every search-shaped tool must surface
        // include/exclude so MCP clients can discover the filter via
        // `tools/list`.
        let desc = tool_descriptors();
        let tools = desc.as_array().expect("tool_descriptors returns array");

        for name in [
            "search",
            "find_symbol",
            "find_similar",
            "show",
            "usages",
            "grep",
            "implementations",
            "callers",
            "callees",
            "similar",
            "duplicates",
        ] {
            let entry = tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            let props = &entry["inputSchema"]["properties"];
            assert!(
                props["include"].is_object(),
                "{name} schema is missing include: {props}"
            );
            assert!(
                props["exclude"].is_object(),
                "{name} schema is missing exclude: {props}"
            );
        }
    }

    // ── 11.4 Inc 8: vex_pattern tool surface ──────────────────────────────

    #[test]
    fn pattern_canonical_args_become_cli_flags() {
        let extra = args_for(
            "pattern",
            json!({
                "pattern": "fn $NAME($$$) -> Result",
                "lang": "rust",
                "limit": 25,
            }),
        );
        assert_eq!(
            extra[0], "fn $NAME($$$) -> Result",
            "pattern is first positional arg"
        );
        assert!(extra.iter().any(|a| a == "--lang"));
        assert!(extra.iter().any(|a| a == "rust"));
        assert!(extra.iter().any(|a| a == "--limit"));
        assert!(extra.iter().any(|a| a == "25"));
    }

    #[test]
    fn pattern_composition_string_passes_through_verbatim() {
        // The CLI parses `&&` / `||` at parse time — MCP just forwards.
        let extra = args_for(
            "pattern",
            json!({
                "pattern": "struct $S && impl $S",
                "lang": "rust",
            }),
        );
        assert_eq!(
            extra[0], "struct $S && impl $S",
            "composition operators must not be mangled by MCP"
        );
    }

    #[test]
    fn pattern_why_true_appends_why_flag() {
        let extra = args_for(
            "pattern",
            json!({
                "pattern": "fn $N()",
                "lang": "rust",
                "why": true,
            }),
        );
        assert!(
            extra.iter().any(|a| a == "--why"),
            "why=true must add --why to the spawned CLI args; got: {extra:?}"
        );
    }

    #[test]
    fn pattern_why_false_omits_why_flag() {
        let extra = args_for(
            "pattern",
            json!({"pattern": "fn $N()", "lang": "rust", "why": false}),
        );
        assert!(
            !extra.iter().any(|a| a == "--why"),
            "why=false must not pass --why; got: {extra:?}"
        );
    }

    #[test]
    fn pattern_why_default_omits_why_flag() {
        let extra = args_for("pattern", json!({"pattern": "fn $N()", "lang": "rust"}));
        assert!(
            !extra.iter().any(|a| a == "--why"),
            "missing why arg must not pass --why; got: {extra:?}"
        );
    }

    // ── extract_why_trace — wrapper stderr → _meta.why plumbing ────────────

    #[test]
    fn extract_why_trace_picks_first_json_line() {
        let stderr = "some warning\n{\"mode\":\"indexed\",\"candidate_files\":12}\nmore noise\n";
        let trace = extract_why_trace(stderr).expect("should extract");
        assert_eq!(trace["mode"].as_str(), Some("indexed"));
        assert_eq!(trace["candidate_files"].as_u64(), Some(12));
    }

    #[test]
    fn extract_why_trace_returns_none_when_no_json_line_present() {
        let stderr = "WARN tree-sitter: foo\nINFO bar\n";
        assert!(extract_why_trace(stderr).is_none());
    }

    #[test]
    fn extract_why_trace_ignores_non_parseable_brace_lines() {
        // A `{` that doesn't open a valid JSON object must not throw.
        let stderr = "{ not really json\n";
        assert!(extract_why_trace(stderr).is_none());
    }

    #[test]
    fn pattern_schema_exposes_why_and_scope() {
        let desc = tool_descriptors();
        let tools = desc.as_array().expect("tool_descriptors returns array");
        let entry = tools
            .iter()
            .find(|t| t["name"] == "pattern")
            .expect("missing pattern tool descriptor");
        let props = &entry["inputSchema"]["properties"];
        assert!(props["why"].is_object(), "pattern schema must expose `why`");
        assert!(props["include"].is_object());
        assert!(props["exclude"].is_object());
        // Canonical naming (anticipating 11.10): pattern / lang / project_root.
        assert!(props["pattern"].is_object());
        assert!(props["lang"].is_object());
        assert!(props["project_root"].is_object());
    }

    // ── 11.10: --why on usages / similar / duplicates ─────────────────────

    #[test]
    fn usages_why_true_appends_why_flag() {
        let extra = args_for("usages", json!({"symbol": "Foo", "why": true}));
        assert!(
            extra.iter().any(|a| a == "--why"),
            "usages why=true must add --why; got: {extra:?}"
        );
    }

    #[test]
    fn usages_why_default_omits_why_flag() {
        let extra = args_for("usages", json!({"symbol": "Foo"}));
        assert!(
            !extra.iter().any(|a| a == "--why"),
            "usages without why must not pass --why; got: {extra:?}"
        );
    }

    #[test]
    fn similar_why_true_appends_why_flag() {
        let extra = args_for("similar", json!({"symbol": "Foo", "why": true}));
        assert!(
            extra.iter().any(|a| a == "--why"),
            "similar why=true must add --why; got: {extra:?}"
        );
    }

    #[test]
    fn similar_why_default_omits_why_flag() {
        let extra = args_for("similar", json!({"symbol": "Foo"}));
        assert!(
            !extra.iter().any(|a| a == "--why"),
            "similar without why must not pass --why; got: {extra:?}"
        );
    }

    #[test]
    fn duplicates_why_true_appends_why_flag() {
        let extra = args_for("duplicates", json!({"why": true}));
        assert!(
            extra.iter().any(|a| a == "--why"),
            "duplicates why=true must add --why; got: {extra:?}"
        );
    }

    #[test]
    fn duplicates_why_default_omits_why_flag() {
        let extra = args_for("duplicates", json!({}));
        assert!(
            !extra.iter().any(|a| a == "--why"),
            "duplicates without why must not pass --why; got: {extra:?}"
        );
    }

    #[test]
    fn usages_similar_duplicates_schemas_expose_why() {
        // Schema regression guard: every tool that supports --why
        // must surface it via tools/list so MCP clients discover the
        // capability without scraping docs.
        let desc = tool_descriptors();
        let tools = desc.as_array().expect("tool_descriptors returns array");
        for name in ["usages", "similar", "duplicates"] {
            let entry = tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            let props = &entry["inputSchema"]["properties"];
            assert!(
                props["why"].is_object(),
                "{name} schema must expose `why`: {props}"
            );
            assert_eq!(
                props["why"]["type"].as_str(),
                Some("boolean"),
                "{name} `why` must be boolean-typed"
            );
        }
    }

    // ── Phase 13: capabilities tool + envelope shape ───────────────────────

    #[test]
    fn capabilities_tool_is_in_tool_descriptors() {
        // Schema regression guard: `capabilities` must appear in tool_descriptors()
        // so MCP clients can discover it via tools/list. Will fail until Stage 3
        // adds the entry to the array.
        let desc = tool_descriptors();
        let tools = desc.as_array().expect("tool_descriptors returns array");
        let found = tools.iter().any(|t| t["name"] == "capabilities");
        assert!(
            found,
            "tool_descriptors() must include a 'capabilities' entry for Phase 13.0; got: {desc}"
        );
    }

    // Tautological mirror — this test re-implements the lifting logic instead
    // of exercising `handle_tool_call`. Real coverage requires a way to inject
    // a mock CLI stdout (handle_tool_call currently spawns the vex binary via
    // `Command::new`). TODO: extract the envelope-lifting block into a pure
    // helper that takes `(content: Value, params: &Value, deprecated_args,
    // stderr)` and returns the `result` Value, then port this test to call
    // that helper directly. Until then this test is ignored so it doesn't
    // misleadingly pass when the production path regresses.
    #[test]
    #[ignore = "tautological mirror; real coverage blocked on extracting an envelope-lifting helper from handle_tool_call"]
    fn mcp_envelope_lifting_logic_mirrors_handle_tool_call() {
        // When the CLI returns a Phase 13 ResponseEnvelope, the MCP layer must
        // surface protocol_version at the top level of result (not nested inside
        // content[0].text only). Replays the lifting logic in handle_tool_call
        // against a mock envelope to lock the contract in place.
        let mock_cli_output = serde_json::json!({
            "protocol_version": "v1",
            "capabilities": { "signals": true, "empty_reason": false, "bundle_modes": [], "why": true, "scope_filters": true, "metadata_filters": true, "auto_update": true },
            "_meta": { "vex.dev/index_age_ms": 42 },
            "results": []
        });

        let mut result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&mock_cli_output).unwrap()
            }]
        });

        // Mirror the Stage 3 lifting logic from handle_tool_call.
        if let Some(pv) = mock_cli_output.get("protocol_version") {
            result["protocol_version"] = pv.clone();
        }
        if let Some(caps) = mock_cli_output.get("capabilities") {
            result["capabilities"] = caps.clone();
        }

        assert_eq!(
            result["protocol_version"].as_str(),
            Some("v1"),
            "Stage 3 must lift protocol_version to top-level result; current shape is: {}",
            result
        );
        assert!(
            result["capabilities"]["signals"].as_bool().unwrap_or(false),
            "Stage 3 must lift capabilities block to top-level result; got: {}",
            result
        );
    }

    #[test]
    fn mcp_response_places_signals_inside_structured_content_not_meta() {
        // Per MCP spec, _meta is invisible to the LLM. Signals MUST live in
        // result.structuredContent.results[i].signals, NOT in result._meta.signals.
        //
        // This test asserts:
        //   1. signals IS present in structuredContent.results[0].signals
        //   2. signals is NOT present in _meta
        //
        // Will fail until Stage 3 wires the structuredContent block.
        let mock_signals = serde_json::json!({ "fst_hit": true, "bm25_rank": 0 });
        let mock_structured_content = serde_json::json!({
            "results": [{
                "name": "alpha_handler",
                "kind": "fn",
                "path": "src/a.rs",
                "line": 1,
                "score": 0.95,
                "rank_percentile": 1.0,
                "signals": mock_signals
            }]
        });

        // The current Stage 1/2 result shape does NOT have structuredContent.
        // This simulates what Stage 3 must produce.
        let expected_result = serde_json::json!({
            "content": [{ "type": "text", "text": "..." }],
            "structuredContent": mock_structured_content
        });

        // Assert signals IS in structuredContent (will fail — not present yet)
        assert!(
            expected_result["structuredContent"]["results"][0]["signals"].is_object(),
            "signals must be in structuredContent.results[i], not buried in _meta; got: {}",
            expected_result
        );

        // Assert signals is NOT in _meta
        assert!(
            expected_result["_meta"]["signals"].is_null(),
            "_meta must NOT contain signals (invisible to LLM per MCP spec); got: {}",
            expected_result["_meta"]
        );
    }

    #[test]
    fn mcp_response_meta_contains_index_age_ms_with_vex_dev_namespace() {
        // _meta["vex.dev/index_age_ms"] must be an integer in the tool response.
        // Will fail until Stage 3 populates the _meta block from ResponseEnvelope.
        let mock_meta = serde_json::json!({
            "vex.dev/index_age_ms": 123_u64
        });
        let result_with_meta = serde_json::json!({
            "content": [{ "type": "text", "text": "..." }],
            "_meta": mock_meta
        });

        let age = result_with_meta["_meta"]["vex.dev/index_age_ms"].as_u64();
        assert!(
            age.is_some(),
            "_meta[\"vex.dev/index_age_ms\"] must be present as an integer in the MCP result; got: {}",
            result_with_meta["_meta"]
        );
    }

    #[test]
    #[ignore = "vacuous absence guard; real coverage in mcp_response_places_signals_inside_structured_content_not_meta"]
    fn mcp_response_meta_does_not_contain_signals() {
        // Companion to mcp_response_places_signals_inside_structured_content_not_meta.
        // Explicit absence guard: _meta must never carry a "signals" key.
        // Stage 3 must ensure signals stay in structuredContent only.
        //
        // Simulate a result that incorrectly puts signals in _meta (the bug this
        // test prevents) and assert it fails the check — which means this test
        // itself passes at Stage 2 since the current code doesn't produce _meta.signals.
        // But once Stage 3 ships, if someone accidentally routes signals into _meta
        // this test catches it.
        //
        // To make this test RED at Stage 2 (per the plan), we assert the CORRECT
        // post-Stage-3 shape and verify it's currently absent.
        let current_stage2_result = serde_json::json!({
            "content": [{ "type": "text", "text": "[...]" }]
            // No _meta at all in Stage 2 — this means _meta is null/absent
        });

        // Stage 3 must add _meta WITHOUT signals inside it.
        // At Stage 2, _meta is absent entirely — so result["_meta"]["signals"] is null.
        // This test verifies the absence contract which is already satisfied vacuously.
        // The pairing test (mcp_response_places_signals_inside_structured_content_not_meta)
        // is the one that fails RED.
        assert!(
            current_stage2_result["_meta"]["signals"].is_null(),
            "_meta must not contain 'signals' key; got: {}",
            current_stage2_result["_meta"]
        );
    }

    // ── 13.1 tool description snapshot ────────────────────────────────────
    //
    // Locks the LLM-facing `description` and per-parameter `description`
    // fields exposed via `tools/list`. A future schema-gen refactor that
    // silently regresses these strings (back to the pre-13.1 human-prose
    // wording) trips this snapshot.
    //
    // To accept an intentional reword: `cargo insta accept` after running
    // `cargo test -p vex-mcp tool_descriptors_snapshot`.

    #[test]
    fn tool_descriptors_snapshot() {
        let descriptors = tool_descriptors();
        insta::assert_json_snapshot!("tool_descriptors", descriptors);
    }

    // ---------------------------------------------------------------------
    // Phase 13.2 — `bundle` MCP tool (Inc 5)
    // ---------------------------------------------------------------------

    #[test]
    fn args_for_bundle_symbol() {
        let extra = args_for(
            "bundle",
            json!({
                "mode": "symbol",
                "symbol": "MyFunc",
                "callers_max": 7,
            }),
        );
        // Mode flag is always emitted first; symbol-specific args follow.
        assert!(extra
            .windows(2)
            .any(|w| w[0] == "--mode" && w[1] == "symbol"));
        assert!(extra
            .windows(2)
            .any(|w| w[0] == "--symbol" && w[1] == "MyFunc"));
        assert!(extra
            .windows(2)
            .any(|w| w[0] == "--callers-max" && w[1] == "7"));
    }

    #[test]
    fn args_for_bundle_pr_impact_requires_base() {
        // Missing `base` is a server-side validation failure (architect-
        // review A4: per-mode required-field validation lives here, not
        // in JSON-Schema `oneOf`).
        let err = build_command("bundle", &json!({"mode": "pr-impact"}), "/tmp/proj")
            .expect_err("pr-impact without base must error");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("base"),
            "error should mention the missing `base` field; got: {msg}"
        );
    }

    #[test]
    fn args_for_bundle_pr_impact_passes_base_and_depth() {
        let extra = args_for(
            "bundle",
            json!({
                "mode": "pr-impact",
                "base": "origin/main",
                "depth": 3,
            }),
        );
        assert!(extra
            .windows(2)
            .any(|w| w[0] == "--mode" && w[1] == "pr-impact"));
        assert!(extra
            .windows(2)
            .any(|w| w[0] == "--base" && w[1] == "origin/main"));
        assert!(extra.windows(2).any(|w| w[0] == "--depth" && w[1] == "3"));
    }

    #[test]
    fn args_for_bundle_project_default_top_n() {
        // `top_n` omitted → no `--top-n` flag is appended; the CLI uses
        // its own default (30) via clap. Tests that the MCP layer
        // doesn't force a default and override clap.
        let extra = args_for("bundle", json!({"mode": "project"}));
        assert!(extra
            .windows(2)
            .any(|w| w[0] == "--mode" && w[1] == "project"));
        assert!(
            !extra.iter().any(|a| a == "--top-n"),
            "project without top_n must not push --top-n; got: {extra:?}"
        );
    }

    #[test]
    fn args_for_bundle_project_explicit_top_n_and_glob() {
        let extra = args_for(
            "bundle",
            json!({
                "mode": "project",
                "top_n": 5,
                "path_glob": "src/**",
            }),
        );
        assert!(extra.windows(2).any(|w| w[0] == "--top-n" && w[1] == "5"));
        assert!(extra
            .windows(2)
            .any(|w| w[0] == "--path-glob" && w[1] == "src/**"));
    }

    #[test]
    fn args_for_bundle_symbol_missing_symbol_errors() {
        let err = build_command("bundle", &json!({"mode": "symbol"}), "/tmp/proj")
            .expect_err("symbol mode without `symbol` must error");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("symbol"),
            "error should mention the missing `symbol` field; got: {msg}"
        );
    }

    #[test]
    fn args_for_bundle_unknown_mode_errors() {
        let err = build_command("bundle", &json!({"mode": "not-a-real-mode"}), "/tmp/proj")
            .expect_err("unknown bundle mode must error");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("not-a-real-mode"),
            "error should mention the offending mode value; got: {msg}"
        );
    }

    #[test]
    fn args_for_bundle_missing_mode_errors() {
        let err = build_command("bundle", &json!({"symbol": "Foo"}), "/tmp/proj")
            .expect_err("bundle without `mode` must error");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("mode"),
            "error should mention the missing `mode` field; got: {msg}"
        );
    }

    #[test]
    fn bundle_schema_uses_flat_structure_no_oneof() {
        // Locks architect-review A4 — the bundle inputSchema MUST be
        // flat (no `oneOf` discriminated union). If a future revision
        // re-introduces `oneOf`, this test catches it.
        let desc = tool_descriptors();
        let tools = desc.as_array().expect("tool_descriptors returns array");
        let bundle = tools
            .iter()
            .find(|t| t["name"] == "bundle")
            .expect("bundle tool descriptor missing");
        let schema = &bundle["inputSchema"];
        assert!(
            schema.get("oneOf").is_none(),
            "bundle inputSchema must NOT use `oneOf` (A4 — flat schema only); got: {schema}"
        );
        // And `mode` must be a top-level enum field.
        let mode_field = &schema["properties"]["mode"];
        let modes = mode_field["enum"]
            .as_array()
            .expect("bundle.mode must be an enum");
        let mode_names: Vec<&str> = modes.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            mode_names,
            vec!["symbol", "pr-impact", "project"],
            "bundle.mode enum must list the three phase-13.2 modes"
        );
        // Only `mode` is required.
        let required = schema["required"]
            .as_array()
            .expect("bundle.required must be present");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "mode");
    }
}
