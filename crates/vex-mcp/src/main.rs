// `tool_descriptors()` builds a single large `serde_json::json!([...])`
// macro tree; each property added pushes the macro expansion deeper.
// Raise the crate-level recursion limit so the macro fits.
#![recursion_limit = "512"]

use std::io::{self, BufRead, ErrorKind, Write};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Marker error returned by the typed-param helpers (H8). When
/// `handle_request` downcasts to this type it emits a JSON-RPC 2.0
/// `-32602 Invalid params` response instead of the generic `-32000`
/// server error. Pre-H8 the MCP server silently coerced wrong-typed
/// fields to their defaults (`as_bool().unwrap_or(false)` etc.), which
/// hid integration bugs in downstream agents.
#[derive(Debug, thiserror::Error)]
#[error("invalid params: {0}")]
struct ParamError(String);

impl ParamError {
    fn wrong_type(field: &str, expected: &str, actual: &Value) -> Self {
        let kind = match actual {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        Self(format!(
            "`{field}` must be {expected}; got {kind} ({actual})"
        ))
    }

    fn missing(field: &str) -> Self {
        Self(format!("missing required field `{field}`"))
    }

    /// For fields that also accept a deprecated alias. Tells the caller
    /// both names so an LLM agent that hallucinated the alias
    /// (`symbol` vs `symbols`, `names` vs `symbols`) sees the canonical
    /// shape without round-tripping back to the schema description.
    fn missing_with_alias(canonical: &str, legacy: &str) -> Self {
        Self(format!(
            "missing required field `{canonical}` \
             (legacy alias `{legacy}` also accepted)"
        ))
    }
}

/// Required string field. Fails with `-32602` if missing or wrong type.
fn req_str<'a>(args: &'a Value, field: &str) -> Result<&'a str> {
    let v = &args[field];
    if v.is_null() {
        return Err(ParamError::missing(field).into());
    }
    v.as_str()
        .ok_or_else(|| ParamError::wrong_type(field, "a string", v).into())
}

/// Optional string field. `None` when absent / null; fails with `-32602`
/// when present but not a string.
fn opt_str<'a>(args: &'a Value, field: &str) -> Result<Option<&'a str>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    Some(
        v.as_str()
            .ok_or_else(|| ParamError::wrong_type(field, "a string", v)),
    )
    .transpose()
    .map_err(Into::into)
}

/// Optional bool with default. Fails when present-but-not-bool — silent
/// coerce (`as_bool().unwrap_or(default)` on `"true"`-string) silently
/// dropped the value, which hid downstream type bugs.
fn opt_bool(args: &Value, field: &str, default: bool) -> Result<bool> {
    let v = &args[field];
    if v.is_null() {
        return Ok(default);
    }
    v.as_bool()
        .ok_or_else(|| ParamError::wrong_type(field, "a boolean", v).into())
}

/// Optional bool that distinguishes "absent / null" from an explicit value.
/// Returns `None` when the field is absent or null; fails on wrong type the
/// same way [`opt_bool`] does. Used by the `index`/`update` `gpu` arm so an
/// explicit `gpu: false` can forward `--no-gpu` (overriding `.vex.toml gpu =
/// true`), while an absent `gpu` forwards nothing (letting config / VEX_DEVICE
/// decide via the CLI's `Device::resolve`).
fn opt_bool_some(args: &Value, field: &str) -> Result<Option<bool>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    Some(
        v.as_bool()
            .ok_or_else(|| ParamError::wrong_type(field, "a boolean", v)),
    )
    .transpose()
    .map_err(Into::into)
}

/// Optional u64 with default. Fails on negative / float / string input —
/// `serde_json::Value::as_u64()` returns `None` for all three, which the
/// old `unwrap_or(default)` silently masked.
fn opt_u64(args: &Value, field: &str, default: u64) -> Result<u64> {
    let v = &args[field];
    if v.is_null() {
        return Ok(default);
    }
    v.as_u64()
        .ok_or_else(|| ParamError::wrong_type(field, "a non-negative integer", v).into())
}

/// Optional u64 that distinguishes "absent / null" from "explicit value".
/// Returns `None` when the field is absent or null; fails on wrong type
/// the same way [`opt_u64`] does. Used by the `bundle` arm where a `0`
/// fallback would leak `--depth 0` (etc.) to the CLI — there the
/// presence of the field is itself the signal to forward it.
fn opt_u64_some(args: &Value, field: &str) -> Result<Option<u64>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    Some(
        v.as_u64()
            .ok_or_else(|| ParamError::wrong_type(field, "a non-negative integer", v)),
    )
    .transpose()
    .map_err(Into::into)
}

/// Optional f64. `None` when absent / null.
fn opt_f64(args: &Value, field: &str) -> Result<Option<f64>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    Some(
        v.as_f64()
            .ok_or_else(|| ParamError::wrong_type(field, "a number", v)),
    )
    .transpose()
    .map_err(Into::into)
}

/// Optional string-array. `None` when absent / null; fails when present
/// but not an array or when an element is not a string.
fn opt_str_array<'a>(args: &'a Value, field: &str) -> Result<Option<Vec<&'a str>>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    let arr = v
        .as_array()
        .ok_or_else(|| ParamError::wrong_type(field, "a string array", v))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, elem) in arr.iter().enumerate() {
        let s = elem
            .as_str()
            .ok_or_else(|| ParamError::wrong_type(&format!("{field}[{i}]"), "a string", elem))?;
        out.push(s);
    }
    Ok(Some(out))
}

/// Hard cap on the raw input fragment we echo back in a `-32700` parse
/// error response. Keeps a megabyte of garbage from being copied verbatim
/// into the error frame, which would just exhaust the client's buffer.
/// 512 Unicode scalar values is enough to point a developer at the offending
/// fragment (~512 bytes for ASCII input, up to 2 KiB for all-emoji / 4-byte
/// UTF-8 sequences — the cap is applied via `.chars().take(...)` so it
/// counts code points, not bytes).
const PARSE_ERROR_ECHO_CAP: usize = 512;

/// v1.12.0 S8.3 — resolve the `vex` binary path and validate it before
/// `Command::spawn` so a typo'd `VEX_BIN` surfaces a human-readable error
/// instead of an opaque OS-level "No such file or directory" buried
/// inside the JSON-RPC tool-call response. When `VEX_BIN` is unset we
/// keep the existing behaviour: fall through to the literal string
/// `"vex"` and let the OS's PATH resolution find it (or fail loudly
/// later — that path is already user-controlled).
fn resolve_vex_bin() -> Result<String> {
    let Some(raw) = std::env::var_os("VEX_BIN") else {
        return Ok("vex".into());
    };
    let path = std::path::PathBuf::from(&raw);
    if !path.exists() {
        anyhow::bail!(
            "VEX_BIN points to `{}` but no such file exists; \
             unset VEX_BIN to fall back to PATH lookup of `vex`",
            path.display()
        );
    }
    if !path.is_file() {
        anyhow::bail!(
            "VEX_BIN points to `{}` but it is not a regular file \
             (likely a directory); unset VEX_BIN or point it at the \
             `vex` binary directly",
            path.display()
        );
    }
    // On Unix, additionally assert the binary is executable. Windows
    // associates executability by extension (.exe), so the `is_file`
    // check above is sufficient there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = path
            .metadata()
            .with_context(|| format!("stat VEX_BIN target `{}`", path.display()))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            anyhow::bail!(
                "VEX_BIN target `{}` is not executable (mode {:o}); \
                 run `chmod +x` on it or unset VEX_BIN",
                path.display(),
                mode & 0o777
            );
        }
    }
    Ok(path
        .into_os_string()
        .into_string()
        .unwrap_or_else(|os| os.to_string_lossy().into_owned()))
}

/// v1.12.0 S8.3 — validate the resolved project root before passing it to
/// `Command::current_dir`. Without this, a bogus `project_root` argument
/// (or a typo'd `VEX_ROOT`) yields the same opaque OS-level error as
/// VEX_BIN above. We keep `.` (the implicit default) un-canonicalized so
/// the spawn falls through to the MCP server's cwd unchanged.
fn validate_project_root(project_root: &str) -> Result<()> {
    if project_root == "." {
        return Ok(());
    }
    let path = std::path::Path::new(project_root);
    if !path.exists() {
        anyhow::bail!(
            "project_root `{}` does not exist (set via tool arg \
             `project_root` or env VEX_ROOT)",
            project_root
        );
    }
    if !path.is_dir() {
        anyhow::bail!("project_root `{}` is not a directory", project_root);
    }
    Ok(())
}

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

/// Stdin closed cleanly by the client. Both kinds surface when the MCP
/// host process exits or closes its pipe while we're mid-`read_line`.
fn is_clean_shutdown(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof)
}

