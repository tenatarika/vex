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
            Ok(("search".into(), extra))
        }
        "find_symbol" => {
            let name = args["name"].as_str().context("missing name")?;
            Ok((
                "search".into(),
                vec![name.to_string(), "--limit".into(), "10".into()],
            ))
        }
        "find_similar" => {
            let query = args["query"].as_str().context("missing query")?;
            Ok((
                "search".into(),
                vec![
                    query.to_string(),
                    "--semantic".into(),
                    "--limit".into(),
                    "10".into(),
                ],
            ))
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
                    "project_root": { "type": "string", "description": "Project root path" }
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
                    "project_root": { "type": "string", "description": "Project root path" }
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
                    "project_root": { "type": "string", "description": "Project root path" }
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
