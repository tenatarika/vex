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
//!   * `UsagesTrace` — mode (strict / text_scan), hits before/after
//!     path filter, prefix-suggestion count when no exact hit.
//!   * `SimilarTrace` — seed resolution, applied threshold,
//!     candidates before/after path filter.
//!   * `DuplicatesTrace` — applied threshold + min_body_lines, pairs
//!     before/after path filter.
//!
//! `FilterSnapshot` is local to this module (search has its own with
//! a `kind` field that doesn't apply to refs/embedding-based commands).

use serde::Serialize;

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
    /// (binder-resolved refs only); `"text_scan"` for the legacy refs
    /// FST that captures CamelCase identifiers across every supported
    /// language.
    pub mode: &'static str,
    /// Hits returned by the underlying lookup, BEFORE path-filter
    /// narrowing (filter_path + include/exclude).
    pub hits_before_filter: usize,
    /// Hits surviving path-filter narrowing — the count printed in the
    /// result list (modulo `--limit` truncation).
    pub hits_after_filter: usize,
    /// `Some(n)` when zero exact hits were found in the text-scan
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
            mode: "text_scan",
            hits_before_filter: 5,
            hits_after_filter: 5,
            prefix_suggestions: None,
            filter_applied: FilterSnapshot::default(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""mode":"text_scan""#));
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
        let t = UsagesTrace {
            mode: "strict",
            hits_before_filter: 0,
            hits_after_filter: 0,
            prefix_suggestions: None,
            filter_applied: FilterSnapshot::default(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""mode":"strict""#));
        assert!(
            !s.contains("prefix_suggestions"),
            "strict mode + None must omit the field, got {s}",
        );
    }

    #[test]
    fn usages_trace_records_text_scan_prefix_count() {
        // Mirror of the above for the non-strict (text_scan) path:
        // when zero exact hits land, the handler runs the prefix
        // fallback and surfaces the suggestion count.
        let t = UsagesTrace {
            mode: "text_scan",
            hits_before_filter: 0,
            hits_after_filter: 0,
            prefix_suggestions: Some(3),
            filter_applied: FilterSnapshot::default(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""mode":"text_scan""#));
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
