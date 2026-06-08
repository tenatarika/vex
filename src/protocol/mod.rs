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
    /// v1.15.1 HIGH: set to `Some(true)` when the on-disk index is stale
    /// AND an auto-update attempt failed during this request. The
    /// envelope still carries `results` against the existing (stale)
    /// index — the caller should treat the data as a best-effort
    /// snapshot, not a fresh refresh.
    ///
    /// Pre-v1.15.1 a failed `pipeline::update` bubbled up as a non-zero
    /// CLI exit → the MCP wrapper surfaced an error or, in one observed
    /// case for `vex usages`, an empty `{results: []}` wrapped in an
    /// MCP error string. Agents trusted the "0 results" answer and made
    /// wrong decisions. This field is the explicit "the answer you got
    /// is stale, here's why" signal.
    #[serde(rename = "vex.dev/stale", skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
    /// Human-readable reason the auto-update failed when [`Self::stale`]
    /// is set. Verbatim from `pipeline::update`'s formatted error chain
    /// (`{e:#}`). Typically short enough to log directly.
    #[serde(
        rename = "vex.dev/stale_reason",
        skip_serializing_if = "Option::is_none"
    )]
    pub stale_reason: Option<String>,
    /// v1.15.1: trace data from `--why`-eligible commands (currently
    /// `vex usages --strict --why`). Pre-v1.15.1 the trace was only
    /// emitted on stderr (`VEX_WHY:` prefix) and never attached to the
    /// success envelope — JSON consumers couldn't observe it. The same
    /// `vex.dev/` namespace as the other observability fields.
    #[serde(rename = "vex.dev/why_trace", skip_serializing_if = "Option::is_none")]
    pub why_trace: Option<serde_json::Value>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ResponseEnvelope<T: Serialize> {
    pub protocol_version: &'static str,
    pub capabilities: Capabilities,
    #[serde(rename = "_meta")]
    pub meta: MetaEnvelope,
    pub results: T,
}
