// `tool_descriptors()` builds a single large `serde_json::json!([...])`
// macro tree; each property added pushes the macro expansion deeper.
// Raise the crate-level recursion limit so the macro fits.
#![recursion_limit = "512"]

use std::io::{self, BufRead, Write};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::Value;

mod args;
mod descriptors;
mod env;
mod params;
mod protocol;
mod response;
mod tools;

use descriptors::tool_descriptors;
use env::{resolve_vex_bin, validate_project_root};
use params::{opt_str, req_str, ParamError};
use protocol::{
    emit_response, is_clean_shutdown, parse_error_response, JsonRpcError, JsonRpcRequest,
    JsonRpcResponse,
};
use response::build_mcp_response;
use tools::build_command;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    run_loop(stdin, &mut stdout)
}

/// Read-and-dispatch loop. Extracted from `main()` so integration tests
/// can feed canned input + capture output without spawning a subprocess.
///
/// Per JSON-RPC 2.0 + MCP semantics, the loop is resilient by design:
///   * `BrokenPipe` / `UnexpectedEof` on stdin → clean shutdown
///     (the client closed its end). Return `Ok(())`.
///   * Any other `io::Error` → log and continue. Tearing down the
///     server on a transient read hiccup would drop every in-flight
///     tool call (C4 fix).
///   * Parse failure on a non-empty line → emit a spec-compliant
///     `{"jsonrpc":"2.0","id":null,"error":{"code":-32700,...}}`
///     response so the client doesn't hang waiting for `id: N`.
fn run_loop<R: BufRead, W: Write>(reader: R, writer: &mut W) -> Result<()> {
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) if is_clean_shutdown(&e) => {
                tracing::debug!(error = %e, "stdin closed; shutting down");
                return Ok(());
            }
            Err(e) => {
                // Transient read error — log and keep serving. The next
                // iteration will retry. Bailing out here would drop every
                // in-flight tool call.
                //
                // Subtle gotcha (pre-existing in `BufReader::lines`): on
                // `ErrorKind::Interrupted` (EINTR), `BufRead::lines()` has
                // already consumed the partial line and advanced past the
                // next `\n`, so this `continue` drops whatever bytes the
                // interrupted read returned. In practice EINTR is rare
                // under MCP's stdio shape, and the next line will produce
                // a `-32700` if it lands mid-record — keeping the client
                // informed. Documented for the next reader; not patched
                // here because the fix is upstream in std.
                tracing::warn!(error = %e, "stdin read error; continuing");
                continue;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("invalid JSON-RPC: {e}");
                // JSON-RPC 2.0 §5.1: parse error → `-32700`, `id: null`
                // (we can't read the id from input we couldn't parse).
                // Without this, MCP clients hang on `id: N` forever.
                let response = parse_error_response(&line);
                emit_response(writer, &response)?;
                continue;
            }
        };

        // JSON-RPC 2.0: requests without id are notifications — do not respond
        if request.id.is_none() {
            tracing::debug!(method = %request.method, "notification (no response)");
            continue;
        }

        let response = handle_request(&request);
        emit_response(writer, &response)?;
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
                    data: None,
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
        Err(e) => {
            // H8: ParamError is the marker our typed-param helpers
            // emit. Map it to the JSON-RPC spec's `-32602 Invalid
            // params` so MCP clients can distinguish caller-side
            // type bugs from server-side failures (the generic
            // `-32000` bucket below).
            let (code, message) = match e.downcast_ref::<ParamError>() {
                Some(pe) => (-32602, pe.0.clone()),
                None => (-32000, format!("{e:#}")),
            };
            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: None,
                error: Some(JsonRpcError {
                    code,
                    message,
                    data: None,
                }),
            }
        }
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
    let params = params
        .as_ref()
        .ok_or_else(|| ParamError::missing("params"))?;
    let tool_name = req_str(params, "name")?;
    let args = &params["arguments"];

    let project_root = opt_str(args, "project_root")?
        .map(String::from)
        .or_else(|| std::env::var("VEX_ROOT").ok())
        .unwrap_or_else(|| ".".into());
    validate_project_root(&project_root)?;

    let built = build_command(tool_name, args, &project_root)?;

    let vex_bin = resolve_vex_bin()?;

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

    build_mcp_response(
        output.status.code(),
        output.status.to_string(),
        &stdout,
        &stderr,
        &built.subcommand,
        &built.deprecated_args,
        params,
    )
}

#[cfg(test)]
mod tests;
