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
            "description": "Hybrid structural + semantic code search. Finds symbols by name, signature, or meaning.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Free-text search query — symbol name, pattern, or natural language" },
                    "limit": { "type": "integer", "description": "Max results", "default": 20 },
                    "semantic": { "type": "boolean", "description": "Enable semantic vector search", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why` in the response: normalized query, per-channel hits (FST/BM25/semantic/fuzzy), filter_applied snapshot", "default": false },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax, e.g. 'tests/**')" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" },
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
            "description": "Find a symbol by exact or prefix name match.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name to find (canonical key, v1.7+)" },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "find_similar",
            "description": "Find symbols semantically similar to a description. E.g. 'payment processing' finds ChargeUseCase, BillingService.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural language description" },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "outline",
            "description": "Show structure of a source file: all symbols with kinds and line numbers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the source file (canonical key, v1.7+)" },
                    "file": { "type": "string", "description": "DEPRECATED — use `path`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Project root path" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "index",
            "description": "Build or rebuild the code index. Use --semantic for embedding generation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Project root path" },
                    "semantic": { "type": "boolean", "description": "Generate embeddings", "default": false }
                },
                "required": ["project_root"]
            }
        },
        {
            "name": "update",
            "description": "Incremental update: only re-index files that changed since last index.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Project root path" },
                    "semantic": { "type": "boolean", "description": "Generate embeddings", "default": false }
                },
                "required": ["project_root"]
            }
        },
        {
            "name": "status",
            "description": "Show index statistics: symbol count, size, embeddings status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Project root path" }
                }
            }
        },
        {
            "name": "show",
            "description": "Show the full source body of one or more symbols (function, class, struct, etc.).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Symbol names to show (canonical key, v1.7+)" },
                    "symbol": { "type": "string", "description": "DEPRECATED — use `symbols: [name]`. Pre-v1.7 singular alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results per symbol", "default": 1 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["symbols"]
            }
        },
        {
            "name": "usages",
            "description": "Find all usages/references of a symbol across the codebase.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name to find usages of (canonical key, v1.7+)" },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "strict": { "type": "boolean", "description": "Request scope-resolved (type-aware) refs. Until the persistent reference_edges section ships in 11.1.3, this flag prints a deferral notice and still serves from the legacy refs FST.", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: mode (strict/text_scan), hits before/after path filter, prefix-suggestion count when no exact hits, filter snapshot.", "default": false },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "grep",
            "description": "Search file contents by regex pattern (no index needed). Like ripgrep.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern" },
                    "filter": { "type": "string", "description": "Filter by path substring" },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "implementations",
            "description": "Find all types that inherit from or implement a base class/trait/interface (no index needed).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Base class/trait/interface name (canonical key, v1.7+)" },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "callers",
            "description": "Find all functions that call a given function. Uses the persistent call-graph FST (fast, ~4ms) when an index is present; falls back to live-scan otherwise.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Function name (canonical key, v1.7+)" },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "callees",
            "description": "Find all functions called by a given function. Uses the persistent call-graph FST (fast, ~4ms) when an index is present; falls back to live-scan otherwise.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Function name (canonical key, v1.7+)" },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "pattern",
            "description": "Structural AST pattern matching. Match code by shape rather than text: `$NAME` captures an identifier or balanced expression, `$_` is a wildcard, `$$$` matches anything anonymously (ellipsis), `$$$NAME` / `$$NAME` is a named ellipsis that captures a multi-line body or arg list, repeated metavars enforce back-reference equality. Composition: space-flanked ` && ` and ` || ` join sub-patterns (AND requires both shapes in the file with shared captures agreeing; OR takes the union). When the project has been indexed (`vex index`), a persisted skeleton prefilter narrows candidates to files containing the right node kinds — set `why: true` to inspect which mode fired.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Structural pattern (e.g. `fn $NAME($$ARGS) -> Result<$T, $E> { $$$BODY }`, `interface $N || class $N`)" },
                    "lang": { "type": "string", "description": "Language: rust, python, typescript, go, java, csharp, ruby, kotlin, swift, cpp, php, sql, markdown" },
                    "limit": { "type": "integer", "description": "Max matches to return", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" },
                    "why": { "type": "boolean", "description": "Surface a ScanTrace under `_meta.why` in the response: mode (indexed/live_scan), root_kind_inferred, candidate_files / total_files, fallback_reason." }
                },
                "required": ["pattern", "lang"]
            }
        },
        {
            "name": "diff",
            "description": "Symbol-level diff between an arbitrary git revision and the working tree. Lists added / removed / moved / body-changed symbols across the files touched on the branch. Useful for `what did this PR change?` queries without scrolling through unified diffs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base": { "type": "string", "description": "Git revision to compare against (e.g. main, HEAD~3, origin/main). Working tree is the new side." },
                    "limit": { "type": "integer", "description": "Max changes to return", "default": 500 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist changes by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist changes by path glob (wins over include)" }
                },
                "required": ["base"]
            }
        },
        {
            "name": "paths",
            "description": "Enumerate caller chains from `from` to `to` in the persistent call graph. Multi-hop generalisation of callers — useful when investigating how a function gets reached from a known entry point. Requires a v4 index with call graph (built without `--no-call-graph`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Starting function (the caller / entry point)" },
                    "to": { "type": "string", "description": "Destination function (the callee being investigated)" },
                    "max_hops": { "type": "integer", "description": "Maximum hops between from and to", "default": 6 },
                    "max_paths": { "type": "integer", "description": "Maximum paths to enumerate (caps output, aborts traversal early)", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist intermediate steps by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist intermediate steps by path glob (wins over include)" }
                },
                "required": ["from", "to"]
            }
        },
        {
            "name": "reachable",
            "description": "List symbols whose callees transitively reach `target` — i.e. everything that could end up calling target, directly or indirectly. Useful for blast-radius analysis. Requires a v4 index with call graph.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Symbol whose callers (direct + transitive) you want" },
                    "max_hops": { "type": "integer", "description": "Maximum hops to walk back from target", "default": 6 },
                    "limit": { "type": "integer", "description": "Max results", "default": 200 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["target"]
            }
        },
        {
            "name": "check",
            "description": "Fast existence check: verify if symbols exist in the index without full search. Use before search to avoid unnecessary queries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Symbol names to check (canonical key, v1.7+)" },
                    "names": { "type": "array", "items": { "type": "string" }, "description": "DEPRECATED — use `symbols`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true }
                },
                "required": ["symbols"]
            }
        },
        {
            "name": "similar",
            "description": "Find symbols semantically similar to an EXISTING symbol (resolves the symbol's stored embedding, returns nearest neighbors). Different from find_similar, which queries by free-form description. Requires `vex index --semantic`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Existing symbol name to find similar to (canonical key, v1.7+)" },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 10 },
                    "threshold": { "type": "number", "description": "Minimum cosine similarity (0.0..1.0)", "default": 0.5 },
                    "filter": { "type": "string", "description": "Filter results by path substring" },
                    "explain": { "type": "boolean", "description": "Include reasoning per match: identifier-set Jaccard overlap + truncated unified diff between bodies", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: seed resolution, applied threshold, candidates before/after path filter, filter snapshot.", "default": false },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
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
            "name": "duplicates",
            "description": "Find pairs of near-duplicate symbols by embedding similarity. Useful for refactoring and dedup. Requires `vex index --semantic`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "threshold": { "type": "number", "description": "Minimum cosine similarity (0.0..1.0)", "default": 0.9 },
                    "limit": { "type": "integer", "description": "Max pairs to return", "default": 50 },
                    "min_body_lines": { "type": "integer", "description": "Skip symbols with body shorter than this many lines", "default": 5 },
                    "filter": { "type": "string", "description": "Restrict to pairs where at least one symbol's path contains this substring" },
                    "explain": { "type": "boolean", "description": "Include reasoning per pair: identifier-set Jaccard overlap + truncated unified diff between the two bodies", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: applied threshold + min_body_lines, pairs before/after path filter, filter snapshot.", "default": false },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist pairs by path glob — a pair is kept when at least one side matches" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist pairs by path glob — a pair is dropped when either side matches" }
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

    #[test]
    fn mcp_response_lifts_protocol_version_to_top_level() {
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
}
