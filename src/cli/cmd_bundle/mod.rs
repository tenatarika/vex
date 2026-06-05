//! Phase 13.2 — `vex bundle` handler.
//!
//! Unified multi-source bundle primitive. One CLI subcommand + one MCP tool,
//! three modes (`symbol`, `pr-impact`, `project`) that assemble structured
//! responses from existing v6 index sections — no new index section, no
//! format bump.
//!
//! All three modes are implemented. See
//! `.claude/Task/PHASE13.2-bundle.md` for the design history and
//! architect-review decisions referenced by inline `A1`–`A10` markers
//! in source comments.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::args::ScopeArgs;
use crate::cli::common::CmdCtx;
use crate::cli::index_management::ensure_index_ready;
use crate::cli::output::print_envelope;
use crate::protocol::{capabilities, MetaEnvelope, Signals};
use crate::store::reader::IndexReader;
use crate::util::config;

mod pr_impact;
mod project;
mod symbol;

// Re-export the per-mode assemblers at the historical
// `crate::cli::cmd_bundle::*` path. `assemble_symbol` is used by
// `benches/bundle.rs`; `assemble_pr_impact`, `assemble_project`, and
// `MAX_PR_IMPACT_NODES` are documented as "Public for bench/test
// access" so we keep the path available even though no current bench
// uses them — the docs and the public surface stay aligned.
#[allow(unused_imports)]
pub use pr_impact::{assemble_pr_impact, MAX_PR_IMPACT_NODES};
#[allow(unused_imports)]
pub use project::assemble_project;
pub use symbol::assemble_symbol;

/// CLI-facing mode selector. Clap renders the variants in kebab-case:
/// `symbol`, `pr-impact`, `project`. Keep the variant order stable —
/// `capabilities::current().bundle_modes` is derived from
/// [`BundleModeFlag::ALL`] and downstream agents may rely on the order
/// they observe.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleModeFlag {
    Symbol,
    PrImpact,
    Project,
}

impl BundleModeFlag {
    /// Stable wire-format string. Must match the kebab-case rendering clap
    /// emits for `--mode` and the strings published in `bundle_modes`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::PrImpact => "pr-impact",
            Self::Project => "project",
        }
    }

    /// Stable ordering for `capabilities::current().bundle_modes`. The
    /// capabilities array MUST mirror this slice or downstream MCP
    /// clients will see drift.
    pub const ALL: [&'static str; 3] = ["symbol", "pr-impact", "project"];
}

/// Borrowed view of `Commands::Bundle { … }`. Constructed once in
/// `cmd_bundle()` so per-mode assemblers don't re-parse the variant.
/// Every field is consumed by at least one mode — the union shape is
/// intentional so the dispatch site stays uniform.
pub struct BundleArgs<'a> {
    pub mode: BundleModeFlag,
    pub symbol: Option<&'a str>,
    pub base: Option<&'a str>,
    pub depth: usize,
    pub path_glob: Option<&'a str>,
    pub top_n: usize,
    pub callers_max: usize,
    pub callees_max: usize,
    pub similar_max: usize,
    pub tests_max: usize,
    /// `project` mode only. When true the assembler skips the indegree
    /// walk and emits only `directory_tree` in `mode_hints`. See FU-6.
    pub directory_tree_only: bool,
    /// `project` mode only. Caps `directory_tree` entries sorted by
    /// `recursive_symbol_count` descending.
    pub directory_tree_top: usize,
}

/// Execution context built once at dispatch entry (architect-review A2 —
/// shared spine extracted from the existing CLI plumbing). Threaded into
/// every assembler so mode-specific functions stay narrow.
///
/// `reader` is the open `IndexReader`; assemblers borrow it for the
/// duration of `cmd_bundle()`. `hnsw_path` is required by
/// `search::similar::find_similar` for the symbol mode's semantic block.
/// `excludes` mirrors `&cfg.exclude` (substring excludes from `.vex.toml`)
/// — passed straight through to `diff::diff_against_base` for the
/// `pr-impact` mode. `scope` is the per-query glob filter from
/// `ScopeArgs`; pr-impact applies it as a post-filter on diff output.
pub struct BundleCtx<'a> {
    pub root: PathBuf,
    pub scope: &'a ScopeArgs,
    pub reader: &'a IndexReader,
    pub hnsw_path: PathBuf,
    pub excludes: &'a [String],
    /// v1.13 P5: `true` when the index's manifest records vectors as
    /// L2-normalized. Passed through to `find_similar` so the
    /// brute-force fallback uses the dot-product fast path. Defaults
    /// to `false` for pre-1.13 indexes (cosine path remains correct).
    pub vectors_normalized: bool,
}

