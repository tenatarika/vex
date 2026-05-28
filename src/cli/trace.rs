//! Phase 11.10 — structured traces for `--why` / MCP `why: true` on
//! `usages`, `similar`, and `duplicates`.
//!
//! Mirrors the design of `crate::search::trace`:
//!   * built post-hoc from values the handler already has in scope so
//!     the fast path pays nothing when `--why` is off
//!   * emitted as a one-line JSON object on stderr, so
//!     `vex usages X --why | jq` keeps working
//!   * MCP wrapper picks up the same stderr line via
//!     `extract_why_trace` (in `crates/vex-mcp/src/main.rs`)
//!
//! Per-trace shape:
//!   * `UsagesTrace` — mode (strict / fst_lookup), hits before/after
//!     path filter, prefix-suggestion count when no exact hit.
//!     `mode_legacy` carries the v1.8.x label (`text_scan`) for back-
//!     compat with consumers that learned the old name — slated for
//!     removal in v1.12 (Phase 14.4 rename).
//!   * `SimilarTrace` — seed resolution, applied threshold,
//!     candidates before/after path filter.
//!   * `DuplicatesTrace` — applied threshold + min_body_lines, pairs
//!     before/after path filter.
//!
//! `FilterSnapshot` is local to this module (search has its own with
//! a `kind` field that doesn't apply to refs/embedding-based commands).

use serde::Serialize;

/// Stderr prefix the MCP wrapper looks for when extracting `--why`
/// traces (review S8.1, v1.10.1).
///
/// Before v1.10.1 `crates/vex-mcp::extract_why_trace` picked the first
/// stderr line that started with `{` and tried to parse it as JSON —
/// any earlier `tracing::warn!` JSON (e.g. the "cannot determine
/// index freshness" warning) would shadow the real trace and surface
/// under `_meta.why`. Tagging the trace with `VEX_WHY:` makes the
/// extractor unambiguous: it scans for this prefix specifically and
/// only falls back to the legacy last-`{`-line behaviour when the
/// prefix is absent (older CLIs on PATH at MCP-spawn time).
pub const WHY_TRACE_PREFIX: &str = "VEX_WHY:";

/// Stderr prefix for the diff-filter envelope CLI users get alongside
/// `--why`. Distinct from [`WHY_TRACE_PREFIX`] so the MCP's legacy
/// fallback (last `{`-line) can't mistake one for the other.
///
/// The diff-filter JSON also flows through the response envelope's
/// `_meta.vex.dev/diff_filter` field, so the stderr emission is purely
/// for `vex … --why 2>&1 | jq` CLI consumers. Tagging it here means
/// the MCP wrapper would have to opt in to pick it up.
pub const DIFF_FILTER_PREFIX: &str = "VEX_DIFF:";

/// Emit a `--why` trace on stderr tagged with [`WHY_TRACE_PREFIX`].
///
/// Replacing every `eprintln!("{}", serde_json::to_string(&trace)?)`
/// call site with this helper guarantees the MCP wrapper picks up
/// the right line even when earlier stderr output (tracing warnings,
/// auto-update banners) emits JSON-shaped text first.
pub fn emit_why_trace<T: Serialize + ?Sized>(trace: &T) -> anyhow::Result<()> {
    let json = serde_json::to_string(trace)?;
    eprintln!("{WHY_TRACE_PREFIX} {json}");
    Ok(())
}

/// Emit a `diff_filter_meta` envelope on stderr tagged with
/// [`DIFF_FILTER_PREFIX`]. Mirrors [`emit_why_trace`] so the legacy
/// MCP fallback never confuses the two payloads.
pub fn emit_diff_filter<T: Serialize + ?Sized>(meta: &T) -> anyhow::Result<()> {
    let json = serde_json::to_string(meta)?;
    eprintln!("{DIFF_FILTER_PREFIX} {json}");
    Ok(())
}

