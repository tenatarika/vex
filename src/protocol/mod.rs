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

/// Reason values for `_meta.vex.dev/semantic_channel` (single-repo) and the
/// per-repo `semantic_channel` field in `--workspace` output. Named so the
/// producer (`cmd_search::produce_results`) and the workspace advisory /
/// suppression checks stay in sync if the wire values ever change — a rename
/// then can't silently break a string comparison at a distant call site.
pub mod semantic_channel_reason {
    /// The caller did not pass `--semantic` / `semantic: true`.
    pub const NOT_REQUESTED: &str = "not_requested";
    /// The caller asked for semantic but the index has no embeddings;
    /// re-run `vex index --semantic`.
    pub const INDEX_LACKS_VECTORS: &str = "index_lacks_vectors";
}

#[derive(Serialize, Clone, Debug)]
pub struct Capabilities {
    pub signals: bool,
    pub empty_reason: bool,
    pub bundle_modes: Vec<&'static str>,
    pub why: bool,
    pub scope_filters: bool,
    pub metadata_filters: bool,
    pub auto_update: bool,
    /// Phase 14.9 Tier A.1: `vex history --diff` is available, and the
    /// JSON envelope's `results[*]` carry `body_diff: { from, to,
    /// hunks }` for non-head entries within each `(symbol, kind)`
    /// group. Lets MCP agents feature-detect the diff rendering.
    pub history_diff: bool,
}

#[derive(Serialize, Default, Clone, Debug)]
pub struct Signals {
    pub fst_hit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bm25_rank: Option<u32>,
    /// v1.20.0 (D4) — raw BM25 score from the pre-fusion channel,
    /// surfaced alongside `bm25_rank` so agents can read absolute
    /// quality (not just ordinal). `None` when this row did not
    /// appear in the BM25 channel. Stored as `f64` because BM25 is
    /// computed in `f64` throughout the search pipeline; contrast
    /// with `semantic_cosine` which is `f32` because the cosine
    /// computation is `f32`-native (no precision is lost on the
    /// downcast for normalized inputs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bm25_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_rank: Option<u32>,
    /// v1.20.0 (D4) — raw cosine similarity (range [-1.0, 1.0],
    /// typically [0.0, 1.0] post-normalization) from the pre-fusion
    /// semantic channel. `None` when this row did not appear in the
    /// semantic channel; absent silently if embeddings are not
    /// loaded (see `_meta.vex.dev/semantic_channel` for the reason).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_cosine: Option<f32>,
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

/// Internal concern-grouping of [`Signals`], used only at construction sites.
/// These sub-structs are **not serialized** — the wire `Signals` stays flat and
/// byte-identical (see `docs/PROTOCOL-EVOLUTION.md` §3.1). They exist so
/// construction expresses intent by channel family, and so the eventual v2
/// wire-nesting is a mechanical swap. Route all `Signals` construction through
/// [`Signals::from_parts`] — the single flat<->grouped mapping boundary — so a
/// future nesting change touches one function, not N scattered literals.
/// Sub-struct field names deliberately mirror the flat wire field names, so
/// [`Signals::from_parts`] is a pure 1:1 map (no silent rename hazard).
#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct StructuralSignals {
    pub fst_hit: bool,
}

/// Lexical (BM25 + fuzzy) channel signals — see [`StructuralSignals`].
#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct LexicalSignals {
    pub bm25_rank: Option<u32>,
    pub bm25_score: Option<f64>,
    pub fuzzy_distance: Option<u32>,
}

/// Semantic (embedding) channel signals — see [`StructuralSignals`].
#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct SemanticSignals {
    pub semantic_rank: Option<u32>,
    pub semantic_cosine: Option<f32>,
}

/// Post-fusion signals (reranker delta, call-graph indegree) — see
/// [`StructuralSignals`].
#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct PostSignals {
    pub rerank_boost: Option<f32>,
    pub indegree: Option<u32>,
}

impl Signals {
    /// The single flat<->grouped mapping boundary. Assembles the flat wire
    /// `Signals` from the four concern groups via an explicit field map —
    /// never `#[serde(flatten)]`, which drops `skip_serializing_if` and breaks
    /// byte-identity (`docs/PROTOCOL-EVOLUTION.md` §1a, invariant 1).
    #[must_use]
    pub(crate) fn from_parts(
        structural: StructuralSignals,
        lexical: LexicalSignals,
        semantic: SemanticSignals,
        post: PostSignals,
    ) -> Self {
        Signals {
            fst_hit: structural.fst_hit,
            bm25_rank: lexical.bm25_rank,
            bm25_score: lexical.bm25_score,
            semantic_rank: semantic.semantic_rank,
            semantic_cosine: semantic.semantic_cosine,
            fuzzy_distance: lexical.fuzzy_distance,
            rerank_boost: post.rerank_boost,
            indegree: post.indegree,
        }
    }
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
    /// Phase 14.9 Tier A.5: which path served a `vex history` query —
    /// `"indexed"` (Phase 14.8 sidecar FST lookup, ~ms) or
    /// `"walker"` (v1.16 query-time git-log walk, ~seconds). Only set
    /// by `cmd_history`. Same `vex.dev/` namespace as the other
    /// observability fields.
    #[serde(
        rename = "vex.dev/history_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub history_mode: Option<&'static str>,
    /// v1.20.0 (D4) — explicit reason the semantic channel did NOT
    /// contribute to a search result list. Absent (no key) when the
    /// semantic channel ran normally. One of:
    ///   * `"not_requested"` — caller did not pass `--semantic` /
    ///     `semantic: true`.
    ///   * `"index_lacks_vectors"` — caller asked for semantic but the
    ///     index has no embeddings; re-run `vex index --semantic`.
    ///
    /// Pre-D4 the semantic channel silently no-op'd in both cases and
    /// agents couldn't tell whether `semantic_rank: None` on a result
    /// meant "didn't match" or "channel didn't run". Same `vex.dev/`
    /// namespace as the other observability fields.
    #[serde(
        rename = "vex.dev/semantic_channel",
        skip_serializing_if = "Option::is_none"
    )]
    pub semantic_channel: Option<&'static str>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ResponseEnvelope<T: Serialize> {
    pub protocol_version: &'static str,
    pub capabilities: Capabilities,
    #[serde(rename = "_meta")]
    pub meta: MetaEnvelope,
    pub results: T,
}
