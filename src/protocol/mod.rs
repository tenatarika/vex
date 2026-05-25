//! Phase 13 response envelope types: protocol versioning, capabilities,
//! signals, and observability metadata.
//!
//! All MCP tool responses and CLI `--format json` outputs share the
//! `ResponseEnvelope<T>` shape so agents can detect capabilities and
//! reason about per-result signal quality.

pub mod capabilities;
pub mod signals;

use serde::Serialize;

pub const PROTOCOL_VERSION: &str = "v1";

#[derive(Serialize, Clone, Debug)]
pub struct Capabilities {
    pub signals: bool,
    pub empty_reason: bool,
    pub bundle_modes: Vec<&'static str>,
    pub why: bool,
    pub scope_filters: bool,
    pub metadata_filters: bool,
    pub auto_update: bool,
}

#[derive(Serialize, Default, Clone, Debug)]
pub struct Signals {
    pub fst_hit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bm25_rank: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_rank: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuzzy_distance: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_boost: Option<f32>,
    /// Reverse call-graph indegree (count of *distinct* caller symbol
    /// indices). Only set by `vex bundle --mode project`; absent
    /// everywhere else. Additive extension to the locked 13.11
    /// envelope — `skip_serializing_if = "Option::is_none"` keeps the
    /// wire format unchanged for existing consumers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indegree: Option<u32>,
}

#[derive(Serialize, Default, Clone, Debug)]
pub struct MetaEnvelope {
    #[serde(
        rename = "vex.dev/index_age_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub index_age_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    #[serde(rename = "ttlMs", skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u32>,
    #[serde(rename = "cacheScope", skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<String>,
    /// Phase 13.7-D3: observability for diff-context filters. Carried as
    /// an untyped JSON blob to avoid coupling the protocol layer to the
    /// CLI's clap struct. Shape: `{ scope, changed_paths, retained, dropped }`.
    /// Only present when a `--since*` / `--changed-only` flag was passed.
    ///
    /// Key uses the `vex.dev/` namespace so the protocol's vendor-prefixed
    /// fields cluster together in JSON output (matches `vex.dev/index_age_ms`).
    #[serde(
        rename = "vex.dev/diff_filter",
        skip_serializing_if = "Option::is_none"
    )]
    pub diff_filter: Option<serde_json::Value>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ResponseEnvelope<T: Serialize> {
    pub protocol_version: &'static str,
    pub capabilities: Capabilities,
    #[serde(rename = "_meta")]
    pub meta: MetaEnvelope,
    pub results: T,
}