/// Snapshot of the scope filters that narrowed a result set —
/// reused across all three trace types in this module.
///
/// Intentionally distinct from `crate::search::trace::FilterSnapshot`,
/// which carries an additional `kind: Vec<String>` for search's
/// result-kind filter. If a new filter axis (lang, visibility, …) is
/// added to BOTH search and the refs/embedding commands, update both
/// structs in lock-step — they are not consolidated because the
/// `kind` field is search-specific and would render as a noisy empty
/// array on every usages/similar/duplicates trace today.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FilterSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsagesTrace {
    /// `"strict"` when the v5 `reference_edges` section was queried
    /// (binder-resolved refs only); `"fst_lookup"` for the refs FST
    /// that captures CamelCase identifiers across every supported
    /// language. Phase 14.4 renamed the non-strict value from
    /// `"text_scan"` → `"fst_lookup"` — the data path is and always
    /// was an FST lookup, not a text scan.
    pub mode: &'static str,
    /// Legacy alias for `mode`. Mirrors the value in `mode`, EXCEPT
    /// when `mode == "fst_lookup"` it carries the old label
    /// (`"text_scan"`) for back-compat with v1.9.x consumers that
    /// learned the contract before the rename. Slated for removal
    /// in v1.12 (one minor after v1.11).
    pub mode_legacy: &'static str,
    /// Hits returned by the underlying lookup, BEFORE path-filter
    /// narrowing (filter_path + include/exclude).
    pub hits_before_filter: usize,
    /// Hits surviving path-filter narrowing — the count printed in the
    /// result list (modulo `--limit` truncation).
    pub hits_after_filter: usize,
    /// `Some(n)` when zero exact hits were found in the fst-lookup
    /// path and the prefix-search fallback turned up `n`
    /// "Did you mean" candidates. `None` when no prefix search ran:
    /// either there WERE exact hits, OR `--strict` is in use (the
    /// scope-binder-resolved path has no prefix-fallback today).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix_suggestions: Option<usize>,
    pub filter_applied: FilterSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimilarTrace {
    /// Whether the seed symbol resolved to a stored embedding vector.
    /// `false` means the seed name didn't match any indexed symbol —
    /// the result list is empty and the user likely needs to check
    /// the spelling or rebuild with `--semantic`.
    pub seed_resolved: bool,
    /// Cosine-similarity threshold applied AFTER the HNSW ranking.
    pub threshold_applied: f32,
    /// Candidates the HNSW returned (post-threshold).
    pub candidates_before_filter: usize,
    /// Candidates surviving the path filter + `--limit` truncation —
    /// matches the printed result count.
    pub candidates_after_filter: usize,
    pub filter_applied: FilterSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct DuplicatesTrace {
    pub threshold_applied: f32,
    pub min_body_lines_applied: usize,
    /// Pairs returned by the duplicate scan BEFORE the path filter.
    pub pairs_before_filter: usize,
    /// Pairs after path filter + `--limit` truncation — matches the
    /// printed pair count.
    pub pairs_after_filter: usize,
    pub filter_applied: FilterSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usages_trace_omits_empty_filter_fields() {
        // Pin that an "all defaults" filter snapshot serialises as
        // `"filter_applied":{}` — without `skip_serializing_if`, the
        // JSON would carry three empty fields per call, which adds
        // noise for the common no-filter case.
        let t = UsagesTrace {
            mode: "fst_lookup",
            mode_legacy: "text_scan",
            hits_before_filter: 5,
            hits_after_filter: 5,
            prefix_suggestions: None,
            filter_applied: FilterSnapshot::default(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""mode":"fst_lookup""#));
        assert!(s.contains(r#""mode_legacy":"text_scan""#));
        assert!(s.contains(r#""filter_applied":{}"#), "got {s}");
        // prefix_suggestions=None must NOT appear (skip_if).
        assert!(!s.contains("prefix_suggestions"), "got {s}");
    }

    #[test]
    fn usages_trace_records_strict_mode_with_no_prefix_fallback() {
        // Strict mode has no prefix-suggestion fallback today, so the
        // production invariant is: `mode = "strict"` => `prefix_suggestions
        // is None`. Pin BOTH halves of that contract here so a future
        // arm that adds prefix support to strict mode flips this test
        // loudly (rather than silently emitting an `Option::None`-shaped
        // JSON field via skip_serializing_if).
        //
        // Phase 14.4: on the strict path `mode` and `mode_legacy` both
        // carry `"strict"` — the legacy alias only diverges from `mode`
        // on the non-strict (FST-lookup) path.
        let t = UsagesTrace {
            mode: "strict",
            mode_legacy: "strict",
            hits_before_filter: 0,
            hits_after_filter: 0,
            prefix_suggestions: None,
            filter_applied: FilterSnapshot::default(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""mode":"strict""#));
        assert!(s.contains(r#""mode_legacy":"strict""#));
        assert!(
            !s.contains("prefix_suggestions"),
            "strict mode + None must omit the field, got {s}",
        );
    }

    #[test]
    fn usages_trace_records_fst_lookup_prefix_count() {
        // Mirror of the above for the non-strict (FST-lookup) path:
        // when zero exact hits land, the handler runs the prefix
        // fallback and surfaces the suggestion count. Phase 14.4:
        // pins both new label (`mode = "fst_lookup"`) AND the back-
        // compat legacy alias (`mode_legacy = "text_scan"`).
        let t = UsagesTrace {
            mode: "fst_lookup",
            mode_legacy: "text_scan",
            hits_before_filter: 0,
            hits_after_filter: 0,
            prefix_suggestions: Some(3),
            filter_applied: FilterSnapshot::default(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""mode":"fst_lookup""#));
        assert!(s.contains(r#""mode_legacy":"text_scan""#));
        assert!(s.contains(r#""prefix_suggestions":3"#));
    }

    #[test]
    fn similar_trace_records_seed_resolution_and_threshold() {
        let t = SimilarTrace {
            seed_resolved: true,
            threshold_applied: 0.65,
            candidates_before_filter: 12,
            candidates_after_filter: 4,
            filter_applied: FilterSnapshot {
                include: vec!["src/**".into()],
                ..Default::default()
            },
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""seed_resolved":true"#));
        assert!(s.contains(r#""threshold_applied":0.65"#));
        assert!(s.contains(r#""candidates_before_filter":12"#));
        assert!(s.contains(r#""include":["src/**"]"#));
    }

    #[test]
    fn similar_trace_handles_unresolved_seed() {
        // `seed_resolved=false` is the load-bearing signal that
        // distinguishes "spelled the symbol wrong" from "the threshold
        // dropped everything" — pin the JSON shape.
        let t = SimilarTrace {
            seed_resolved: false,
            threshold_applied: 0.5,
            candidates_before_filter: 0,
            candidates_after_filter: 0,
            filter_applied: FilterSnapshot::default(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""seed_resolved":false"#));
        assert!(s.contains(r#""candidates_before_filter":0"#));
    }

    #[test]
    fn duplicates_trace_records_both_thresholds() {
        let t = DuplicatesTrace {
            threshold_applied: 0.9,
            min_body_lines_applied: 5,
            pairs_before_filter: 17,
            pairs_after_filter: 8,
            filter_applied: FilterSnapshot {
                filter: Some("tests/".into()),
                ..Default::default()
            },
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""threshold_applied":0.9"#));
        assert!(s.contains(r#""min_body_lines_applied":5"#));
        assert!(s.contains(r#""pairs_before_filter":17"#));
        assert!(s.contains(r#""filter":"tests/""#));
    }
}
