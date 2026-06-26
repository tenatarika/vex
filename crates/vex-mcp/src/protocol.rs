//! JSON-RPC 2.0 framing primitives shared by `run_loop` and the
//! request dispatcher.
//!
//! Extracted from `main.rs` in the v1.21 split — see
//! `.claude/Task/v1.21-vex-mcp-split.md`.

use std::io::{self, ErrorKind, Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Hard cap on the raw input fragment we echo back in a `-32700` parse
/// error response. Keeps a megabyte of garbage from being copied verbatim
/// into the error frame, which would just exhaust the client's buffer.
/// 512 Unicode scalar values is enough to point a developer at the offending
/// fragment (~512 bytes for ASCII input, up to 2 KiB for all-emoji / 4-byte
/// UTF-8 sequences — the cap is applied via `.chars().take(...)` so it
/// counts code points, not bytes).
pub(crate) const PARSE_ERROR_ECHO_CAP: usize = 512;

#[derive(Deserialize)]
pub(crate) struct JsonRpcRequest {
    #[allow(dead_code)]
    pub(crate) jsonrpc: String,
    pub(crate) id: Option<Value>,
    pub(crate) method: String,
    pub(crate) params: Option<Value>,
}

#[derive(Serialize)]
pub(crate) struct JsonRpcResponse {
    pub(crate) jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<JsonRpcError>,
}

#[derive(Serialize)]
pub(crate) struct JsonRpcError {
    pub(crate) code: i32,
    pub(crate) message: String,
    /// JSON-RPC 2.0 §5.1 allows an optional `data` member for
    /// implementation-defined error detail. Used by the `-32700` path
    /// to echo a truncated copy of the unparseable input so the client
    /// dev can debug without consulting transport logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<Value>,
}

/// Stdin closed cleanly by the client. Both kinds surface when the MCP
/// host process exits or closes its pipe while we're mid-`read_line`.
pub(crate) fn is_clean_shutdown(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof)
}

/// Build the JSON-RPC 2.0 `-32700` Parse-error frame. `id` is always
/// `null` because we couldn't parse the input far enough to recover one.
/// The `data` field carries a truncated echo of the offending input so
/// the developer doesn't have to consult their MCP transport logs.
pub(crate) fn parse_error_response(raw_input: &str) -> JsonRpcResponse {
    let echo: String = raw_input.chars().take(PARSE_ERROR_ECHO_CAP).collect();
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        // JSON-RPC §5.1: id is `null` when the request couldn't be parsed.
        // We serialize it explicitly (not skipped) so clients can
        // distinguish "couldn't recover the id" from a notification.
        id: Some(Value::Null),
        result: None,
        error: Some(JsonRpcError {
            code: -32700,
            message: "Parse error".into(),
            data: Some(Value::String(echo)),
        }),
    }
}

/// Write `response` to `writer` as newline-delimited JSON. Pulled into a
/// helper so both the happy path and the parse-error path use exactly
/// the same flushing discipline.
pub(crate) fn emit_response<W: Write>(writer: &mut W, response: &JsonRpcResponse) -> Result<()> {
    let json = serde_json::to_string(response).context("serialize JSON-RPC response")?;
    writeln!(writer, "{json}")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_echo_truncated_to_cap() {
        // Garbage payload longer than the echo cap. The data field must
        // not exceed PARSE_ERROR_ECHO_CAP chars — protects clients from
        // multi-megabyte echo storms when an upstream sends junk.
        let huge = "x".repeat(PARSE_ERROR_ECHO_CAP * 4);
        let response = parse_error_response(&huge);
        let echo = response
            .error
            .as_ref()
            .expect("parse-error response carries error")
            .data
            .as_ref()
            .expect("parse-error data present")
            .as_str()
            .expect("parse-error data is string");
        assert!(
            echo.chars().count() <= PARSE_ERROR_ECHO_CAP,
            "echo must be ≤ PARSE_ERROR_ECHO_CAP chars, got {}",
            echo.chars().count()
        );
    }

    #[test]
    fn clean_shutdown_recognized_for_broken_pipe_and_eof() {
        // Both kinds count as a clean shutdown (client closed its pipe).
        let bp = io::Error::new(ErrorKind::BrokenPipe, "pipe");
        assert!(is_clean_shutdown(&bp));
        let eof = io::Error::new(ErrorKind::UnexpectedEof, "eof");
        assert!(is_clean_shutdown(&eof));
        // Other I/O errors are NOT clean shutdowns — the loop should
        // keep serving past them.
        let other = io::Error::other("transient");
        assert!(!is_clean_shutdown(&other));
    }
}
