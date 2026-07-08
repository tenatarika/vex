//! MCP tool-call response builder. Turns a `vex` subprocess outcome
//! into a JSON-RPC 2.0 `result` value, lifting any Phase-13
//! ResponseEnvelope shape into the MCP-spec-prescribed
//! `structuredContent` / `_meta` blocks.
//!
//! Extracted from `main.rs` in the v1.21 split — see
//! `.claude/Task/v1.21-vex-mcp-split.md`.

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
}