/// Build the JSON-RPC 2.0 `-32700` Parse-error frame. `id` is always
/// `null` because we couldn't parse the input far enough to recover one.
/// The `data` field carries a truncated echo of the offending input so
/// the developer doesn't have to consult their MCP transport logs.
fn parse_error_response(raw_input: &str) -> JsonRpcResponse {
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
fn emit_response<W: Write>(writer: &mut W, response: &JsonRpcResponse) -> Result<()> {
    let json = serde_json::to_string(response).context("serialize JSON-RPC response")?;
    writeln!(writer, "{json}")?;
    writer.flush()?;
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

/// Pure helper that turns a `vex` subprocess outcome into the JSON-RPC
/// tool-call `result` value. Extracted from `handle_tool_call` so unit
/// tests can drive every exit-code / envelope shape without a real
/// subprocess (resolving the long-standing TODO around the
/// `mcp_envelope_lifting_logic_mirrors_handle_tool_call` test).
///
/// **v1.19.1 (D1)**: exit code 1 is the documented "no results found"
/// success path per `src/cli/exit_code.rs`. Pre-fix the wrapper
/// collapsed exit 1 and exit 2 into a single `-32000` failure, so
/// every clean empty result (`vex usages X --strict` with 0 hits,
/// `vex search Y` with no matches, etc.) reached the MCP client as
/// an error. Agents could no longer distinguish "binder confirmed
/// 0 references → safe to delete" from "tool fell over → I don't
/// know" — the flagship delete-safety query was unusable. Exit 1 now
/// falls through to envelope parsing and the LLM sees
/// `structuredContent.results = []`. Exit 2 (and signal-kill, where
/// `exit_code` is `None`) still bails as a real error.
fn build_mcp_response(
    exit_code: Option<i32>,
    exit_status_display: String,
    stdout: &str,
    stderr: &str,
    subcommand: &str,
    deprecated_args: &[String],
    params: &Value,
) -> Result<Value> {
    let is_success = exit_code == Some(0);
    let is_soft_empty = exit_code == Some(1);
    if !is_success && !is_soft_empty {
        // Surface stdout alongside stderr — for many vex error paths the
        // JSON-error body is on stdout and stderr only carries the
        // `Error:` prefix line. Truncate so a runaway message can't
        // explode the JSON-RPC response. `str::floor_char_boundary` is
        // only stable since Rust 1.93; on the 1.88 MSRV we walk
        // backwards from the cap via the stable `is_char_boundary`
        // until we land on a UTF-8 split point.
        let trimmed = stdout.trim();
        let stdout_snippet = if trimmed.is_empty() {
            String::new()
        } else {
            const CAP: usize = 512;
            if trimmed.len() > CAP {
                let mut end = CAP;
                while end > 0 && !trimmed.is_char_boundary(end) {
                    end -= 1;
                }
                format!(" stdout: {}…(truncated)", &trimmed[..end])
            } else {
                format!(" stdout: {trimmed}")
            }
        };
        anyhow::bail!("vex {subcommand} failed ({exit_status_display}): {stderr}{stdout_snippet}");
    }

    let content: Value = serde_json::from_str(stdout)
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
    if !deprecated_args.is_empty() {
        meta.insert("deprecated_args".into(), serde_json::json!(deprecated_args));
    }
    if let Some(trace) = extract_why_trace(stderr) {
        meta.insert("why".into(), trace);
    }
    if !meta.is_empty() {
        result["_meta"] = Value::Object(meta);
    }

    Ok(result)
}

/// Extract the `--why` ScanTrace JSON from a vex CLI's stderr.
///
/// **v1.10.1 (review S8.1)**: the CLI now tags the trace line with
/// `VEX_WHY:` (see `src/cli/trace.rs::WHY_TRACE_PREFIX`). Before that,
/// any early `tracing::warn!` JSON on stderr (e.g. the "cannot
/// determine index freshness" warning) could shadow the real trace
/// because we picked the first `{`-prefixed line. We scan for the
/// tagged line first; if none is present (older `vex` binary on PATH
/// at MCP-spawn time, no `--why` was passed, or the CLI failed before
/// the emit site) we fall back to the legacy behaviour — picking the
/// LAST line that parses as JSON, so that any earlier diagnostic
/// objects no longer override the trace.
fn extract_why_trace(stderr: &str) -> Option<Value> {
    const PREFIX: &str = "VEX_WHY:";
    for line in stderr.lines() {
        if let Some(rest) = line.trim_start().strip_prefix(PREFIX) {
            if let Ok(v) = serde_json::from_str::<Value>(rest.trim()) {
                return Some(v);
            }
        }
    }
    // Legacy fallback: scan bottom-up so a later `--why` trace beats
    // an earlier `tracing::warn!` JSON object even on un-tagged output.
    stderr.lines().rev().find_map(|line| {
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
fn auto_update(args: &Value) -> Result<bool> {
    opt_bool(args, "auto_update", true)
}

fn push_auto_update(extra: &mut Vec<String>, args: &Value) -> Result<()> {
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
fn push_gpu(extra: &mut Vec<String>, args: &Value) -> Result<()> {
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
fn push_no_stale_check(extra: &mut Vec<String>, args: &Value) -> Result<()> {
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
fn push_diff_scope(extra: &mut Vec<String>, args: &Value) -> Result<()> {
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
fn push_show_truncate(extra: &mut Vec<String>, args: &Value) -> Result<()> {
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
fn push_kind(extra: &mut Vec<String>, args: &Value) -> Result<()> {
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
fn push_scope(extra: &mut Vec<String>, args: &Value) -> Result<()> {
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
fn push_metadata(extra: &mut Vec<String>, args: &Value) -> Result<()> {
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
) -> Result<Option<&'a str>> {
    if let Some(s) = opt_str(args, canonical)? {
        return Ok(Some(s));
    }
    if let Some(s) = opt_str(args, legacy)? {
        deprecated.push(legacy.to_string());
        return Ok(Some(s));
    }
    Ok(None)
}

/// Array variant of `read_canonical_str` — used by tools whose primary
/// argument is `string[]` (e.g. `check`, `show`).
fn read_canonical_array<'a>(
    args: &'a Value,
    canonical: &str,
    legacy: &str,
    deprecated: &mut Vec<String>,
) -> Result<Option<&'a Vec<Value>>> {
    let cv = &args[canonical];
    if !cv.is_null() {
        let arr = cv
            .as_array()
            .ok_or_else(|| ParamError::wrong_type(canonical, "an array", cv))?;
        return Ok(Some(arr));
    }
    let lv = &args[legacy];
    if !lv.is_null() {
        let arr = lv
            .as_array()
            .ok_or_else(|| ParamError::wrong_type(legacy, "an array", lv))?;
        deprecated.push(legacy.to_string());
        return Ok(Some(arr));
    }
    Ok(None)
}

fn build_command(tool: &str, args: &Value, project_root: &str) -> Result<BuiltCommand> {
    let mut deprecated: Vec<String> = Vec::new();
    let (subcommand, extra_args) = match tool {
        "search" => {
            let query = req_str(args, "query")?;
            let limit = opt_u64(args, "limit", 20)?;
            let semantic = opt_bool(args, "semantic", false)?;
            let mut extra = vec![query.to_string(), "--limit".into(), limit.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            if opt_bool(args, "why", false)? {
                extra.push("--why".into());
            }
            if let Some(filter) = opt_str(args, "filter")? {
                extra.extend(["--filter".into(), filter.to_string()]);
            }
            push_kind(&mut extra, args)?;
            if let Some(cp) = opt_str(args, "context_path")? {
                extra.extend(["--context-path".into(), cp.to_string()]);
            }
            if opt_bool(args, "no_bm25", false)? {
                extra.push("--no-bm25".into());
            }
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            push_metadata(&mut extra, args)?;
            push_diff_scope(&mut extra, args)?;
            ("search".to_string(), extra)
        }
        "find_symbol" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)?
                .ok_or_else(|| ParamError::missing("symbol"))?;
            let mut extra = vec![symbol.to_string(), "--limit".into(), "10".into()];
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            push_metadata(&mut extra, args)?;
            ("search".to_string(), extra)
        }
        "find_similar" => {
            let query = req_str(args, "query")?;
            let mut extra = vec![
                query.to_string(),
                "--semantic".into(),
                "--limit".into(),
                "10".into(),
            ];
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            push_metadata(&mut extra, args)?;
            ("search".to_string(), extra)
        }
        "outline" => {
            let path = read_canonical_str(args, "path", "file", &mut deprecated)?
                .ok_or_else(|| ParamError::missing("path"))?;
            ("outline".to_string(), vec![path.to_string()])
        }
        "index" => {
            let semantic = opt_bool(args, "semantic", false)?;
            let mut extra = vec!["--path".into(), project_root.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            push_gpu(&mut extra, args)?;
            ("index".to_string(), extra)
        }
        "update" => {
            let semantic = opt_bool(args, "semantic", false)?;
            let mut extra = vec!["--path".into(), project_root.to_string()];
            if semantic {
                extra.push("--semantic".into());
            }
            push_gpu(&mut extra, args)?;
            ("update".to_string(), extra)
        }
        "status" => (
            "status".to_string(),
            vec!["--path".into(), project_root.to_string()],
        ),
        "eval" => {
            // FU-1: thin wrapper around `vex eval`. Index-less in the sense
            // that it never builds — consumes whatever index already lives
            // at --path. Lives next to `status` / `diff` because all three
            // are indexless / read-only.
            let mut extra = vec!["--path".into(), project_root.to_string()];
            if let Some(bench) = opt_str(args, "bench")? {
                extra.push("--bench".into());
                extra.push(bench.to_string());
            }
            if let Some(min_ndcg) = opt_f64(args, "min_ndcg")? {
                extra.push("--min-ndcg".into());
                extra.push(min_ndcg.to_string());
            }
            // MCP defaults `json` to true (agents want structured output);
            // the CLI defaults to text. Honor an explicit `false` to opt
            // back into the human-readable summary.
            let want_json = opt_bool(args, "json", true)?;
            if want_json {
                extra.push("--json".into());
            }
            ("eval".to_string(), extra)
        }
        "show" => {
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
            ("show".to_string(), extra)
        }
        "usages" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)?
                .ok_or_else(|| ParamError::missing("symbol"))?;
            let limit = opt_u64(args, "limit", 50)?;
            let mut extra = vec![symbol.to_string(), "--limit".into(), limit.to_string()];
            if opt_bool(args, "strict", false)? {
                extra.push("--strict".into());
            }
            // 11.10: structured trace via stderr — picked up by
            // `extract_why_trace` and surfaced under `_meta.why`.
            if opt_bool(args, "why", false)? {
                extra.push("--why".into());
            }
            if let Some(filter) = opt_str(args, "filter")? {
                extra.extend(["--filter".into(), filter.to_string()]);
            }
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            push_diff_scope(&mut extra, args)?;
            ("usages".to_string(), extra)
        }
        "grep" => {
            let pattern = req_str(args, "pattern")?;
            let limit = opt_u64(args, "limit", 50)?;
            let mut extra = vec![
                pattern.to_string(),
                "--limit".into(),
                limit.to_string(),
                "--path".into(),
                project_root.to_string(),
            ];
            if let Some(filter) = opt_str(args, "filter")? {
                extra.extend(["--filter".into(), filter.to_string()]);
            }
            push_scope(&mut extra, args)?;
            ("grep".to_string(), extra)
        }
        "implementations" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)?
                .ok_or_else(|| ParamError::missing("symbol"))?;
            let limit = opt_u64(args, "limit", 50)?;
            let mut extra = vec![
                symbol.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            push_diff_scope(&mut extra, args)?;
            ("implementations".to_string(), extra)
        }
        "callers" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)?
                .ok_or_else(|| ParamError::missing("symbol"))?;
            let limit = opt_u64(args, "limit", 50)?;
            let mut extra = vec![
                symbol.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            push_diff_scope(&mut extra, args)?;
            ("callers".to_string(), extra)
        }
        "callees" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)?
                .ok_or_else(|| ParamError::missing("symbol"))?;
            let limit = opt_u64(args, "limit", 50)?;
            let mut extra = vec![
                symbol.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            push_diff_scope(&mut extra, args)?;
            ("callees".to_string(), extra)
        }
        "pattern" => {
            let pattern = req_str(args, "pattern")?;
            let lang = req_str(args, "lang")?;
            let limit = opt_u64(args, "limit", 50)?;
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
            if opt_bool(args, "why", false)? {
                extra.push("--why".into());
            }
            push_scope(&mut extra, args)?;
            push_diff_scope(&mut extra, args)?;
            ("pattern".to_string(), extra)
        }
        "diff" => {
            let base = req_str(args, "base")?;
            let limit = opt_u64(args, "limit", 500)?;
            let mut extra = vec![
                "--base".into(),
                base.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_scope(&mut extra, args)?;
            ("diff".to_string(), extra)
        }
        "paths" => {
            let from = req_str(args, "from")?;
            let to = req_str(args, "to")?;
            let max_hops = opt_u64(args, "max_hops", 6)?;
            let max_paths = opt_u64(args, "max_paths", 50)?;
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
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            ("paths".to_string(), extra)
        }
        "reachable" => {
            let target = req_str(args, "target")?;
            let max_hops = opt_u64(args, "max_hops", 6)?;
            let limit = opt_u64(args, "limit", 200)?;
            let mut extra = vec![
                target.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--max-hops".into(),
                max_hops.to_string(),
                "--limit".into(),
                limit.to_string(),
            ];
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            ("reachable".to_string(), extra)
        }
        "check" => {
            let arr = read_canonical_array(args, "symbols", "names", &mut deprecated)?
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
            ("check".to_string(), extra)
        }
        "similar" => {
            let symbol = read_canonical_str(args, "symbol", "name", &mut deprecated)?
                .ok_or_else(|| ParamError::missing("symbol"))?;
            let limit = opt_u64(args, "limit", 10)?;
            let threshold = opt_f64(args, "threshold")?.unwrap_or(0.5);
            let mut extra = vec![
                symbol.to_string(),
                "--path".into(),
                project_root.to_string(),
                "--limit".into(),
                limit.to_string(),
                "--threshold".into(),
                threshold.to_string(),
            ];
            if let Some(filter) = opt_str(args, "filter")? {
                extra.extend(["--filter".into(), filter.to_string()]);
            }
            if opt_bool(args, "explain", false)? {
                extra.push("--explain".into());
            }
            if opt_bool(args, "why", false)? {
                extra.push("--why".into());
            }
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            push_diff_scope(&mut extra, args)?;
            ("similar".to_string(), extra)
        }
        "duplicates" => {
            let threshold = opt_f64(args, "threshold")?.unwrap_or(0.9);
            let limit = opt_u64(args, "limit", 50)?;
            let min_body_lines = opt_u64(args, "min_body_lines", 5)?;
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
            if let Some(filter) = opt_str(args, "filter")? {
                extra.extend(["--filter".into(), filter.to_string()]);
            }
            if opt_bool(args, "explain", false)? {
                extra.push("--explain".into());
            }
            if opt_bool(args, "why", false)? {
                extra.push("--why".into());
            }
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
            push_diff_scope(&mut extra, args)?;
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
            let mode = req_str(args, "mode")?;
            let mut extra: Vec<String> = vec!["--mode".into(), mode.into()];
            match mode {
                "symbol" => {
                    let symbol = opt_str(args, "symbol")?.ok_or_else(|| {
                        ParamError("`mode: symbol` requires the `symbol` field".to_string())
                    })?;
                    extra.extend(["--symbol".into(), symbol.into()]);
                    if let Some(v) = opt_u64_some(args, "callers_max")? {
                        extra.extend(["--callers-max".into(), v.to_string()]);
                    }
                    if let Some(v) = opt_u64_some(args, "callees_max")? {
                        extra.extend(["--callees-max".into(), v.to_string()]);
                    }
                    if let Some(v) = opt_u64_some(args, "similar_max")? {
                        extra.extend(["--similar-max".into(), v.to_string()]);
                    }
                }
                "pr-impact" => {
                    let base = opt_str(args, "base")?.ok_or_else(|| {
                        ParamError("`mode: pr-impact` requires the `base` field".to_string())
                    })?;
                    extra.extend(["--base".into(), base.into()]);
                    if let Some(d) = opt_u64_some(args, "depth")? {
                        extra.extend(["--depth".into(), d.to_string()]);
                    }
                    if let Some(m) = opt_u64_some(args, "tests_max")? {
                        extra.extend(["--tests-max".into(), m.to_string()]);
                    }
                }
                "project" => {
                    if let Some(g) = opt_str(args, "path_glob")? {
                        extra.extend(["--path-glob".into(), g.into()]);
                    }
                    if let Some(n) = opt_u64_some(args, "top_n")? {
                        extra.extend(["--top-n".into(), n.to_string()]);
                    }
                }
                other => {
                    return Err(ParamError(format!(
                        "unknown bundle mode `{other}` — expected one of `symbol`, `pr-impact`, `project`"
                    ))
                    .into());
                }
            }
            push_auto_update(&mut extra, args)?;
            push_no_stale_check(&mut extra, args)?;
            push_scope(&mut extra, args)?;
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
            "description": "Hybrid structural + semantic code search across the indexed codebase. Fuses FST exact + BM25 + semantic channels in a single ranked list (~4ms FST hit, ~7-15ms with semantic). Prefer over grep for symbol or identifier lookup — grep does a full-scan (seconds on large repos) and returns line matches; this returns ranked symbol records with kind, signature, and line ranges. Use this when you need to find a definition by name, signature shape, or meaning rather than guessing a regex. Supports `filter` (substring path filter), `kind` (kind-boost / restrict), `context_path` (proximity hint), `no_bm25` (disable BM25 channel), and `no_stale_check` (skip pre-call staleness probe).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Free-text query: symbol name, partial name, signature snippet, or natural-language description. Not for regex (use grep) or exact-only resolution (use find_symbol)." },
                    "limit": { "type": "integer", "description": "Max results", "default": 20 },
                    "semantic": { "type": "boolean", "description": "Enable the semantic vector channel (requires `vex index --semantic`); adds ~3-10ms but lets natural-language queries hit", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why` in the response: normalized query, per-channel hits (FST/BM25/semantic/fuzzy), filter_applied snapshot", "default": false },
                    "filter": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns)." },
                    "kind": { "type": "array", "items": { "type": "string" }, "description": "Boost results matching one or more kinds (repeatable). Canonical names (function, struct, class, …) plus aliases: def, comment, test, ref." },
                    "context_path": { "type": "string", "description": "Boost results near this file path (e.g. the agent's current editor file)." },
                    "no_bm25": { "type": "boolean", "description": "Disable the BM25 channel for this query (auto-on when the index has BM25 data otherwise).", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true (which already refreshes).", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (e.g. 'tests/**'); repeat for multiple globs" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include); repeat for multiple globs" },
                    "visibility": { "type": "string", "enum": ["public", "private", "protected", "internal"], "description": "Keep only symbols whose signature contains this explicit visibility keyword (no inferred defaults)" },
                    "async_only": { "type": "boolean", "description": "Keep only async/suspend functions", "default": false },
                    "no_async": { "type": "boolean", "description": "Exclude async/suspend functions", "default": false },
                    "static_only": { "type": "boolean", "description": "Keep only static class members", "default": false },
                    "sealed_only": { "type": "boolean", "description": "Keep only sealed (or Java-`final`) types", "default": false },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
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
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
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
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
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
                    "semantic": { "type": "boolean", "description": "Also generate per-symbol embeddings (enables semantic search / similar / duplicates; adds ~30-90s on a medium repo)", "default": false },
                    "gpu": { "type": "boolean", "description": "Use the GPU for embedding generation if this vex build supports it (DirectML on Windows / CoreML on macOS prebuilts; CUDA via source build), with silent CPU fallback. Only speeds up cold/large semantic builds. Omit to let .vex.toml gpu/device or $VEX_DEVICE decide; pass false to force CPU even when config enables GPU." },
                    "device": { "type": "string", "description": "Advanced: pin a specific embedding execution provider (cpu | auto | cuda | directml | coreml). Mutually exclusive with `gpu`." }
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
                    "semantic": { "type": "boolean", "description": "Also refresh embeddings for changed files", "default": false },
                    "gpu": { "type": "boolean", "description": "Use the GPU for embedding generation if this vex build supports it, with silent CPU fallback. Mostly a no-op for incremental updates (few/zero embeddings recomputed). Omit to let .vex.toml gpu/device or $VEX_DEVICE decide; pass false to force CPU." },
                    "device": { "type": "string", "description": "Advanced: pin a specific embedding execution provider (cpu | auto | cuda | directml | coreml). Mutually exclusive with `gpu`." }
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
            "name": "eval",
            "description": "Run the ranking-quality harness against a golden query set and return nDCG@10 / recall@10 / MRR per query and aggregated. Indexless in the sense that it never builds — consumes whatever index already lives at the project root (run `index` first if missing). Intended as a CI regression guard. MCP defaults to `json: true` so agents receive structured `EvalReport` JSON instead of the human-readable summary the CLI emits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bench": { "type": "string", "description": "Path to the golden-set TOML. Defaults to the bundled `benches/ranking_golden/queries.toml` on the CLI side; pass this when running against a fixture." },
                    "min_ndcg": { "type": "number", "description": "Fail with non-zero exit if mean nDCG@10 drops below this floor. Default 0.0 (always succeed). CI pins a recorded floor.", "default": 0.0 },
                    "json": { "type": "boolean", "description": "Emit the EvalReport as JSON to stdout. Default `true` in MCP context (agents want structured output) — note the CLI default is `false`. Set explicitly to `false` to fall back to the human-readable summary.", "default": true },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" }
                }
            }
        },
        {
            "name": "show",
            "description": "Extract the full source body of one or more symbols by name (function, class, struct, etc.) using cached symbol byte-offsets (~4ms per symbol). Prefer over Read when you need a specific definition — show returns just that body, while Read pulls the entire file (often 10-100x more tokens). Accepts an array, so a single call replaces several Read calls. Phase 13.3 truncation: `signature_only` (signature line only), `head` (first N body lines), `no_body` (signature + leading doc only), `collapsed` (collapse nested methods — v1.9 NO-OP). Also supports `filter` (substring path filter), `kind` (kind-restrict), `context_path` (proximity hint), and `no_stale_check`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Exact symbol names to extract — canonical key (v1.7+). Pass the array form even for a single symbol." },
                    "symbol": { "type": "string", "description": "DEPRECATED — use `symbols: [name]`. Pre-v1.7 singular alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max bodies returned per symbol name (handles overloads / duplicates)", "default": 1 },
                    "filter": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns)." },
                    "kind": { "type": "array", "items": { "type": "string" }, "description": "Boost results matching one or more kinds (repeatable). Same vocabulary as `search.kind`." },
                    "context_path": { "type": "string", "description": "Boost results near this file path (e.g. the agent's current editor file)." },
                    "signature_only": { "type": "boolean", "description": "Phase 13.3: print only the signature line(s). Mutually exclusive with `head`, `no_body`, `collapsed`.", "default": false },
                    "head": { "type": "integer", "minimum": 1, "description": "Phase 13.3: print only the first N body lines and append `... (M more lines)`. Mutually exclusive with `signature_only`, `no_body`, `collapsed`." },
                    "no_body": { "type": "boolean", "description": "Phase 13.3: print signature + leading docstring only; drop the body. Mutually exclusive with `signature_only`, `head`, `collapsed`.", "default": false },
                    "collapsed": { "type": "boolean", "description": "Phase 13.3: collapse nested methods inside a class/impl/module. v1.9 NO-OP (flag-shape stable; emits a stderr warning). Mutually exclusive with `signature_only`, `head`, `no_body`.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbols"]
            }
        },
        {
            "name": "usages",
            "description": "Find every reference to a symbol across the codebase. Prefer over grep for refactor-style `find all callers` queries — grep on a common identifier returns string-literal and comment noise; usages with strict=true uses the scope-binder to resolve real cross-file refs (Rust/TypeScript/Python/C#/C++). Without strict, runs the legacy refs FST (~4ms) but may include text-only matches. Supports `filter` (substring path filter) and `no_stale_check`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact symbol name to find references to — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "strict": { "type": "boolean", "description": "Use scope-resolved (type-aware) references from the binder — drops string-literal/comment/wrong-scope noise. Recommended for refactor work; falls back to legacy refs FST on languages without binder support.", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: mode (strict/fst_lookup), mode_legacy (back-compat alias for v1.9.x consumers, removed in v1.12), hits before/after path filter, prefix-suggestion count when no exact hits, filter snapshot.", "default": false },
                    "filter": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns)." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
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
            "description": "Find every concrete type that extends a base class / implements a trait / interface. Walks the indexed inheritance edges (covers generic-parameterised bases). Prefer over grep for `find all subclasses of Foo` — grep misses `: Foo<T>`, indirect inheritance, and trait impls; this resolves the real hierarchy. Supports diff scoping: `since` / `since_branched` / `changed_only` (mutually exclusive) to restrict to recently-touched code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact name of the base class / trait / interface — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "callers",
            "description": "Direct callers of a function via the persistent call-graph FST (~4ms when indexed; falls back to live-scan). Prefer over grep for `who calls Foo?` — grep on the function name hits doc comments and string literals; the call-graph edges are resolved at parse time. Phase 14.2 + 14.2.2 + 14.2.1: Python/Java function/method decorators, Kotlin annotations / C# method+constructor attributes, and TypeScript method decorators / Rust outer attributes on fns/methods emit forward edges, so `callers GetMapping` lists every Spring handler, `callers get` lists every FastAPI route, `callers HttpGet` every ASP.NET action, `callers JvmStatic` every Kotlin function annotated `@JvmStatic`, `callers Get` every Nest.js `@Get(...)`, `callers test` every Rust `#[tokio::test]` (the rightmost identifier of the decorator/attribute path becomes the callee; arguments are ignored — `#[serde(rename = \"x\")]` → `serde`, not `rename`). Rust `#[derive(...)]` is filtered (compile-time codegen, not call edges). Note the rightmost-identifier convention means `callers get` mixes decorator handlers with any regular `.get()` call — narrow with `include`/`exclude` if needed. Pair with `paths` for multi-hop chains. Supports diff scoping: `since` / `since_branched` / `changed_only` (mutually exclusive) to restrict callers to recently-touched code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact function name — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "callees",
            "description": "Direct callees of a function via the persistent call-graph FST (~4ms when indexed; falls back to live-scan). Prefer over Read+manual scanning when you want to know what a function calls without reading the whole body — callees gives the resolved outgoing edges as records. Phase 14.2 + 14.2.2 + 14.2.1: Python/Java decorators, Kotlin annotations, C# method/constructor attributes, TypeScript method decorators, and Rust outer attributes on fns/methods are surfaced as callees of the decorated function (decorator factories like `@lru_cache(maxsize=128)`, `@Inject`, `@Get(\"/x\")`, or `#[tokio::test]` appear as the path-rightmost identifier `lru_cache` / `Inject` / `Get` / `test` alongside regular body calls). Rust `#[derive(...)]` is intentionally filtered. Supports diff scoping: `since` / `since_branched` / `changed_only` (mutually exclusive) to restrict callees to recently-touched code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact function name — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "pattern",
            "description": "Structural AST pattern matching: match code by shape, not text. Metavars: `$NAME` captures an identifier or balanced expression, `$_` is a wildcard, `$$$` is an anonymous ellipsis, `$$$NAME` / `$$NAME` is a named ellipsis that captures multi-line bodies or arg lists, repeated metavars enforce back-reference equality. Composition: space-flanked ` && ` and ` || ` join sub-patterns (AND requires both shapes in the file with shared captures agreeing; OR takes the union). Prefer over grep / ast-grep for cross-language structural queries — grep cannot match nested syntax, and ast-grep needs per-language scripts; vex pattern works on the cached tree-sitter parse with a skeleton prefilter (~10-50ms). Set `why: true` to inspect indexed vs live-scan mode. Supports diff scoping: `since` (rev), `since_branched` (since this branch diverged from main), `changed_only` (working-tree changes) — mutually exclusive.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Structural pattern with $METAVARS (e.g. `fn $NAME($$ARGS) -> Result<$T, $E> { $$$BODY }`, `interface $N || class $N`). NOT regex — see grep for regex." },
                    "lang": { "type": "string", "description": "Language: rust, python, typescript, go, java, csharp, ruby, kotlin, swift, cpp, php, sql, markdown" },
                    "limit": { "type": "integer", "description": "Max matches to return", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false },
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
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
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
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
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
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false }
                },
                "required": ["symbols"]
            }
        },
        {
            "name": "similar",
            "description": "Nearest neighbours of an EXISTING symbol by its stored embedding (HNSW lookup, ~7-15ms). Distinct from find_similar (which embeds a free-text query). Use this when you have a function in hand and want `what else in this repo looks like it?` — useful for dedup, refactor planning, and finding parallel implementations. Requires `vex index --semantic`. Supports diff scoping: `since` (rev), `since_branched`, `changed_only` (mutually exclusive) and `no_stale_check`.",
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
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD`. Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
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
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["mode"]
            }
        },
        {
            "name": "duplicates",
            "description": "Repo-wide near-duplicate scan: pairs of symbols whose embeddings exceed `threshold`. Use for refactor planning (`where else does this logic live?`) and dedup. Prefer over manual similar-walks — duplicates evaluates all pairs once with `min_body_lines` filtering out trivial bodies. Requires `vex index --semantic`. Supports diff scoping: `since` (rev), `since_branched`, `changed_only` (mutually exclusive) and `no_stale_check`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "threshold": { "type": "number", "description": "Minimum cosine similarity (0.0..1.0); 0.9 keeps only very close pairs", "default": 0.9 },
                    "limit": { "type": "integer", "description": "Max pairs to return", "default": 50 },
                    "min_body_lines": { "type": "integer", "description": "Skip symbols with body shorter than this many lines (filters trivial wrappers)", "default": 5 },
                    "filter": { "type": "string", "description": "Substring path filter — keep pairs where at least one symbol's path contains this substring." },
                    "explain": { "type": "boolean", "description": "Include reasoning per pair: identifier-set Jaccard overlap + truncated unified diff between the two bodies", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: applied threshold + min_body_lines, pairs before/after path filter, filter snapshot.", "default": false },
                    "since": { "type": "string", "description": "Restrict pairs to files changed between `<rev>..HEAD`. Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict pairs to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict pairs to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
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
    /// JSON-RPC 2.0 §5.1 allows an optional `data` member for
    /// implementation-defined error detail. Used by the `-32700` path
    /// to echo a truncated copy of the unparseable input so the client
    /// dev can debug without consulting transport logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
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

    // --- S8.3 (v1.12.0): VEX_BIN + project_root validation ----------------

    /// All three VEX_BIN scenarios live in a single test because `cargo
    /// test` runs unit tests in parallel within the same binary and
    /// `std::env::set_var` is process-global — splitting them would race
    /// (one test's `set_var` clobbers another's snapshot). The scenarios
    /// are independent enough to assert sequentially in one body.
    #[test]
    fn resolve_vex_bin_validates_env_or_falls_back_to_path_literal() {
        let prior = std::env::var_os("VEX_BIN");

        // Scenario 1: VEX_BIN unset → falls back to literal "vex" so the
        // OS does PATH resolution at spawn time.
        // SAFETY: tests can race set_var/remove_var but we're inside one
        // test body that consolidates all three scenarios.
        unsafe {
            std::env::remove_var("VEX_BIN");
        }
        assert_eq!(resolve_vex_bin().expect("scenario 1 must succeed"), "vex");

        // Scenario 2: VEX_BIN points at a nonexistent file → clear error.
        unsafe {
            std::env::set_var("VEX_BIN", "/definitely/not/a/real/path/vex_xxx");
        }
        let err = resolve_vex_bin().expect_err("scenario 2 must fail");
        assert!(
            format!("{err}").contains("no such file"),
            "scenario 2 must mention 'no such file', got: {err}"
        );

        // Scenario 3: VEX_BIN points at a directory → clear error.
        // Use `env::temp_dir()` so the path exists on every platform —
        // hard-coding `/tmp` made Windows CI fall through to the
        // "no such file" branch instead of "not a regular file".
        let dir = std::env::temp_dir();
        unsafe {
            std::env::set_var("VEX_BIN", &dir);
        }
        let err = resolve_vex_bin().expect_err("scenario 3 must fail");
        assert!(
            format!("{err}").contains("not a regular file"),
            "scenario 3 must mention 'not a regular file', got: {err}"
        );

        // Restore prior state so neighbouring tests (and the broader
        // test binary) see no observable change.
        unsafe {
            match prior {
                Some(v) => std::env::set_var("VEX_BIN", v),
                None => std::env::remove_var("VEX_BIN"),
            }
        }
    }

    #[test]
    fn validate_project_root_passes_dot() {
        // "." is the implicit default; we don't canonicalize it so the
        // server's cwd wins. Validation must short-circuit on this case
        // so a server running in a non-existent cwd (impossible at the OS
        // level but possible inside containers with deleted dirs) is not
        // gratuitously rejected.
        validate_project_root(".").expect("'.' must pass");
    }

    #[test]
    fn validate_project_root_rejects_missing_path() {
        let err =
            validate_project_root("/definitely/not/a/real/directory/xxx").expect_err("must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("does not exist"),
            "error must mention 'does not exist', got: {msg}"
        );
    }

    #[test]
    fn validate_project_root_rejects_file_target() {
        // Cargo.toml exists in the crate root; safe target.
        let cargo_toml = std::env::current_dir().expect("cwd").join("Cargo.toml");
        let err = validate_project_root(cargo_toml.to_str().expect("utf-8"))
            .expect_err("must fail on a file path");
        let msg = format!("{err}");
        assert!(
            msg.contains("not a directory"),
            "error must mention 'not a directory', got: {msg}"
        );
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
    fn extract_why_trace_legacy_untagged_single_line_still_parses() {
        // Legacy fallback: an untagged JSON line still parses (older
        // CLIs on PATH at MCP-spawn time). v1.10.1 changed the legacy
        // scan from "first `{`-line" to "last `{`-line" so a leading
        // tracing::warn JSON doesn't shadow the trace — see
        // `extract_why_trace_falls_back_to_last_json_when_untagged`
        // for the load-bearing multi-line case.
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
        let stderr = "{ not really json\n";
        assert!(extract_why_trace(stderr).is_none());
    }

    #[test]
    fn extract_why_trace_prefers_vex_why_tag_over_earlier_json() {
        // Review S8.1 (v1.10.1): an early tracing::warn JSON line used to
        // shadow the real --why trace under _meta.why. With the
        // `VEX_WHY:` tag the extractor must pick the tagged line even
        // when an earlier `{`-prefixed line parses as JSON.
        let stderr = "\
{\"level\":\"WARN\",\"message\":\"cannot determine index freshness\"}\n\
VEX_WHY: {\"mode\":\"strict\",\"hits_before_filter\":3,\"hits_after_filter\":2}\n\
INFO trailing line\n\
";
        let trace = extract_why_trace(stderr).expect("tagged trace must be picked");
        assert_eq!(trace["mode"].as_str(), Some("strict"));
        assert_eq!(trace["hits_before_filter"].as_u64(), Some(3));
        assert!(
            trace.get("level").is_none(),
            "early warn-shaped JSON must not leak into the extracted trace; got: {trace}"
        );
    }

    #[test]
    fn extract_why_trace_falls_back_to_last_json_when_untagged() {
        // Older CLIs on PATH at MCP-spawn time emit the trace without
        // the `VEX_WHY:` tag — fall back to the last JSON-shaped line so
        // a leading warning doesn't shadow the real trace.
        let stderr = "\
{\"level\":\"WARN\",\"message\":\"cannot determine index freshness\"}\n\
{\"mode\":\"fst_lookup\",\"hits_before_filter\":7}\n\
";
        let trace = extract_why_trace(stderr).expect("legacy fallback must still find a trace");
        assert_eq!(trace["mode"].as_str(), Some("fst_lookup"));
        assert_eq!(trace["hits_before_filter"].as_u64(), Some(7));
    }

    #[test]
    fn extract_why_trace_tolerates_extra_whitespace_around_tag() {
        // Belt-and-suspenders: a leading space before `VEX_WHY:` or
        // trailing whitespace between the tag and the JSON must not
        // defeat extraction.
        let stderr =
            "   VEX_WHY:   {\"mode\":\"strict\",\"hits_before_filter\":1,\"hits_after_filter\":1}\n";
        let trace = extract_why_trace(stderr).expect("whitespace must not defeat the tag");
        assert_eq!(trace["mode"].as_str(), Some("strict"));
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

    // v1.19.1 — was tautological pre-refactor. Now calls `build_mcp_response`
    // directly so a regression in the production lifting logic actually
    // fails the test. Locks the Stage 3 contract that `protocol_version`
    // and `capabilities` reach top-level `result`.
    #[test]
    fn build_mcp_response_lifts_protocol_version_and_capabilities_to_top_level() {
        let mock_envelope = serde_json::json!({
            "protocol_version": "v1",
            "capabilities": {
                "signals": true, "empty_reason": false, "bundle_modes": [],
                "why": true, "scope_filters": true, "metadata_filters": true,
                "auto_update": true, "history_diff": true,
            },
            "_meta": { "vex.dev/index_age_ms": 42 },
            "results": []
        });
        let stdout = serde_json::to_string(&mock_envelope).unwrap();

        let result = build_mcp_response(
            Some(0),
            "exit status: 0".to_string(),
            &stdout,
            "",
            "search",
            &[],
            &json!({}),
        )
        .expect("envelope-shaped success must lift cleanly");

        assert_eq!(
            result["protocol_version"].as_str(),
            Some("v1"),
            "protocol_version must be lifted to top-level result; got: {result}"
        );
        assert_eq!(
            result["capabilities"]["signals"].as_bool(),
            Some(true),
            "capabilities must be lifted to top-level result; got: {result}"
        );
        assert!(
            result["structuredContent"]["results"].is_array(),
            "structuredContent.results must carry the lifted payload; got: {result}"
        );
    }

    /// v1.19.1 D1 — exit code 1 (the documented "no results found"
    /// CLI contract per `src/cli/exit_code.rs`) must pass through as a
    /// successful MCP response with `structuredContent.results = []`,
    /// NOT bail as a JSON-RPC `-32000` failure. Exit code 2 is a real
    /// error and must still bail with the legacy message shape so MCP
    /// clients can distinguish "clean 0 hits" from "tool fell over".
    ///
    /// Locks the post-fix contract that closed the field-report defect
    /// where `usages X --strict` with 0 hits became an MCP hard error.
    #[test]
    fn build_mcp_response_passes_exit_one_through_and_bails_on_exit_two() {
        // The CLI emits this exact envelope shape for an empty
        // `usages --strict` against a real symbol with 0 references
        // (reproduced live on 2026-06-22 against vex's own index).
        let empty_envelope = serde_json::json!({
            "protocol_version": "v1",
            "capabilities": {
                "signals": true, "empty_reason": false,
                "bundle_modes": ["symbol", "pr-impact", "project"],
                "why": true, "scope_filters": true, "metadata_filters": true,
                "auto_update": true, "history_diff": true,
            },
            "_meta": { "vex.dev/index_age_ms": 5 },
            "results": []
        });
        let stdout_exit1 = serde_json::to_string(&empty_envelope).unwrap();

        // Scenario 1: exit 1 + valid envelope on stdout → success.
        let ok = build_mcp_response(
            Some(1),
            "exit status: 1".to_string(),
            &stdout_exit1,
            "VEX_WHY: {\"mode\":\"strict\",\"hits_before_filter\":0,\"hits_after_filter\":0}\n",
            "usages",
            &[],
            &json!({}),
        )
        .expect(
            "exit 1 + envelope must pass through as success — the documented \
             empty-result contract; pre-fix this bailed as -32000",
        );
        assert_eq!(
            ok["structuredContent"]["results"].as_array().map(Vec::len),
            Some(0),
            "exit 1 must surface as structuredContent.results = []; got: {ok}"
        );
        assert_eq!(
            ok["capabilities"]["signals"].as_bool(),
            Some(true),
            "envelope capabilities must still lift on exit-1 path; got: {ok}"
        );

        // Scenario 2: exit 2 + arbitrary error payload → still bails.
        let err = build_mcp_response(
            Some(2),
            "exit status: 2".to_string(),
            "{\"error\":\"corrupt index\"}",
            "Error: index corrupted\n",
            "usages",
            &[],
            &json!({}),
        )
        .expect_err("exit 2 must continue to bail as a real error");
        let msg = format!("{err}");
        assert!(
            msg.contains("vex usages failed"),
            "exit 2 bail must mention the subcommand; got: {msg}"
        );
        assert!(
            msg.contains("corrupt index"),
            "exit 2 bail must surface stdout/stderr context; got: {msg}"
        );

        // Scenario 3: signal-kill (exit_code == None) — still bails so an
        // OOM-killed `vex` doesn't masquerade as an empty result.
        let err = build_mcp_response(
            None,
            "signal: 9 (SIGKILL)".to_string(),
            "",
            "",
            "search",
            &[],
            &json!({}),
        )
        .expect_err("signal-killed CLI must continue to bail");
        let msg = format!("{err}");
        assert!(
            msg.contains("vex search failed"),
            "signal-kill bail must mention the subcommand; got: {msg}"
        );

        // Scenario 4: exit 1 + malformed stdout (theoretical CLI bug).
        // The wrapper must still return success (per the exit-code
        // contract) but without an envelope-shaped `structuredContent`
        // — `content[0].text` carries `{"raw": "<stdout>"}` so the
        // client at least sees the raw output. Locks the fallback so a
        // future "always coerce to envelope" change doesn't silently
        // turn malformed-stdout into a JSON-RPC error.
        let ok = build_mcp_response(
            Some(1),
            "exit status: 1".to_string(),
            "not-an-envelope",
            "",
            "search",
            &[],
            &json!({}),
        )
        .expect("exit 1 + malformed stdout must still surface as success");
        assert!(
            ok.get("structuredContent").is_none(),
            "non-envelope stdout must NOT populate structuredContent; got: {ok}"
        );
        let text = ok["content"][0]["text"]
            .as_str()
            .expect("content[0].text must be present on the fallback path");
        assert!(
            text.contains("\"raw\""),
            "fallback path must surface raw stdout under content[0].text; got: {text}"
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

    // ── HIGH-tier CLI ↔ MCP parity gap closure ────────────────────────────

    // search: filter / kind / context_path / no_bm25

    #[test]
    fn search_filter_arg_pushes_filter_flag() {
        let extra = args_for("search", json!({"query": "Foo", "filter": "src/api/"}));
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--filter" && w[1] == "src/api/"),
            "search filter must surface as --filter; got: {extra:?}"
        );
    }

    #[test]
    fn search_kind_array_becomes_repeated_kind_flags() {
        let extra = args_for(
            "search",
            json!({"query": "Foo", "kind": ["fn", "method", "struct"]}),
        );
        let kinds: Vec<&str> = extra
            .windows(2)
            .filter_map(|w| (w[0] == "--kind").then_some(w[1].as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec!["fn", "method", "struct"],
            "search kind must emit one --kind pair per element; got: {extra:?}"
        );
    }

    #[test]
    fn search_context_path_pushes_context_path_flag() {
        let extra = args_for(
            "search",
            json!({"query": "Foo", "context_path": "src/main.rs"}),
        );
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--context-path" && w[1] == "src/main.rs"),
            "search context_path must surface as --context-path; got: {extra:?}"
        );
    }

    #[test]
    fn search_no_bm25_true_pushes_flag() {
        let extra = args_for("search", json!({"query": "Foo", "no_bm25": true}));
        assert!(
            extra.iter().any(|a| a == "--no-bm25"),
            "search no_bm25:true must add --no-bm25; got: {extra:?}"
        );
    }

    #[test]
    fn search_no_bm25_default_omits_flag() {
        let extra = args_for("search", json!({"query": "Foo"}));
        assert!(
            !extra.iter().any(|a| a == "--no-bm25"),
            "search without no_bm25 must not pass --no-bm25; got: {extra:?}"
        );
    }

    // show: filter / kind / context_path / signature_only / head / no_body / collapsed

    #[test]
    fn show_filter_arg_pushes_filter_flag() {
        let extra = args_for("show", json!({"symbols": ["Foo"], "filter": "tests/"}));
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--filter" && w[1] == "tests/"),
            "show filter must surface as --filter; got: {extra:?}"
        );
    }

    #[test]
    fn show_kind_array_becomes_repeated_kind_flags() {
        let extra = args_for(
            "show",
            json!({"symbols": ["Foo"], "kind": ["fn", "method"]}),
        );
        let kinds: Vec<&str> = extra
            .windows(2)
            .filter_map(|w| (w[0] == "--kind").then_some(w[1].as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec!["fn", "method"],
            "show kind must emit one --kind pair per element; got: {extra:?}"
        );
    }

    #[test]
    fn show_context_path_pushes_context_path_flag() {
        let extra = args_for(
            "show",
            json!({"symbols": ["Foo"], "context_path": "src/main.rs"}),
        );
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--context-path" && w[1] == "src/main.rs"),
            "show context_path must surface as --context-path; got: {extra:?}"
        );
    }

    #[test]
    fn show_signature_only_pushes_flag() {
        let extra = args_for("show", json!({"symbols": ["Foo"], "signature_only": true}));
        assert!(
            extra.iter().any(|a| a == "--signature-only"),
            "show signature_only must add --signature-only; got: {extra:?}"
        );
    }

    #[test]
    fn show_head_pushes_head_with_value() {
        let extra = args_for("show", json!({"symbols": ["Foo"], "head": 5}));
        assert!(
            extra.windows(2).any(|w| w[0] == "--head" && w[1] == "5"),
            "show head must surface as --head <N>; got: {extra:?}"
        );
    }

    #[test]
    fn show_no_body_pushes_flag() {
        let extra = args_for("show", json!({"symbols": ["Foo"], "no_body": true}));
        assert!(
            extra.iter().any(|a| a == "--no-body"),
            "show no_body must add --no-body; got: {extra:?}"
        );
    }

    #[test]
    fn show_collapsed_pushes_flag() {
        let extra = args_for("show", json!({"symbols": ["Foo"], "collapsed": true}));
        assert!(
            extra.iter().any(|a| a == "--collapsed"),
            "show collapsed must add --collapsed; got: {extra:?}"
        );
    }

    #[test]
    fn show_truncation_flags_are_mutually_exclusive() {
        let err = build_command(
            "show",
            &json!({"symbols": ["Foo"], "signature_only": true, "no_body": true}),
            "/tmp/proj",
        )
        .expect_err("mutually exclusive show truncation flags must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    #[test]
    fn show_no_truncation_flags_yields_clean_argv() {
        let extra = args_for("show", json!({"symbols": ["Foo"]}));
        for flag in ["--signature-only", "--head", "--no-body", "--collapsed"] {
            assert!(
                !extra.iter().any(|a| a == flag),
                "no truncation flag requested but {flag} present; got: {extra:?}"
            );
        }
    }

    // usages: filter

    #[test]
    fn usages_filter_arg_pushes_filter_flag() {
        let extra = args_for("usages", json!({"symbol": "Foo", "filter": "src/"}));
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--filter" && w[1] == "src/"),
            "usages filter must surface as --filter; got: {extra:?}"
        );
    }

    // pattern / similar / duplicates: diff scope

    #[test]
    fn pattern_since_pushes_since_with_value() {
        let extra = args_for(
            "pattern",
            json!({"pattern": "fn $N()", "lang": "rust", "since": "main"}),
        );
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--since" && w[1] == "main"),
            "pattern since must surface as --since <rev>; got: {extra:?}"
        );
    }

    #[test]
    fn pattern_since_branched_pushes_flag() {
        let extra = args_for(
            "pattern",
            json!({"pattern": "fn $N()", "lang": "rust", "since_branched": true}),
        );
        assert!(
            extra.iter().any(|a| a == "--since-branched"),
            "pattern since_branched must add --since-branched; got: {extra:?}"
        );
    }

    #[test]
    fn pattern_changed_only_pushes_flag() {
        let extra = args_for(
            "pattern",
            json!({"pattern": "fn $N()", "lang": "rust", "changed_only": true}),
        );
        assert!(
            extra.iter().any(|a| a == "--changed-only"),
            "pattern changed_only must add --changed-only; got: {extra:?}"
        );
    }

    #[test]
    fn pattern_diff_scope_flags_mutually_exclusive() {
        let err = build_command(
            "pattern",
            &json!({
                "pattern": "fn $N()",
                "lang": "rust",
                "since": "main",
                "since_branched": true,
            }),
            "/tmp/proj",
        )
        .expect_err("mutually exclusive diff-scope flags must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    #[test]
    fn similar_since_pushes_since_with_value() {
        let extra = args_for("similar", json!({"symbol": "Foo", "since": "HEAD~3"}));
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--since" && w[1] == "HEAD~3"),
            "similar since must surface as --since <rev>; got: {extra:?}"
        );
    }

    #[test]
    fn similar_changed_only_pushes_flag() {
        let extra = args_for("similar", json!({"symbol": "Foo", "changed_only": true}));
        assert!(
            extra.iter().any(|a| a == "--changed-only"),
            "similar changed_only must add --changed-only; got: {extra:?}"
        );
    }

    #[test]
    fn duplicates_since_branched_pushes_flag() {
        let extra = args_for("duplicates", json!({"since_branched": true}));
        assert!(
            extra.iter().any(|a| a == "--since-branched"),
            "duplicates since_branched must add --since-branched; got: {extra:?}"
        );
    }

    // no_stale_check — applied to every tool that already accepted auto_update.

    #[test]
    fn no_stale_check_default_omits_flag_across_tools() {
        // Default-off — no MCP client should see surprise behaviour.
        let cases: Vec<(&str, Value)> = vec![
            ("search", json!({"query": "Foo"})),
            ("find_symbol", json!({"symbol": "Foo"})),
            ("find_similar", json!({"query": "Foo"})),
            ("show", json!({"symbols": ["Foo"]})),
            ("usages", json!({"symbol": "Foo"})),
            ("implementations", json!({"symbol": "Foo"})),
            ("callers", json!({"symbol": "Foo"})),
            ("callees", json!({"symbol": "Foo"})),
            ("paths", json!({"from": "A", "to": "B"})),
            ("reachable", json!({"target": "Foo"})),
            ("check", json!({"symbols": ["Foo"]})),
            ("similar", json!({"symbol": "Foo"})),
            ("duplicates", json!({})),
            ("bundle", json!({"mode": "project"})),
        ];
        for (tool, args) in cases {
            let extra = args_for(tool, args);
            assert!(
                !extra.iter().any(|a| a == "--no-stale-check"),
                "{tool} default must not pass --no-stale-check; got: {extra:?}"
            );
        }
    }

    #[test]
    fn no_stale_check_true_pushes_flag_across_tools() {
        let cases: Vec<(&str, Value)> = vec![
            ("search", json!({"query": "Foo", "no_stale_check": true})),
            (
                "find_symbol",
                json!({"symbol": "Foo", "no_stale_check": true}),
            ),
            (
                "find_similar",
                json!({"query": "Foo", "no_stale_check": true}),
            ),
            ("show", json!({"symbols": ["Foo"], "no_stale_check": true})),
            ("usages", json!({"symbol": "Foo", "no_stale_check": true})),
            (
                "implementations",
                json!({"symbol": "Foo", "no_stale_check": true}),
            ),
            ("callers", json!({"symbol": "Foo", "no_stale_check": true})),
            ("callees", json!({"symbol": "Foo", "no_stale_check": true})),
            (
                "paths",
                json!({"from": "A", "to": "B", "no_stale_check": true}),
            ),
            (
                "reachable",
                json!({"target": "Foo", "no_stale_check": true}),
            ),
            ("check", json!({"symbols": ["Foo"], "no_stale_check": true})),
            ("similar", json!({"symbol": "Foo", "no_stale_check": true})),
            ("duplicates", json!({"no_stale_check": true})),
            ("bundle", json!({"mode": "project", "no_stale_check": true})),
        ];
        for (tool, args) in cases {
            let extra = args_for(tool, args);
            assert!(
                extra.iter().any(|a| a == "--no-stale-check"),
                "{tool} no_stale_check:true must add --no-stale-check; got: {extra:?}"
            );
        }
    }

    #[test]
    fn no_stale_check_appears_in_every_relevant_schema() {
        // Schema-regression guard: every tool that accepts `auto_update`
        // must also expose `no_stale_check` so MCP clients discover the
        // companion flag via tools/list.
        let desc = tool_descriptors();
        let tools = desc.as_array().expect("tool_descriptors returns array");
        for name in [
            "search",
            "find_symbol",
            "find_similar",
            "show",
            "usages",
            "implementations",
            "callers",
            "callees",
            "paths",
            "reachable",
            "check",
            "similar",
            "duplicates",
            "bundle",
        ] {
            let entry = tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            let props = &entry["inputSchema"]["properties"];
            assert!(
                props["no_stale_check"].is_object(),
                "{name} schema must expose no_stale_check: {props}"
            );
            assert_eq!(
                props["no_stale_check"]["type"].as_str(),
                Some("boolean"),
                "{name} no_stale_check must be boolean"
            );
        }
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

    // ── HIGH-tier reviewer follow-up: absence-of-flag guards ──────────────
    // `search_filter_arg_pushes_filter_flag` and siblings assert that the
    // CLI flag appears when the MCP arg is set; the omitted-by-default
    // path was untested. A future change accidentally always-pushing
    // `--filter` (or `--kind`, `--context-path`) would slip past the
    // present-when-set tests — these absence guards close that gap.

    #[test]
    fn search_filter_default_omits_flag() {
        let extra = args_for("search", json!({"query": "Foo"}));
        assert!(
            !extra.iter().any(|a| a == "--filter"),
            "search without filter must not pass --filter; got: {extra:?}"
        );
    }

    #[test]
    fn search_kind_default_omits_flag() {
        let extra = args_for("search", json!({"query": "Foo"}));
        assert!(
            !extra.iter().any(|a| a == "--kind"),
            "search without kind must not pass --kind; got: {extra:?}"
        );
    }

    #[test]
    fn search_context_path_default_omits_flag() {
        let extra = args_for("search", json!({"query": "Foo"}));
        assert!(
            !extra.iter().any(|a| a == "--context-path"),
            "search without context_path must not pass --context-path; got: {extra:?}"
        );
    }

    #[test]
    fn show_filter_default_omits_flag() {
        let extra = args_for("show", json!({"symbols": ["Foo"]}));
        assert!(
            !extra.iter().any(|a| a == "--filter"),
            "show without filter must not pass --filter; got: {extra:?}"
        );
    }

    #[test]
    fn show_kind_default_omits_flag() {
        let extra = args_for("show", json!({"symbols": ["Foo"]}));
        assert!(
            !extra.iter().any(|a| a == "--kind"),
            "show without kind must not pass --kind; got: {extra:?}"
        );
    }

    #[test]
    fn show_context_path_default_omits_flag() {
        let extra = args_for("show", json!({"symbols": ["Foo"]}));
        assert!(
            !extra.iter().any(|a| a == "--context-path"),
            "show without context_path must not pass --context-path; got: {extra:?}"
        );
    }

    // ── HIGH-tier reviewer follow-up: head input validation ───────────────
    // `head` is a positive integer. `serde_json::Value::as_u64()` returns
    // `None` for negatives and floats — silently dropping the bad value
    // hides bugs. Surface them as JSON-RPC errors instead. `head: 0` is
    // also rejected (CLI would otherwise reject it too).

    #[test]
    fn show_head_zero_returns_error() {
        let err = build_command("show", &json!({"symbols": ["Foo"], "head": 0}), "/tmp/proj")
            .expect_err("head: 0 must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("integer") && msg.contains("head"),
            "error should mention `head` and integer-typed expectation; got: {msg}"
        );
    }

    #[test]
    fn show_head_negative_returns_error() {
        let err = build_command(
            "show",
            &json!({"symbols": ["Foo"], "head": -1}),
            "/tmp/proj",
        )
        .expect_err("head: -1 must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("integer") && msg.contains("head"),
            "error should mention `head` and integer-typed expectation; got: {msg}"
        );
    }

    #[test]
    fn show_head_float_returns_error() {
        let err = build_command(
            "show",
            &json!({"symbols": ["Foo"], "head": 5.5}),
            "/tmp/proj",
        )
        .expect_err("head: 5.5 must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("integer") && msg.contains("head"),
            "error should mention `head` and integer-typed expectation; got: {msg}"
        );
    }

    // ── HIGH-tier reviewer follow-up: diff-scope parity on search/usages ──
    // The diff added diff-scope wiring to pattern/similar/duplicates but
    // overlooked search and usages even though both already flatten
    // `DiffFilterArgs` in `src/cli/args.rs`. Close the audit hole.

    #[test]
    fn search_since_pushes_since_flag() {
        let extra = args_for("search", json!({"query": "Foo", "since": "main"}));
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--since" && w[1] == "main"),
            "search since must surface as --since <rev>; got: {extra:?}"
        );
    }

    #[test]
    fn search_since_branched_pushes_flag() {
        let extra = args_for("search", json!({"query": "Foo", "since_branched": true}));
        assert!(
            extra.iter().any(|a| a == "--since-branched"),
            "search since_branched must add --since-branched; got: {extra:?}"
        );
    }

    #[test]
    fn search_changed_only_pushes_flag() {
        let extra = args_for("search", json!({"query": "Foo", "changed_only": true}));
        assert!(
            extra.iter().any(|a| a == "--changed-only"),
            "search changed_only must add --changed-only; got: {extra:?}"
        );
    }

    #[test]
    fn search_diff_scope_flags_mutually_exclusive() {
        let err = build_command(
            "search",
            &json!({"query": "Foo", "since": "main", "since_branched": true}),
            "/tmp/proj",
        )
        .expect_err("mutually exclusive diff-scope flags must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    #[test]
    fn usages_since_pushes_since_flag() {
        let extra = args_for("usages", json!({"symbol": "Foo", "since": "HEAD~3"}));
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--since" && w[1] == "HEAD~3"),
            "usages since must surface as --since <rev>; got: {extra:?}"
        );
    }

    #[test]
    fn usages_since_branched_pushes_flag() {
        let extra = args_for("usages", json!({"symbol": "Foo", "since_branched": true}));
        assert!(
            extra.iter().any(|a| a == "--since-branched"),
            "usages since_branched must add --since-branched; got: {extra:?}"
        );
    }

    #[test]
    fn usages_changed_only_pushes_flag() {
        let extra = args_for("usages", json!({"symbol": "Foo", "changed_only": true}));
        assert!(
            extra.iter().any(|a| a == "--changed-only"),
            "usages changed_only must add --changed-only; got: {extra:?}"
        );
    }

    #[test]
    fn usages_diff_scope_flags_mutually_exclusive() {
        let err = build_command(
            "usages",
            &json!({"symbol": "Foo", "since": "main", "changed_only": true}),
            "/tmp/proj",
        )
        .expect_err("mutually exclusive diff-scope flags must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    // ── HIGH-tier reviewer follow-up: exhaustive diff-scope pair coverage ─
    // `pattern_diff_scope_flags_mutually_exclusive` only covered the
    // `since + since_branched` pair. The shared `push_diff_scope` helper
    // enforces all three pairs — round out the matrix for pattern, plus
    // one pair per other tool (similar, duplicates) since the helper is
    // shared.

    #[test]
    fn pattern_diff_scope_since_plus_changed_only_errors() {
        let err = build_command(
            "pattern",
            &json!({
                "pattern": "fn $N()",
                "lang": "rust",
                "since": "main",
                "changed_only": true,
            }),
            "/tmp/proj",
        )
        .expect_err("since + changed_only must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    #[test]
    fn pattern_diff_scope_since_branched_plus_changed_only_errors() {
        let err = build_command(
            "pattern",
            &json!({
                "pattern": "fn $N()",
                "lang": "rust",
                "since_branched": true,
                "changed_only": true,
            }),
            "/tmp/proj",
        )
        .expect_err("since_branched + changed_only must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    #[test]
    fn similar_diff_scope_flags_mutually_exclusive() {
        let err = build_command(
            "similar",
            &json!({"symbol": "Foo", "since": "main", "since_branched": true}),
            "/tmp/proj",
        )
        .expect_err("similar mutually exclusive diff-scope flags must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    #[test]
    fn duplicates_diff_scope_flags_mutually_exclusive() {
        let err = build_command(
            "duplicates",
            &json!({"since": "main", "changed_only": true}),
            "/tmp/proj",
        )
        .expect_err("duplicates mutually exclusive diff-scope flags must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    // ── FU-2b: diff-scope on call-graph tools ────────────────────────────
    // CLI's `Commands::Callers` / `Callees` / `Implementations` already
    // flatten `DiffFilterArgs` (see src/cli/args.rs:556-627). MCP forwards
    // through the shared `push_diff_scope` helper. One presence + one
    // mutual-exclusion test per tool covers the wiring; the helper itself
    // is exhaustively tested for pair combinations above on `pattern`.

    #[test]
    fn callers_since_pushes_since_flag() {
        let extra = args_for("callers", json!({"symbol": "Foo", "since": "main"}));
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--since" && w[1] == "main"),
            "callers since must surface as --since <rev>; got: {extra:?}"
        );
    }

    #[test]
    fn callers_diff_scope_flags_mutually_exclusive() {
        let err = build_command(
            "callers",
            &json!({"symbol": "Foo", "since": "main", "since_branched": true}),
            "/tmp/proj",
        )
        .expect_err("callers mutually exclusive diff-scope flags must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    #[test]
    fn callees_since_pushes_since_flag() {
        let extra = args_for("callees", json!({"symbol": "Bar", "since": "HEAD~2"}));
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--since" && w[1] == "HEAD~2"),
            "callees since must surface as --since <rev>; got: {extra:?}"
        );
    }

    #[test]
    fn callees_diff_scope_flags_mutually_exclusive() {
        let err = build_command(
            "callees",
            &json!({"symbol": "Bar", "since_branched": true, "changed_only": true}),
            "/tmp/proj",
        )
        .expect_err("callees mutually exclusive diff-scope flags must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    #[test]
    fn implementations_since_pushes_since_flag() {
        let extra = args_for(
            "implementations",
            json!({"symbol": "Trait", "since": "origin/main"}),
        );
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--since" && w[1] == "origin/main"),
            "implementations since must surface as --since <rev>; got: {extra:?}"
        );
    }

    #[test]
    fn implementations_diff_scope_flags_mutually_exclusive() {
        let err = build_command(
            "implementations",
            &json!({"symbol": "Trait", "since": "main", "changed_only": true}),
            "/tmp/proj",
        )
        .expect_err("implementations mutually exclusive diff-scope flags must error");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("mutually exclusive"),
            "error should call out the mutual exclusion; got: {msg}"
        );
    }

    #[test]
    fn diff_scope_flags_default_omitted_across_call_graph_tools() {
        // Belt-and-suspenders absence guard: the empty-args case must NOT
        // accidentally push diff-scope flags. Mirrors the existing
        // `no_stale_check_default_omits_flag_across_tools` table test, but
        // narrowed to the FU-2b-affected tools where diff-scope is new.
        let cases: Vec<(&str, Value)> = vec![
            ("callers", json!({"symbol": "Foo"})),
            ("callees", json!({"symbol": "Foo"})),
            ("implementations", json!({"symbol": "Foo"})),
        ];
        for (tool, args) in cases {
            let extra = args_for(tool, args);
            for flag in ["--since", "--since-branched", "--changed-only"] {
                assert!(
                    !extra.iter().any(|a| a == flag),
                    "{tool} default must not pass {flag}; got: {extra:?}"
                );
            }
        }
    }

    // ── FU-1: `eval` MCP wrapper ─────────────────────────────────────────
    // Thin forwarder for `vex eval --path <ROOT> [--bench <PATH>]
    // [--min-ndcg <F64>] [--json]`. Note: MCP defaults `json` to true
    // (CLI default is false) so agents always get structured output.

    #[test]
    fn eval_default_args_pushes_json_flag() {
        let extra = args_for("eval", json!({}));
        // --path <root> always present.
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--path" && w[1] == "/tmp/proj"),
            "eval must always forward --path; got: {extra:?}"
        );
        // MCP defaults to JSON.
        assert!(
            extra.iter().any(|a| a == "--json"),
            "eval defaults json=true in MCP — --json must be present; got: {extra:?}"
        );
        // Neither optional flag should leak in when its key is omitted.
        assert!(
            !extra.iter().any(|a| a == "--bench"),
            "eval without bench must not pass --bench; got: {extra:?}"
        );
        assert!(
            !extra.iter().any(|a| a == "--min-ndcg"),
            "eval without min_ndcg must not pass --min-ndcg; got: {extra:?}"
        );
    }

    #[test]
    fn eval_bench_path_arg_pushes_bench_flag() {
        let extra = args_for("eval", json!({"bench": "/tmp/golden.toml"}));
        assert!(
            extra
                .windows(2)
                .any(|w| w[0] == "--bench" && w[1] == "/tmp/golden.toml"),
            "eval bench must surface as --bench; got: {extra:?}"
        );
    }

    #[test]
    fn eval_min_ndcg_arg_pushes_min_ndcg_flag() {
        let extra = args_for("eval", json!({"min_ndcg": 0.75}));
        let value = extra
            .windows(2)
            .find_map(|w| (w[0] == "--min-ndcg").then(|| w[1].clone()))
            .expect("eval min_ndcg must surface as --min-ndcg");
        // f64::to_string can render 0.75 with platform-specific precision;
        // assert via numeric parse to stay robust to formatting.
        let parsed: f64 = value
            .parse()
            .unwrap_or_else(|_| panic!("--min-ndcg value should parse as f64; got: {value}"));
        assert!(
            (parsed - 0.75).abs() < 1e-9,
            "eval --min-ndcg value mismatch: {value}"
        );
    }

    #[test]
    fn eval_json_false_omits_json_flag() {
        // MCP default flip is overridable — passing json:false must opt
        // back into the CLI's human-readable summary.
        let extra = args_for("eval", json!({"json": false}));
        assert!(
            !extra.iter().any(|a| a == "--json"),
            "eval json=false must NOT pass --json; got: {extra:?}"
        );
    }

    #[test]
    fn eval_schema_exposes_expected_properties() {
        // Schema regression guard: the `eval` tool must exist in
        // tool_descriptors() and expose exactly the 4 documented
        // properties (bench, min_ndcg, json, project_root). Catches
        // accidental schema drift or copy-paste leakage from neighbouring
        // tools.
        let desc = tool_descriptors();
        let tools = desc.as_array().expect("tool_descriptors returns array");
        let entry = tools
            .iter()
            .find(|t| t["name"] == "eval")
            .expect("eval tool descriptor missing");
        let props = entry["inputSchema"]["properties"]
            .as_object()
            .expect("eval inputSchema.properties must be an object");
        let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["bench", "json", "min_ndcg", "project_root"],
            "eval schema must expose exactly bench/min_ndcg/json/project_root"
        );
        // Type sanity.
        assert_eq!(props["bench"]["type"].as_str(), Some("string"));
        assert_eq!(props["min_ndcg"]["type"].as_str(), Some("number"));
        assert_eq!(props["json"]["type"].as_str(), Some("boolean"));
        assert_eq!(props["project_root"]["type"].as_str(), Some("string"));
        // MCP default flip lives in the schema too — surface for clients.
        assert_eq!(
            props["json"]["default"].as_bool(),
            Some(true),
            "eval schema must advertise json default=true (MCP override of CLI default)"
        );
        // No required fields — eval should run with `{}`.
        assert!(
            entry["inputSchema"].get("required").is_none(),
            "eval schema must NOT mark any field required"
        );
    }

    // ── FU-4: include / exclude glob parity across all path-aware tools ──
    // Every MCP tool whose CLI variant flattens `ScopeArgs` in
    // `src/cli/args.rs` must (a) forward the `include` / `exclude` arrays
    // verbatim through `push_scope`, and (b) advertise both fields in its
    // inputSchema so MCP clients can discover them via `tools/list`.
    //
    // The three tests below are dispatch-presence + dispatch-presence +
    // schema-regression guards. The shared `push_scope` helper means one
    // tool's presence test already covers the wiring; the table form here
    // is belt-and-suspenders against an arm forgetting the helper call
    // after a copy-paste of an unrelated dispatch path.

    /// Single source of truth for the 16 tools that the FU-4 audit
    /// confirmed accept `--include` / `--exclude` on the CLI side. Used
    /// by both the presence tests and the schema regression guard so
    /// drift in one place is caught by the others.
    fn fu4_scope_aware_tool_cases() -> Vec<(&'static str, Value)> {
        vec![
            ("search", json!({"query": "Foo"})),
            ("find_symbol", json!({"symbol": "Foo"})),
            ("find_similar", json!({"query": "Foo"})),
            ("show", json!({"symbols": ["Foo"]})),
            ("usages", json!({"symbol": "Foo"})),
            ("grep", json!({"pattern": "Foo"})),
            ("implementations", json!({"symbol": "Foo"})),
            ("callers", json!({"symbol": "Foo"})),
            ("callees", json!({"symbol": "Foo"})),
            ("pattern", json!({"pattern": "fn $N()", "lang": "rust"})),
            ("diff", json!({"base": "main"})),
            ("paths", json!({"from": "A", "to": "B"})),
            ("reachable", json!({"target": "Foo"})),
            ("similar", json!({"symbol": "Foo"})),
            ("duplicates", json!({})),
            ("bundle", json!({"mode": "project"})),
        ]
    }

    #[test]
    fn include_glob_pushes_include_flag_across_tools() {
        // Each glob round-trips as a separate `--include` flag so clap's
        // `Vec<String>` accumulator on the CLI side sees one value per
        // flag (the standard repeatable-arg shape).
        for (tool, base_args) in fu4_scope_aware_tool_cases() {
            let mut args = base_args.clone();
            args["include"] = json!(["foo/**", "bar/**"]);
            let extra = args_for(tool, args);
            let pairs: Vec<&str> = extra.iter().map(String::as_str).collect();
            let window =
                |flag: &str, val: &str| pairs.windows(2).any(|w| w[0] == flag && w[1] == val);
            assert!(
                window("--include", "foo/**"),
                "{tool} include[0] must surface as --include foo/**; got: {pairs:?}"
            );
            assert!(
                window("--include", "bar/**"),
                "{tool} include[1] must surface as --include bar/**; got: {pairs:?}"
            );
            let occurrences = pairs.iter().filter(|a| **a == "--include").count();
            assert_eq!(
                occurrences, 2,
                "{tool} should push exactly two --include flags; got: {pairs:?}"
            );
        }
    }

    #[test]
    fn exclude_glob_pushes_exclude_flag_across_tools() {
        for (tool, base_args) in fu4_scope_aware_tool_cases() {
            let mut args = base_args.clone();
            args["exclude"] = json!(["**/generated/**", "vendor/**"]);
            let extra = args_for(tool, args);
            let pairs: Vec<&str> = extra.iter().map(String::as_str).collect();
            let window =
                |flag: &str, val: &str| pairs.windows(2).any(|w| w[0] == flag && w[1] == val);
            assert!(
                window("--exclude", "**/generated/**"),
                "{tool} exclude[0] must surface as --exclude **/generated/**; got: {pairs:?}"
            );
            assert!(
                window("--exclude", "vendor/**"),
                "{tool} exclude[1] must surface as --exclude vendor/**; got: {pairs:?}"
            );
            let occurrences = pairs.iter().filter(|a| **a == "--exclude").count();
            assert_eq!(
                occurrences, 2,
                "{tool} should push exactly two --exclude flags; got: {pairs:?}"
            );
        }
    }

    #[test]
    fn include_exclude_schemas_complete_across_tools() {
        // Schema-regression guard: every FU-4 scope-aware tool must
        // expose both `include` and `exclude` as `array<string>` so the
        // MCP client's `tools/list` view advertises the filter shape.
        let desc = tool_descriptors();
        let tools = desc.as_array().expect("tool_descriptors returns array");
        for (name, _) in fu4_scope_aware_tool_cases() {
            let entry = tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            let props = &entry["inputSchema"]["properties"];
            for field in ["include", "exclude"] {
                let prop = &props[field];
                assert!(
                    prop.is_object(),
                    "{name} schema is missing {field}: {props}"
                );
                assert_eq!(
                    prop["type"].as_str(),
                    Some("array"),
                    "{name}.{field} must be type=array"
                );
                assert_eq!(
                    prop["items"]["type"].as_str(),
                    Some("string"),
                    "{name}.{field}.items must be type=string"
                );
            }
        }
    }

    // -----------------------------------------------------------------
    // C4 — JSON-RPC parse-error response + stdin-error resilience.
    // Regression guard: a malformed input line must produce a
    // spec-compliant `-32700` frame with `id: null`, and the loop must
    // continue serving subsequent valid requests.
    // -----------------------------------------------------------------

    #[test]
    fn malformed_line_produces_parse_error_then_keeps_serving() {
        // Two lines: garbage (must elicit -32700) + a valid `ping`
        // request (must get its normal response). The loop must NOT
        // terminate on the parse error.
        let input = b"{not valid json\n\
            {\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"ping\"}\n"
            .to_vec();
        let reader = std::io::BufReader::new(&input[..]);
        let mut output: Vec<u8> = Vec::new();
        run_loop(reader, &mut output).expect("loop exits cleanly");

        let stdout = String::from_utf8(output).expect("stdout is utf8");
        let mut lines = stdout.lines();

        // Frame 1 — parse error.
        let parse_err: Value =
            serde_json::from_str(lines.next().expect("parse-error frame present"))
                .expect("frame 1 is JSON");
        assert_eq!(parse_err["jsonrpc"].as_str(), Some("2.0"));
        // §5.1: id is explicitly null (not omitted).
        assert!(
            parse_err["id"].is_null(),
            "parse-error id must be JSON null, got: {}",
            parse_err["id"]
        );
        assert_eq!(parse_err["error"]["code"].as_i64(), Some(-32700));
        assert_eq!(parse_err["error"]["message"].as_str(), Some("Parse error"));
        // The truncated echo is best-effort but must be present so a
        // future regression that drops it surfaces here.
        assert!(
            parse_err["error"]["data"].is_string(),
            "parse-error data must echo the raw input fragment, got: {}",
            parse_err["error"]["data"]
        );

        // Frame 2 — valid `ping` request still gets answered.
        let ping_resp: Value = serde_json::from_str(lines.next().expect("ping response present"))
            .expect("frame 2 is JSON");
        assert_eq!(ping_resp["jsonrpc"].as_str(), Some("2.0"));
        assert_eq!(ping_resp["id"].as_i64(), Some(42));
        assert!(
            ping_resp["error"].is_null(),
            "ping must return result, not error: {ping_resp}"
        );
        assert!(
            ping_resp["result"].is_object(),
            "ping result must be a JSON object: {ping_resp}"
        );

        assert!(
            lines.next().is_none(),
            "exactly two response frames expected"
        );
    }

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

    // ---- H8: typed-params contract ----
    //
    // Every `build_command` path that consumes a typed argument must
    // emit a `ParamError` when the JSON value carries the wrong type.
    // `handle_request` downcasts to `ParamError` and maps it to the
    // JSON-RPC spec's `-32602 Invalid params`.

    /// Convenience: run `build_command` and require that the error
    /// downcasts to `ParamError` containing `needle` in its message.
    #[track_caller]
    fn assert_param_error(tool: &str, args: Value, needle: &str) {
        let err = build_command(tool, &args, "/tmp/proj")
            .err()
            .unwrap_or_else(|| panic!("expected ParamError for `{tool}` with args {args}"));
        let pe = err.downcast_ref::<ParamError>().unwrap_or_else(|| {
            panic!("expected ParamError for `{tool}`; got non-param error: {err:#}")
        });
        assert!(
            pe.0.contains(needle),
            "ParamError for `{tool}` must mention `{needle}`; got: {msg}",
            msg = pe.0
        );
    }

    #[test]
    fn search_missing_query_is_invalid_params() {
        assert_param_error("search", json!({}), "query");
    }

    #[test]
    fn search_string_limit_is_invalid_params() {
        assert_param_error("search", json!({"query": "foo", "limit": "20"}), "limit");
    }

    #[test]
    fn search_string_auto_update_is_invalid_params() {
        assert_param_error(
            "search",
            json!({"query": "foo", "auto_update": "true"}),
            "auto_update",
        );
    }

    #[test]
    fn search_kind_string_instead_of_array_is_invalid_params() {
        assert_param_error("search", json!({"query": "foo", "kind": "fn"}), "kind");
    }

    #[test]
    fn callers_missing_symbol_is_invalid_params() {
        // Canonical field is `symbol` (legacy alias `name` falls back to
        // it inside `read_canonical_str`). When both are absent we report
        // the canonical name in the error so callers see the
        // up-to-date schema vocabulary.
        assert_param_error("callers", json!({}), "symbol");
    }

    #[test]
    fn callees_missing_symbol_is_invalid_params() {
        assert_param_error("callees", json!({}), "symbol");
    }

    // ---- per-tool required-field coverage (H8 review follow-up) ----

    #[test]
    fn paths_missing_from_is_invalid_params() {
        assert_param_error("paths", json!({"to": "bar"}), "from");
    }

    #[test]
    fn paths_missing_to_is_invalid_params() {
        assert_param_error("paths", json!({"from": "foo"}), "to");
    }

    #[test]
    fn reachable_missing_target_is_invalid_params() {
        assert_param_error("reachable", json!({}), "target");
    }

    #[test]
    fn diff_missing_base_is_invalid_params() {
        assert_param_error("diff", json!({}), "base");
    }

    #[test]
    fn show_missing_symbols_is_invalid_params() {
        assert_param_error("show", json!({}), "symbols");
    }

    #[test]
    fn show_missing_field_error_mentions_legacy_alias_symbol() {
        // An LLM agent that hallucinated `symbol` (singular) or sent no
        // matching field at all should see the legacy alias in the
        // error so the recovery is one prompt-of-context away.
        let err =
            build_command("show", &json!({}), "/tmp/proj").expect_err("missing field should error");
        let pe = err.downcast_ref::<ParamError>().expect("ParamError");
        assert!(
            pe.0.contains("symbols") && pe.0.contains("symbol") && pe.0.contains("legacy"),
            "show missing-symbols error must mention canonical + legacy + the word \
             `legacy`; got: {}",
            pe.0
        );
    }

    #[test]
    fn check_missing_field_error_mentions_legacy_alias_names() {
        let err = build_command("check", &json!({}), "/tmp/proj")
            .expect_err("missing field should error");
        let pe = err.downcast_ref::<ParamError>().expect("ParamError");
        assert!(
            pe.0.contains("symbols") && pe.0.contains("names") && pe.0.contains("legacy"),
            "check missing-symbols error must mention canonical + legacy alias `names`; \
             got: {}",
            pe.0
        );
    }

    #[test]
    fn check_non_string_element_is_invalid_params() {
        assert_param_error("check", json!({"symbols": ["Foo", 42]}), "symbols[1]");
    }

    #[test]
    fn search_kind_array_with_non_string_element_is_invalid_params() {
        assert_param_error(
            "search",
            json!({"query": "foo", "kind": ["fn", 42]}),
            "kind[1]",
        );
    }

    #[test]
    fn bundle_missing_mode_is_invalid_params() {
        assert_param_error("bundle", json!({}), "mode");
    }

    #[test]
    fn bundle_symbol_mode_missing_symbol_is_invalid_params() {
        assert_param_error("bundle", json!({"mode": "symbol"}), "symbol");
    }

    #[test]
    fn bundle_pr_impact_missing_base_is_invalid_params() {
        assert_param_error("bundle", json!({"mode": "pr-impact"}), "base");
    }

    #[test]
    fn bundle_unknown_mode_is_invalid_params() {
        assert_param_error("bundle", json!({"mode": "nope"}), "unknown bundle mode");
    }

    /// Pin the cleanup of the bundle arm: presence of an integer field
    /// forwards it to the CLI; absence omits the flag entirely (no
    /// `--depth 0` leakage). Exercises `opt_u64_some`.
    #[test]
    fn bundle_pr_impact_omits_depth_when_absent() {
        let built = build_command(
            "bundle",
            &json!({"mode": "pr-impact", "base": "main"}),
            "/tmp/proj",
        )
        .expect("build_command");
        assert!(
            !built.extra_args.iter().any(|a| a == "--depth"),
            "absent `depth` must NOT forward --depth: {:?}",
            built.extra_args
        );
    }

    #[test]
    fn bundle_pr_impact_forwards_depth_when_present() {
        let built = build_command(
            "bundle",
            &json!({"mode": "pr-impact", "base": "main", "depth": 3}),
            "/tmp/proj",
        )
        .expect("build_command");
        let depth_idx = built
            .extra_args
            .iter()
            .position(|a| a == "--depth")
            .expect("expected --depth flag");
        assert_eq!(built.extra_args.get(depth_idx + 1), Some(&"3".to_string()));
    }

    /// Mutually-exclusive flag pairs that previously fell into `-32000`
    /// now ride the same `-32602` channel as the other type errors.
    #[test]
    fn search_diff_scope_conflict_is_invalid_params() {
        assert_param_error(
            "search",
            json!({"query": "x", "since": "main", "changed_only": true}),
            "mutually exclusive",
        );
    }

    #[test]
    fn show_truncate_conflict_is_invalid_params() {
        assert_param_error(
            "show",
            json!({"symbols": ["Foo"], "signature_only": true, "no_body": true}),
            "mutually exclusive",
        );
    }

    #[test]
    fn search_async_only_no_async_conflict_is_invalid_params() {
        assert_param_error(
            "search",
            json!({"query": "x", "async_only": true, "no_async": true}),
            "mutually exclusive",
        );
    }

    #[test]
    fn search_float_limit_is_invalid_params() {
        assert_param_error("search", json!({"query": "foo", "limit": 5.5}), "limit");
    }

    #[test]
    fn search_negative_limit_is_invalid_params() {
        assert_param_error("search", json!({"query": "foo", "limit": -3}), "limit");
    }

    /// Sanity: a correctly-typed call still builds.
    #[test]
    fn search_correctly_typed_args_succeed() {
        let built = build_command(
            "search",
            &json!({"query": "foo", "limit": 5, "auto_update": false}),
            "/tmp/proj",
        )
        .expect("typed-correct search must build");
        assert_eq!(built.subcommand, "search");
        assert!(built.extra_args.iter().any(|a| a == "foo"));
    }

    /// handle_request must surface ParamError as JSON-RPC `-32602`.
    #[test]
    fn handle_request_maps_param_error_to_minus_32602() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(7)),
            method: "tools/call".into(),
            params: Some(json!({
                "name": "search",
                "arguments": { "query": "foo", "limit": "twenty" }
            })),
        };
        let resp = handle_request(&req);
        let err = resp.error.expect("expected error response");
        assert_eq!(err.code, -32602, "expected -32602 Invalid params");
        assert!(err.message.contains("limit"));
    }
}
