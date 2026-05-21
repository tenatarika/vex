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

    let (subcommand, extra_args) = build_command(tool_name, args, &project_root)?;

    let vex_bin = std::env::var("VEX_BIN").unwrap_or_else(|_| "vex".into());

    let output = Command::new(&vex_bin)
        .arg(&subcommand)
        .args(&extra_args)
        .arg("--format")
        .arg("json")
        .current_dir(&project_root)
        .output()
        .with_context(|| format!("failed to spawn {vex_bin}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("vex {subcommand} failed ({}): {stderr}", output.status);
    }

    let content: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| serde_json::json!({ "raw": stdout.trim() }));

    Ok(serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&content)?
        }]
    }))
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

fn build_command(tool: &str, args: &Value, project_root: &str) -> Result<(String, Vec<String>)> {
    match tool {
        "search" => {
            let query = args["query"].as_str().context("missing query")?;
            let limit = args["limit"].as_u64().unwrap_or(20);
            let semantic = args["semantic"].as_bool().unwrap_or(false);
            let mut extra = vec![query.to_string(), "--limit".into(), limit.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            Ok(("search".into(), extra))
        }
        "find_symbol" => {
            let name = args["name"].as_str().context("missing name")?;
            let mut extra = vec![name.to_string(), "--limit".into(), "10".into()];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            Ok(("search".into(), extra))
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
            Ok(("search".into(), extra))
        }
        "outline" => {
            let file = args["file"].as_str().context("missing file")?;
            Ok(("outline".into(), vec![file.to_string()]))
        }
        "index" => {
            let semantic = args["semantic"].as_bool().unwrap_or(false);
            let mut extra = vec!["--path".into(), project_root.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            Ok(("index".into(), extra))
        }
        "update" => {
            let semantic = args["semantic"].as_bool().unwrap_or(false);
            let mut extra = vec!["--path".into(), project_root.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            Ok(("update".into(), extra))
        }
        "status" => Ok((
            "status".into(),
            vec!["--path".into(), project_root.to_string()],
        )),
        "show" => {
            let symbols: Vec<String> = if let Some(arr) = args["symbols"].as_array() {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            } else if let Some(s) = args["symbol"].as_str() {
                vec![s.to_string()]
            } else {
                anyhow::bail!("missing symbol(s)")
            };
            let limit = args["limit"].as_u64().unwrap_or(1);
            let mut extra = symbols;
            extra.extend(["--limit".into(), limit.to_string()]);
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            Ok(("show".into(), extra))
        }
        "usages" => {
            let name = args["name"].as_str().context("missing name")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![name.to_string(), "--limit".into(), limit.to_string()];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            Ok(("usages".into(), extra))
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
            Ok(("grep".into(), extra))
        }
        "implementations" => {
            let name = args["name"].as_str().context("missing name")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![
                name.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_scope(&mut extra, args);
            Ok(("implementations".into(), extra))
        }
        "callers" => {
            let name = args["name"].as_str().context("missing name")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![
                name.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            Ok(("callers".into(), extra))
        }
        "callees" => {
            let name = args["name"].as_str().context("missing name")?;
            let limit = args["limit"].as_u64().unwrap_or(50);
            let mut extra = vec![
                name.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_auto_update(&mut extra, args);
            push_scope(&mut extra, args);
            Ok(("callees".into(), extra))
        }
        "check" => {
            let names: Vec<String> = args["names"]
                .as_array()
                .context("missing names array")?
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if names.is_empty() {
                anyhow::bail!("names array is empty");
            }
            let mut extra = names;
            push_auto_update(&mut extra, args);
            Ok(("check".into(), extra))
        }
        "similar" => {
            let name = args["name"].as_str().context("missing name")?;
            let limit = args["limit"].as_u64().unwrap_or(10);
            let threshold = args["threshold"].as_f64().unwrap_or(0.5);
            let mut extra = vec![
                name.to_string(),
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
            Ok(("similar".into(), extra))
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
            Ok(("duplicates".into(), extra))
        }
        _ => anyhow::bail!("unknown tool: {tool}"),
    }
}

fn tool_descriptors() -> Value {
    serde_json::json!([
        {
            "name": "search",
            "description": "Hybrid structural + semantic code search. Finds symbols by name, signature, or meaning.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query — symbol name, pattern, or natural language" },
                    "limit": { "type": "integer", "description": "Max results", "default": 20 },
                    "semantic": { "type": "boolean", "description": "Enable semantic vector search", "default": false },
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
                    "name": { "type": "string", "description": "Symbol name to find" },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["name"]
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
                    "file": { "type": "string", "description": "Path to the source file" },
                    "project_root": { "type": "string", "description": "Project root path" }
                },
                "required": ["file"]
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
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Symbol names to show" },
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
                    "name": { "type": "string", "description": "Symbol name to find usages of" },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["name"]
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
                    "name": { "type": "string", "description": "Base class/trait/interface name" },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "callers",
            "description": "Find all functions that call a given function. Uses the persistent call-graph FST (fast, ~4ms) when an index is present; falls back to live-scan otherwise.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Function name" },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "callees",
            "description": "Find all functions called by a given function. Uses the persistent call-graph FST (fast, ~4ms) when an index is present; falls back to live-scan otherwise.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Function name" },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "check",
            "description": "Fast existence check: verify if symbols exist in the index without full search. Use before search to avoid unnecessary queries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "names": { "type": "array", "items": { "type": "string" }, "description": "Symbol names to check" },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true }
                },
                "required": ["names"]
            }
        },
        {
            "name": "similar",
            "description": "Find symbols semantically similar to an EXISTING symbol (resolves the symbol's stored embedding, returns nearest neighbors). Different from find_similar, which queries by free-form description. Requires `vex index --semantic`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Existing symbol name to find similar to" },
                    "limit": { "type": "integer", "description": "Max results", "default": 10 },
                    "threshold": { "type": "number", "description": "Minimum cosine similarity (0.0..1.0)", "default": 0.5 },
                    "filter": { "type": "string", "description": "Filter results by path substring" },
                    "explain": { "type": "boolean", "description": "Include reasoning per match: identifier-set Jaccard overlap + truncated unified diff between bodies", "default": false },
                    "project_root": { "type": "string", "description": "Project root path" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (gitignore syntax)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include)" }
                },
                "required": ["name"]
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
        let (_subcmd, extra) = build_command(tool, &args, "/tmp/proj").expect("build_command");
        extra
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
