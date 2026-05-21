//! Structured trace for `vex search --why` / MCP `search { why: true }`.
//!
//! The trace is built post-hoc from the un-truncated channel result lists
//! that the Search handler already has in scope. Goal: zero impact on
//! the fast path when `--why` is off, and just enough information to
//! answer "what did vex actually search?" when it is on.
//!
//! Fields:
//!   * `normalized_query` — the query string as the search engine sees
//!     it (currently `query.to_lowercase().trim().to_string()` to mirror
//!     `rerank::rerank`'s lowercase comparison, with leading/trailing
//!     whitespace trimmed).
//!   * `channels` — per-channel hit counts before fusion. Lets the user
//!     spot "BM25 returned nothing" or "semantic dominated" without
//!     re-running with `RUST_LOG`.
//!   * `fallbacks` — names of soft-fallback behaviours that engaged
//!     (currently just `"fuzzy"` when any result carries
//!     `MatchType::Fuzzy`).
//!   * `filter_applied` — snapshot of the include/exclude/filter args
//!     that narrowed the result set. Helps catch "I forgot I had
//!     `--filter tests/` active in shell history".

use serde::Serialize;

use super::{MatchType, SearchResult};

#[derive(Debug, Clone, Serialize)]
pub struct SearchTrace {
    pub normalized_query: String,
    pub channels: Vec<ChannelTrace>,
    pub fallbacks: Vec<String>,
    pub filter_applied: FilterSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelTrace {
    pub name: &'static str,
    pub hits: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FilterSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kind: Vec<String>,
}

impl SearchTrace {
    /// Build a trace from the already-collected channel result vectors.
    /// Channel slots that didn't run (e.g. semantic when the index has
    /// no vectors) should be passed as empty slices, which renders as
    /// `hits: 0` in the JSON.
    pub fn from_channels(
        query: &str,
        structural: &[SearchResult],
        bm25: &[SearchResult],
        semantic: &[SearchResult],
        merged: &[SearchResult],
        filter: FilterSnapshot,
    ) -> Self {
        // NB: mirrors the lowercase comparison in `rerank::rerank` for
        // exact-name scoring — the FST lookup (structural channel) uses
        // the raw query bytes (case-sensitive). This field is an
        // approximation of "what rerank sees", not "what FST sees".
        let normalized_query = query.trim().to_lowercase();
        let channels = vec![
            ChannelTrace {
                name: "fst",
                hits: structural.len(),
            },
            ChannelTrace {
                name: "bm25",
                hits: bm25.len(),
            },
            ChannelTrace {
                name: "semantic",
                hits: semantic.len(),
            },
        ];
        let mut fallbacks = Vec::new();
        if merged
            .iter()
            .any(|r| matches!(r.match_type, MatchType::Fuzzy))
        {
            fallbacks.push("fuzzy".to_string());
        }
        Self {
            normalized_query,
            channels,
            fallbacks,
            filter_applied: filter,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(match_type: MatchType) -> SearchResult {
        SearchResult {
            name: "x".into(),
            kind: "function".into(),
            path: "src/x.rs".into(),
            line: 1,
            signature: None,
            score: 1.0,
            match_type,
        }
    }

    #[test]
    fn normalized_query_lowercases_and_trims() {
        let trace = SearchTrace::from_channels(
            "  PaymentProcessor\n",
            &[],
            &[],
            &[],
            &[],
            FilterSnapshot::default(),
        );
        assert_eq!(trace.normalized_query, "paymentprocessor");
    }

    #[test]
    fn channels_carry_per_channel_hit_counts() {
        let structural = vec![make(MatchType::Structural), make(MatchType::Structural)];
        let bm25 = vec![make(MatchType::Bm25)];
        let semantic: Vec<SearchResult> = vec![];
        let merged = structural.clone();
        let trace = SearchTrace::from_channels(
            "q",
            &structural,
            &bm25,
            &semantic,
            &merged,
            FilterSnapshot::default(),
        );
        assert_eq!(trace.channels.len(), 3);
        assert_eq!(trace.channels[0].name, "fst");
        assert_eq!(trace.channels[0].hits, 2);
        assert_eq!(trace.channels[1].name, "bm25");
        assert_eq!(trace.channels[1].hits, 1);
        assert_eq!(trace.channels[2].name, "semantic");
        assert_eq!(trace.channels[2].hits, 0);
    }

    #[test]
    fn fuzzy_fallback_surfaces_in_trace() {
        let merged = vec![make(MatchType::Fuzzy)];
        let trace =
            SearchTrace::from_channels("q", &[], &[], &[], &merged, FilterSnapshot::default());
        assert_eq!(trace.fallbacks, vec!["fuzzy".to_string()]);
    }

    #[test]
    fn no_fuzzy_means_empty_fallbacks() {
        let merged = vec![make(MatchType::Structural)];
        let trace =
            SearchTrace::from_channels("q", &[], &[], &[], &merged, FilterSnapshot::default());
        assert!(trace.fallbacks.is_empty());
    }

    #[test]
    fn filter_snapshot_round_trips_into_trace() {
        let filter = FilterSnapshot {
            filter: Some("tests/".into()),
            include: vec!["src/**".into()],
            exclude: vec!["**/*.gen.rs".into()],
            kind: vec!["fn".into()],
        };
        let trace = SearchTrace::from_channels("q", &[], &[], &[], &[], filter.clone());
        assert_eq!(trace.filter_applied.filter.as_deref(), Some("tests/"));
        assert_eq!(trace.filter_applied.include, filter.include);
        assert_eq!(trace.filter_applied.exclude, filter.exclude);
        assert_eq!(trace.filter_applied.kind, filter.kind);
    }
}
