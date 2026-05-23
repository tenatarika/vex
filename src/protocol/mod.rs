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
}

#[derive(Serialize, Default, Clone, Debug)]
pub struct MetaEnvelope {
    #[serde(rename = "vex.dev/index_age_ms", skip_serializing_if = "Option::is_none")]
    pub index_age_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    #[serde(rename = "ttlMs", skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u32>,
    #[serde(rename = "cacheScope", skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ResponseEnvelope<T: Serialize> {
    pub protocol_version: &'static str,
    pub capabilities: Capabilities,
    #[serde(rename = "_meta")]
    pub meta: MetaEnvelope,
    pub results: T,
}
