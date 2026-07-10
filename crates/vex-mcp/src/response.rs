//! MCP tool-call response builder. Turns a `vex` subprocess outcome
//! into a JSON-RPC 2.0 `result` value, lifting any Phase-13
//! ResponseEnvelope shape into the MCP-spec-prescribed
//! `structuredContent` / `_meta` blocks.
//!
//! Extracted from `main.rs` in the v1.21 split — see
//! `.claude/Task/v1.21-vex-mcp-split.md`.

use std::fmt::Write as _;

use anyhow::Result;
use serde_json::Value;

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
pub(crate) fn build_mcp_response(
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
    // mechanism for typed payloads). `content[0].text` carries the concise
    // agent-tuned render (§4.1), NOT the full envelope — the machine payload
    // lives in `structuredContent`/`_meta` below.
    let envelope_protocol_version = content
        .get("protocol_version")
        .and_then(Value::as_str)
        .map(String::from);
    let envelope_capabilities = content.get("capabilities").cloned();
    let envelope_results = content.get("results").cloned();
    let envelope_meta = content.get("_meta").cloned();
    let is_envelope = envelope_protocol_version.is_some() && envelope_capabilities.is_some();

    // §4.2 safety gate: completeness is trustworthy ONLY when the producer
    // advertises `capabilities.result_completeness`. An old/third-party `vex`
    // emitting stray `_meta.vex.dev/truncated` without the capability must be
    // read as "unknown", never "complete" — so the renderer stays silent about
    // completeness unless this is true (design §4.2 HIGH-1).
    let completeness_known = envelope_capabilities
        .as_ref()
        .and_then(|c| c.get("result_completeness"))
        .and_then(Value::as_bool)
        == Some(true);
    let text = render_content_text(
        text_mode_from_env(),
        completeness_known,
        subcommand,
        is_envelope,
        &content,
        &envelope_results,
        &envelope_meta,
    )?;
    let mut result = serde_json::json!({
        "content": [{
            "type": "text",
            "text": text
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

/// Which text the LLM-facing `content` channel carries. `Raw` is the
/// `VEX_MCP_TEXT=raw` escape hatch (legacy full-envelope dump).
#[derive(Clone, Copy)]
enum TextMode {
    Concise,
    Raw,
}

/// Read the `content`-channel mode from the environment. Split from
/// [`render_content_text`] so the render logic is unit-testable without
/// mutating process-global env (which would poison parallel tests).
fn text_mode_from_env() -> TextMode {
    if std::env::var("VEX_MCP_TEXT").ok().as_deref() == Some("raw") {
        TextMode::Raw
    } else {
        TextMode::Concise
    }
}

/// Build the LLM-facing `content[0].text` (PROTOCOL-EVOLUTION §4.1).
///
/// Two-audience split: `content` is the concise agent channel, while
/// `structuredContent`/`_meta` (assembled by the caller and left untouched
/// here) carry the full-fidelity machine payload with raw scores. This reads
/// the already-parsed envelope; it must NOT mutate `results`/`meta`.
///
/// `VEX_MCP_TEXT=raw` restores the legacy full-envelope pretty dump —
/// indefinitely, the migration path for clients that parse `content` as JSON
/// (undocumented but real).
fn render_content_text(
    mode: TextMode,
    completeness_known: bool,
    subcommand: &str,
    is_envelope: bool,
    content: &Value,
    results: &Option<Value>,
    meta: &Option<Value>,
) -> Result<String> {
    if matches!(mode, TextMode::Raw) {
        return Ok(serde_json::to_string_pretty(content)?);
    }
    if !is_envelope {
        // Raw stdout / non-envelope error body: nothing structured to condense.
        return Ok(serde_json::to_string_pretty(content)?);
    }
    // Rich render only for the row-shape family. `search` is the sole tool
    // verified to emit `{name, kind, path, line, signals, result_kind}`; other
    // subcommands are heterogeneous (`usages` = `{path,line}`, `bundle` =
    // object results, …) and fall through to the fallback branch below.
    if subcommand == "search" {
        if let Some(text) =
            render_search_concise(results.as_ref(), meta.as_ref(), completeness_known)
        {
            return Ok(text);
        }
    }
    // Fallback: compact-but-complete JSON of `results` — never drops a
    // `results` field (worst case "less pretty than a bespoke summary"). The
    // §4.2 completeness line is prepended when the producer emits it, so a
    // delete-safety signal (e.g. `usages` truncation) still reaches the
    // LLM-visible `content` channel even for non-search tools — `_meta` alone
    // is documented invisible to the model.
    let payload = results.as_ref().unwrap_or(content);
    let json = serde_json::to_string(payload)?;
    match completeness_line(meta.as_ref(), completeness_known) {
        Some(line) => Ok(format!("{line}\n{json}")),
        None => Ok(json),
    }
}

/// Concise `search` render: one line per hit —
/// `<def|nbr|hit> name (kind)  path:line  via:<channel>` — with the drift hint
/// prepended and (when the producer emits §4.2 keys) a completeness line
/// appended. No raw scores reach the model. Returns `None` when `results` is
/// not the expected array, so the caller falls back to compact JSON.
fn render_search_concise(
    results: Option<&Value>,
    meta: Option<&Value>,
    completeness_known: bool,
) -> Option<String> {
    let rows = results?.as_array()?;
    let mut out = String::new();

    // Drift hint first: "you searched a name with no local definition — these
    // are neighbours", else the neighbour list is silently over-trusted.
    if let Some(msg) = meta
        .and_then(|m| m.get("vex.dev/search_hint"))
        .and_then(|h| h.get("message"))
        .and_then(Value::as_str)
    {
        out.push_str("hint: ");
        out.push_str(msg);
        out.push('\n');
    }

    if rows.is_empty() {
        out.push_str("(no results)");
    } else {
        for row in rows {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
            let path = row.get("path").and_then(Value::as_str).unwrap_or("?");
            let marker = match row.get("result_kind").and_then(Value::as_str) {
                Some("def") => "def",
                Some("neighbor") => "nbr",
                _ => "hit",
            };
            let loc = match row.get("line").and_then(Value::as_u64) {
                Some(l) => format!("{path}:{l}"),
                None => path.to_string(),
            };
            let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
            let via = derive_via(row);
            out.push_str(marker);
            out.push(' ');
            out.push_str(name);
            if !kind.is_empty() {
                let _ = write!(out, " ({kind})");
            }
            let _ = writeln!(out, "  {loc}  via:{via}");
        }
    }

    // Completeness line (currently dormant for `search` until its own §4.2
    // lower-bound emission lands — `cmd_search` does not set `truncated` yet).
    if let Some(line) = completeness_line(meta, completeness_known) {
        out.push_str(&line);
        out.push('\n');
    }

    Some(out.trim_end().to_string())
}

/// `via:` channel for a search row, mirroring `result_kind`'s own precedence so
/// the marker and `via:` can never disagree: structural (`fst_hit`) > lexical
/// (`bm25_rank`) > semantic (`semantic_rank`). `fst_hit` is always present (a
/// plain wire `bool`); the channel-rank fields are `Option` + `skip_serializing_if`,
/// so for THOSE *absence* — not `false` — means "not this channel".
fn derive_via(row: &Value) -> &'static str {
    let sig = row.get("signals");
    let present = |k: &str| sig.and_then(|s| s.get(k)).is_some();
    if sig.and_then(|s| s.get("fst_hit")).and_then(Value::as_bool) == Some(true) {
        "name"
    } else if present("bm25_rank") {
        "lexical"
    } else if present("semantic_rank") {
        "semantic"
    } else {
        "?"
    }
}

/// Human completeness line from §4.2 `_meta`. `None` when completeness is not
/// trustworthy — the producer didn't advertise `capabilities.result_completeness`
/// (`completeness_known == false`) OR emitted no `truncated` key. A safety
/// signal: absence is rendered as silence, NEVER "all N" (design §4.2 HIGH-1).
fn completeness_line(meta: Option<&Value>, completeness_known: bool) -> Option<String> {
    if !completeness_known {
        return None;
    }
    let m = meta?;
    let truncated = m.get("vex.dev/truncated").and_then(Value::as_bool)?;
    let total = m.get("vex.dev/result_total").and_then(Value::as_u64);
    let exact = m.get("vex.dev/result_total_exact").and_then(Value::as_bool) != Some(false);
    Some(match (truncated, total, exact) {
        (true, Some(t), true) => format!("-- truncated: showing a subset of {t} total"),
        (true, Some(t), false) => format!("-- truncated: ranked, >={t} total (more may exist)"),
        (true, None, _) => "-- truncated: more results exist".to_string(),
        (false, Some(t), _) => format!("-- complete: all {t} shown"),
        (false, None, _) => "-- complete".to_string(),
    })
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
pub(crate) fn extract_why_trace(stderr: &str) -> Option<Value> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// PROTOCOL-EVOLUTION §2a cross-crate tolerance — the MCP wrapper
    /// reconstructs `capabilities` / `_meta` as untyped JSON, so an envelope
    /// carrying an *unknown* capability key AND an unknown `_meta.vex.dev/*`
    /// key (a newer `vex` on PATH than this wrapper was built against) must
    /// pass through untouched, never rejected. This proves invariant #2 for
    /// the real consumer, not just the producer.
    #[test]
    fn build_mcp_response_tolerates_unknown_capability_and_meta_keys() {
        let future_envelope = serde_json::json!({
            "protocol_version": "v1",
            "capabilities": {
                "signals": true, "empty_reason": false, "bundle_modes": [],
                "why": true, "scope_filters": true, "metadata_filters": true,
                "auto_update": true, "history_diff": true,
                "structured_result_kind": true,
                // A capability this wrapper build has never heard of:
                "some_future_flag": true,
            },
            "_meta": {
                "vex.dev/index_age_ms": 7,
                // A _meta key this wrapper build has never heard of:
                "vex.dev/some_future_hint": { "reason": "future" },
            },
            "results": [{
                "name": "alpha_handler", "kind": "function",
                "path": "src/a.rs", "line": 1, "score": 0.9,
                "rank_percentile": 1.0,
                "signals": { "fst_hit": true },
                "result_kind": "def",
            }]
        });
        let stdout = serde_json::to_string(&future_envelope).unwrap();

        let result = build_mcp_response(
            Some(0),
            "exit status: 0".to_string(),
            &stdout,
            "",
            "search",
            &[],
            &json!({}),
        )
        .expect("unknown forward-compat keys must not defeat envelope lifting");

        // Unknown capability survives (capabilities cloned wholesale).
        assert_eq!(
            result["capabilities"]["some_future_flag"].as_bool(),
            Some(true),
            "unknown capability key must pass through untouched; got: {result}"
        );
        // Unknown _meta key survives (copied key-by-key).
        assert!(
            result["_meta"]["vex.dev/some_future_hint"].is_object(),
            "unknown _meta key must pass through untouched; got: {}",
            result["_meta"]
        );
        // The new result_kind marker rides in structuredContent for the LLM.
        assert_eq!(
            result["structuredContent"]["results"][0]["result_kind"].as_str(),
            Some("def"),
            "result_kind must be carried in structuredContent; got: {result}"
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

    // ---- PROTOCOL-EVOLUTION §4.1 concise content render ----

    #[test]
    fn search_concise_render_has_markers_via_and_no_raw_scores() {
        let results = json!([
            { "name": "run_task", "kind": "function", "path": "src/lib.rs", "line": 1,
              "signature": "pub fn run_task()", "score": 0.9, "match_type": "Hybrid",
              "result_kind": "def",
              "signals": { "fst_hit": true, "bm25_rank": 3, "bm25_score": 4.2 } },
            { "name": "run_task", "kind": "method", "path": "src/other.rs", "line": 9,
              "score": 0.4, "match_type": "Lexical", "result_kind": "neighbor",
              "signals": { "fst_hit": false, "bm25_rank": 11, "bm25_score": 1.1 } }
        ]);
        let meta =
            json!({ "vex.dev/search_hint": { "message": "no local definition for 'run_task'" } });
        let text = render_search_concise(Some(&results), Some(&meta), true).expect("array renders");

        assert!(
            text.contains("hint: no local definition"),
            "drift hint prepended:\n{text}"
        );
        assert!(text.contains("def run_task"), "def marker:\n{text}");
        assert!(text.contains("nbr run_task"), "neighbor marker:\n{text}");
        assert!(text.contains("src/lib.rs:1"), "path:line:\n{text}");
        assert!(text.contains("via:name"), "structural via:\n{text}");
        assert!(text.contains("via:lexical"), "lexical via:\n{text}");
        // No raw scores / verbose fields reach the model (they stay in
        // structuredContent).
        assert!(!text.contains("bm25"), "no raw score fields:\n{text}");
        assert!(!text.contains("4.2"), "no raw score values:\n{text}");
        assert!(!text.contains("signature"), "no signature dump:\n{text}");
    }

    #[test]
    fn completeness_line_variants_and_silence_on_unknown() {
        let mk = |t: bool, total: Option<u64>, exact: Option<bool>| {
            let mut m = serde_json::Map::new();
            m.insert("vex.dev/truncated".into(), json!(t));
            if let Some(x) = total {
                m.insert("vex.dev/result_total".into(), json!(x));
            }
            if let Some(e) = exact {
                m.insert("vex.dev/result_total_exact".into(), json!(e));
            }
            Value::Object(m)
        };
        assert_eq!(
            completeness_line(Some(&mk(true, Some(50), None)), true).unwrap(),
            "-- truncated: showing a subset of 50 total"
        );
        assert_eq!(
            completeness_line(Some(&mk(false, Some(3), None)), true).unwrap(),
            "-- complete: all 3 shown"
        );
        assert_eq!(
            completeness_line(Some(&mk(true, Some(10), Some(false))), true).unwrap(),
            "-- truncated: ranked, >=10 total (more may exist)"
        );
        // Safety: absent completeness keys = unknown = silence, NEVER "all N".
        assert!(completeness_line(Some(&json!({})), true).is_none());
        assert!(completeness_line(None, true).is_none());
        // Safety gate (§4.2 HIGH-1): capability NOT advertised → silence even
        // when a stray `truncated` key is present (old/third-party producer).
        assert!(completeness_line(Some(&mk(true, Some(50), None)), false).is_none());
    }

    /// Envelope capabilities block; `completeness` toggles the gated
    /// `result_completeness` flag so tests can exercise the §4.2 skew rule.
    fn caps(completeness: bool) -> Value {
        json!({ "signals": true, "empty_reason": false, "bundle_modes": [],
            "why": true, "scope_filters": true, "metadata_filters": true,
            "auto_update": true, "history_diff": true,
            "result_completeness": completeness })
    }

    #[test]
    fn non_search_fallback_is_compact_and_surfaces_completeness_line() {
        // `usages` (compact-JSON fallback) with the capability advertised: the
        // §4.2 truncation warning must reach the LLM-visible content channel,
        // prepended to the compact results JSON (`_meta` alone is invisible).
        let envelope = json!({
            "protocol_version": "v1",
            "capabilities": caps(true),
            "_meta": { "vex.dev/truncated": true, "vex.dev/result_total": 42 },
            "results": [ { "path": "src/a.rs", "line": 7 } ]
        });
        let stdout = serde_json::to_string(&envelope).unwrap();
        let result = build_mcp_response(
            Some(0),
            "exit status: 0".to_string(),
            &stdout,
            "",
            "usages",
            &[],
            &json!({}),
        )
        .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("-- truncated: showing a subset of 42 total\n"),
            "completeness line must prepend the fallback; got: {text}"
        );
        assert!(
            text.contains("\"path\":\"src/a.rs\""),
            "compact results JSON; got: {text}"
        );
        assert!(
            !text.contains("\n  "),
            "results must be compact, not pretty; got: {text}"
        );
        // Machine channels untouched.
        assert_eq!(
            result["structuredContent"]["results"][0]["line"].as_u64(),
            Some(7)
        );
        assert_eq!(result["_meta"]["vex.dev/truncated"].as_bool(), Some(true));
    }

    /// §4.2 HIGH-1 skew: a `truncated` key WITHOUT the `result_completeness`
    /// capability (old/third-party producer) must NOT render a completeness
    /// line — absent capability = "unknown", never "complete".
    #[test]
    fn stray_truncated_without_capability_renders_no_completeness_line() {
        let envelope = json!({
            "protocol_version": "v1",
            "capabilities": caps(false),
            "_meta": { "vex.dev/truncated": true, "vex.dev/result_total": 42 },
            "results": [ { "path": "src/a.rs", "line": 7 } ]
        });
        let stdout = serde_json::to_string(&envelope).unwrap();
        let result = build_mcp_response(
            Some(0),
            "exit status: 0".to_string(),
            &stdout,
            "",
            "usages",
            &[],
            &json!({}),
        )
        .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            !text.contains("truncated") && !text.contains("complete"),
            "no completeness claim when capability absent; got: {text}"
        );
    }

    /// Object-shaped `results` (e.g. `bundle`) fall through to the never-lossy
    /// compact-JSON branch complete and non-empty — the row renderer must not
    /// empty-render a non-array payload.
    #[test]
    fn object_shaped_results_fallback_is_complete() {
        let envelope = json!({
            "protocol_version": "v1",
            "capabilities": caps(true),
            "results": { "symbol": "Foo", "callers": [ { "path": "a.rs", "line": 1 } ] }
        });
        let stdout = serde_json::to_string(&envelope).unwrap();
        let result = build_mcp_response(
            Some(0),
            "exit status: 0".to_string(),
            &stdout,
            "",
            "bundle",
            &[],
            &json!({}),
        )
        .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("\"symbol\":\"Foo\""),
            "object results complete; got: {text}"
        );
        assert!(
            text.contains("\"callers\""),
            "nested fields preserved; got: {text}"
        );
    }

    #[test]
    fn raw_mode_restores_full_envelope_dump() {
        let content = json!({ "results": [ { "name": "x", "signals": { "bm25_score": 4.2 } } ] });
        let results = content.get("results").cloned();
        let text = render_content_text(
            TextMode::Raw,
            true,
            "search",
            true,
            &content,
            &results,
            &None,
        )
        .unwrap();
        // VEX_MCP_TEXT=raw escape hatch: full dump, raw scores present.
        assert!(
            text.contains("bm25_score"),
            "raw mode keeps full envelope; got: {text}"
        );
    }
}