/// One row in the bundle response. `core` carries the symbol pointer,
/// `signals` is the locked 13.11 block, `rank_percentile` is *global*
/// monotonic-descending (A6 — preserves the existing search-envelope
/// invariant), and `role_rank` is per-role 0-indexed for callers that
/// want to recover within-bucket ordering.
#[derive(Serialize, Clone, Debug)]
pub struct BundleItem {
    #[serde(flatten)]
    pub core: BundleCoreItem,
    pub signals: Signals,
    pub rank_percentile: f32,
    pub role_rank: u32,
    /// Discriminates which sub-list this item came from. Values:
    /// `body | caller | callee | similar | changed | transitive_caller |
    /// test | top`. Kept as `&'static str` so the variant set is closed
    /// at compile time.
    pub role: &'static str,
    /// Source body — only present for `role: "body"`. Skipped on every
    /// other role so the envelope stays compact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Cosine similarity — only present for `role: "similar"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
}

#[derive(Serialize, Clone, Debug)]
pub struct BundleCoreItem {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Per-mode assembly output. The CLI handler wraps this in a
/// `ResponseEnvelope` and prints it. `mode_hints` is an untyped JSON
/// blob whose shape varies per mode.
#[derive(Serialize, Clone, Debug)]
pub struct BundleResponse {
    pub mode: &'static str,
    pub items: Vec<BundleItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode_hints: Option<serde_json::Value>,
}

/// Per-mode meta-envelope additions (architect-review A3). The assembler
/// emits its own bits (e.g. `pr-impact` populates `diff_filter`); the CLI
/// handler merges them into the base `MetaEnvelope`.
#[derive(Default, Clone, Debug)]
pub struct ModeSpecificMeta {
    pub diff_filter: Option<serde_json::Value>,
}

/// Top-level CLI handler for `vex bundle`. Builds the envelope from the
/// per-mode assembler output and prints to stdout.
pub fn cmd_bundle(args: BundleArgs<'_>, ctx: BundleCtx<'_>) -> Result<()> {
    let (response, mode_meta) = match args.mode {
        BundleModeFlag::Symbol => assemble_symbol(&args, &ctx)?,
        BundleModeFlag::PrImpact => pr_impact::assemble_pr_impact(&args, &ctx)?,
        BundleModeFlag::Project => project::assemble_project(&args, &ctx)?,
    };

    let meta = MetaEnvelope {
        diff_filter: mode_meta.diff_filter,
        ..MetaEnvelope::default()
    };

    // v1.12.0 S8.2 — extends exit-code contract to `vex bundle`. The
    // envelope always carries `mode_hints` (even on empty), so we gate
    // strictly on `items` — that's the bit a caller treats as the
    // payload. `mode_hints.empty_reason` already explains *why*; the
    // exit code just lets scripts skip a `jq` call.
    if response.items.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    // Phase 13 envelope contract: every bundle mode (symbol / pr-impact /
    // project) emits through the shared `print_envelope` helper so the
    // wire shape stays aligned with the rest of the CLI surface. The
    // review caller (H5 partial) explicitly required that no bundle arm
    // emit raw `serde_json::to_string_pretty` — every mode flows through
    // this one call site.
    print_envelope(response, capabilities::current(), meta);
    Ok(())
}

/// Resolve the project root from an optional `--path` flag. Mirrors
/// `resolve_root` in `cli/mod.rs` but kept local to avoid a circular
/// re-export.
pub fn resolve_bundle_root(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => std::env::current_dir().map_err(Into::into),
    }
}

