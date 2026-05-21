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

    let mut result = serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&content)?
        }]
    });

    // Surface MCP-protocol-level metadata via the reserved `_meta` field
    // (see modelcontextprotocol.io spec). Clients that don't read
    // `_meta` see the unchanged content array; clients that do can
    // detect deprecated argument usage.
    if !built.deprecated_args.is_empty() {
        result["_meta"] = serde_json::json!({
            "deprecated_args": built.deprecated_args,
        });
    }

    Ok(result)
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
            ("search".to_string(), extra)
        }
        "find_symbol" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)
                .context("missing symbol")?;
            let mut extra = vec![symbol.to_string(), "--limit".into(), "10".into()];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
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
            ("show".to_string(), extra)
        }
        "usages" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)
                .context("missing symbol")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![symbol.to_string(), "--limit".into(), limit.to_string()];
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
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            ("duplicates".to_string(), extra)
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
                    "why": { "type": "boolean", "description": "Append a JSON trace to stderr: normalized query, per-channel hits (FST/BM25/semantic/fuzzy), filter_applied snapshot", "default": false },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax, e.g. 'tests/**')" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
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
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["symbol"]
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
}
