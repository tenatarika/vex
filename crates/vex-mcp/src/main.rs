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

        let request: JsonRpcRequest = serde_json::from_str(&line)
            .context("parse JSON-RPC request")?;

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
        _ => Err(anyhow::anyhow!("unknown method: {}", req.method)),
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
                message: e.to_string(),
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
        .or_else(|| std::env::var("VEX_ROOT").ok().as_deref().map(|s| s.to_string()).as_deref())
        .unwrap_or(".")
        .to_string();

    let (subcommand, extra_args) = match tool_name {
        "search" => {
            let query = args["query"].as_str().context("missing query")?;
            let limit = args["limit"].as_u64().unwrap_or(20);
            let semantic = args["semantic"].as_bool().unwrap_or(false);
            let mut extra = vec![query.to_string(), "--limit".into(), limit.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            ("search", extra)
        }
        "find_symbol" => {
            let name = args["name"].as_str().context("missing name")?;
            ("search".to_string(), vec![name.to_string(), "--limit".into(), "10".into()])
                .into()
        }
        "find_similar" => {
            let query = args["query"].as_str().context("missing query")?;
            ("search", vec![query.to_string(), "--semantic".into(), "--limit".into(), "10".into()])
        }
        "index" => ("index", vec!["--path".into(), project_root.clone()]),
        "status" => ("status", vec!["--path".into(), project_root.clone()]),
        _ => anyhow::bail!("unknown tool: {tool_name}"),
    };

    let output = Command::new("vex")
        .arg(subcommand)
        .args(&extra_args)
        .arg("--format")
        .arg("json")
        .output()
        .context("failed to spawn vex")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("vex exited with {}: {stderr}", output.status);
    }

    let content: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| serde_json::json!({ "text": stdout.to_string() }));

    Ok(serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&content)?
        }]
    }))
}

fn tool_descriptors() -> Value {
    serde_json::json!([
        {
            "name": "search",
            "description": "Hybrid structural + semantic code search. Finds symbols by name, signature, or meaning.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query — symbol name, pattern, or natural language description"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results to return",
                        "default": 20
                    },
                    "semantic": {
                        "type": "boolean",
                        "description": "Enable semantic (vector) search",
                        "default": false
                    },
                    "project_root": {
                        "type": "string",
                        "description": "Project root path"
                    }
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
                    "name": {
                        "type": "string",
                        "description": "Symbol name to find"
                    },
                    "project_root": {
                        "type": "string",
                        "description": "Project root path"
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "find_similar",
            "description": "Find symbols semantically similar to a description. E.g. 'payment processing' finds ChargeUseCase, BillingService, etc.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural language description of what you're looking for"
                    },
                    "project_root": {
                        "type": "string",
                        "description": "Project root path"
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "index",
            "description": "Build or rebuild the code index for a project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": {
                        "type": "string",
                        "description": "Project root path to index"
                    }
                },
                "required": ["project_root"]
            }
        },
        {
            "name": "status",
            "description": "Show index statistics: file count, symbol count, index size.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": {
                        "type": "string",
                        "description": "Project root path"
                    }
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