/// Dispatch-level wrapper for `vex bundle`. Resolves the project root,
/// opens the index, builds the `BundleArgs` + `BundleCtx`, and forwards
/// to [`cmd_bundle`]. Extracted from `cli/mod.rs` in S1 Group E so the
/// dispatch arm collapses to a one-liner.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bundle(
    ctx: &CmdCtx<'_>,
    mode: BundleModeFlag,
    symbol: Option<String>,
    base: Option<String>,
    depth: usize,
    path_glob: Option<String>,
    top_n: usize,
    directory_tree_only: bool,
    directory_tree_top: usize,
    callers_max: usize,
    callees_max: usize,
    similar_max: usize,
    tests_max: usize,
    path: Option<PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
    scope: ScopeArgs,
) -> Result<()> {
    // Inc 2 — open the index for `--mode symbol`. We pass
    // `needs_semantic=false` to `ensure_index_ready`: similar
    // results are best-effort (degrade to empty when the index
    // has no vectors), not a hard requirement. Other modes plug
    // into the same plumbing in Inc 3 / Inc 4.
    let root = resolve_bundle_root(path)?.canonicalize()?;
    let index_path = ensure_index_ready(
        &root,
        auto_update,
        no_stale_check,
        /*needs_semantic=*/ false,
        ctx.local_cache_active,
        ctx.cfg,
    )?;
    let reader = IndexReader::open(&index_path).context("open index")?;
    let hnsw_path = config::hnsw_path(&root);
    let args = BundleArgs {
        mode,
        symbol: symbol.as_deref(),
        base: base.as_deref(),
        depth,
        path_glob: path_glob.as_deref(),
        top_n,
        callers_max,
        callees_max,
        similar_max,
        tests_max,
        directory_tree_only,
        directory_tree_top,
    };
    let vectors_normalized = crate::index::manifest::Manifest::load(&config::manifest_path(&root))
        .ok()
        .and_then(|m| m.vectors_normalized)
        .unwrap_or(false);
    let bctx = BundleCtx {
        root,
        scope: &scope,
        reader: &reader,
        hnsw_path,
        excludes: ctx.excludes,
        vectors_normalized,
    };
    cmd_bundle(args, bctx)
}

// ---------------------------------------------------------------------------
// `--mode symbol` (Inc 2)
// ---------------------------------------------------------------------------

/// Default rank-percentile semantics: `1.0` for the top result, `0.0` for
/// the bottom, linear in between. Matches `print_search_envelope` in
/// `cli/output.rs` so callers can compare bundles to search envelopes
/// without translating ranks (architect-review A6 — preserves the
/// `search_envelope_rank_percentile_monotonic_descending` invariant).
pub(super) fn global_rank_percentile(idx: usize, total: usize) -> f32 {
    if total <= 1 {
        1.0
    } else {
        1.0 - (idx as f32 / (total - 1) as f32)
    }
}

pub(super) fn signals_fst_hit() -> Signals {
    Signals {
        fst_hit: true,
        ..Signals::default()
    }
}

/// Phase 14.1 — `CallMatch` doesn't carry `SymbolKind`, so derive the
/// bundled `kind` field from the caller name. Synthetic per-file Module
/// symbols are named `<module:path>` by `parse::parse_file`; everything
/// else is a real callable (fn / method / closure).
pub(super) fn caller_kind(name: &str) -> &'static str {
    if name.starts_with("<module:") {
        "module"
    } else {
        "function"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_mode_flag_as_str_round_trip() {
        assert_eq!(BundleModeFlag::Symbol.as_str(), "symbol");
        assert_eq!(BundleModeFlag::PrImpact.as_str(), "pr-impact");
        assert_eq!(BundleModeFlag::Project.as_str(), "project");
    }

    #[test]
    fn bundle_mode_flag_all_matches_capabilities() {
        assert_eq!(
            BundleModeFlag::ALL.as_slice(),
            capabilities::current().bundle_modes.as_slice()
        );
    }

    #[test]
    fn global_rank_percentile_is_monotonic_descending() {
        // N == 1 → single result is the best result.
        assert_eq!(global_rank_percentile(0, 1), 1.0);
        // N == 5 → 1.0 .. 0.0 inclusive, monotonic.
        let ranks: Vec<f32> = (0..5).map(|i| global_rank_percentile(i, 5)).collect();
        assert_eq!(ranks.first(), Some(&1.0));
        assert_eq!(ranks.last(), Some(&0.0));
        for win in ranks.windows(2) {
            assert!(
                win[0] > win[1],
                "rank_percentile must strictly decrease: {win:?}"
            );
        }
    }

    #[test]
    fn signals_fst_hit_sets_only_fst_flag() {
        let s = signals_fst_hit();
        assert!(s.fst_hit);
        assert_eq!(s.semantic_rank, None);
        assert_eq!(s.bm25_rank, None);
        assert_eq!(s.fuzzy_distance, None);
    }

    #[test]
    fn resolve_bundle_root_uses_explicit_some() {
        let r = resolve_bundle_root(Some(PathBuf::from("/tmp"))).unwrap();
        assert_eq!(r, std::path::Path::new("/tmp"));
    }
}
