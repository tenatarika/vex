//! Reference channels — the abstraction behind `vex impact`'s
//! delete-safety report and (Phase 2) `vex usages`'s binder-vs-FST
//! split. Each channel resolves the question "what references this
//! symbol?" via a different mechanism (scope-binder, FST identifier
//! scan, grep word-boundary regex, call-graph indegree) and reports
//! its findings via a shared [`ChannelResult`] envelope.
//!
//! v1.21.0 architecture (extracted from the v1.20.0 inline 4-channel
//! body of `cmd_impact.rs`). Verdict logic is now data-driven over
//! `&[ChannelInvocation]` so adding a fifth channel does not require
//! touching [`derive_verdict`] — only appending another entry to the
//! channel slice in `cmd_impact::DEFAULT_CHANNELS`.
//!
//! ## Tier classification
//!
//! Channels split into two tiers (see [`ChannelTier`]):
//!
//! - **`Binder`** — evidence is binder/graph-confirmed. A hit from a
//!   binder channel implies a real reference; the verdict goes
//!   `Unsafe` (Phase 11.1 scope binder, Phase 10.2 call graph).
//! - **`Text`** — evidence is text-only (FST identifier match or
//!   regex scan). A hit might be a comment, string literal, or
//!   dynamic-dispatch reference that binders can't see; verdict is
//!   `Uncertain` when only text channels hit.
//!
//! ## Adding a new channel
//!
//! 1. Define a zero-sized type that implements [`Channel`].
//! 2. Pick the right [`ChannelTier`] — `Binder` if the channel can
//!    confirm "this is definitely a real reference" (e.g. when a new
//!    binder lands for Go), `Text` otherwise.
//! 3. Append the channel to [`DEFAULT_CHANNELS`] in
//!    `src/cli/cmd_impact.rs`.
//!
//! That's it. [`derive_verdict`] is data-driven — no edits needed
//! when channel count goes from 4 to 5 to N. The wire format
//! (`results.channels.<name>: ChannelResult`) is still struct-keyed
//! on the receiver side (`ImpactChannels`); see
//! `cmd_impact::ImpactChannels::from_invocations` for the bridge.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::scope::PathScope;
use crate::store::reader::IndexReader;
use crate::util::paths::to_rel_posix;

/// Cap on rows surfaced in each channel's `sample`. Verdict only
/// needs `≥1 vs 0` granularity; the sample exists so the agent can
/// see *where* the hits are without re-running a per-channel query.
pub const SAMPLE_LIMIT: usize = 10;

/// Hard cap on the grep channel — `\b<Name>\b` against the whole
/// project should never produce thousands of hits for a real symbol
/// name, but the regex scans every file so a bound matters.
pub const GREP_HARD_CAP: usize = 500;

/// Tier classification used by [`derive_verdict`] to know whether
/// a non-zero hit confirms (`Binder`) or only hints (`Text`) at real
/// usage. Binder-tier channels resolve references through the
/// scope-binder or call-graph index, so a non-zero count means
/// "definitely referenced". Text-tier channels see identifier
/// matches anywhere — comments, string literals, configuration
/// files — so a non-zero count means "maybe referenced".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelTier {
    Binder,
    Text,
}

/// A single (path, line) location surfaced by a channel.
#[derive(Debug, Serialize)]
pub struct HitLocation {
    pub path: String,
    pub line: u32,
}

/// Output of a channel run: whether it could execute, how many hits,
/// a bounded sample, and a truncation marker. Serialized to the wire
/// as `ImpactChannels.<name>: ChannelResult` so the field shape is
/// part of the v1 envelope contract.
#[derive(Debug, Serialize)]
pub struct ChannelResult {
    /// `false` when the channel could not run (e.g. the index lacks
    /// the requisite section). Verdict logic treats `available:
    /// false` as informationless — an unavailable binder channel
    /// reporting `count: 0` does NOT drag the verdict to `Safe`.
    pub available: bool,
    /// Human-readable reason when `available == false`. Skipped from
    /// JSON when `None` to keep the envelope tidy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Total post-filter hit count (sample may be smaller — see
    /// `truncated`).
    pub count: usize,
    /// First [`SAMPLE_LIMIT`] hits, surfaced so an agent can drill
    /// into specific files without re-running the per-channel query.
    pub sample: Vec<HitLocation>,
    /// `true` when `count > sample.len()` — the rest is omitted.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl ChannelResult {
    pub fn unavailable(reason: &str) -> Self {
        Self {
            available: false,
            unavailable_reason: Some(reason.to_string()),
            count: 0,
            sample: Vec::new(),
            truncated: false,
        }
    }

    pub fn from_hits(mut hits: Vec<HitLocation>) -> Self {
        let count = hits.len();
        let truncated = count > SAMPLE_LIMIT;
        hits.truncate(SAMPLE_LIMIT);
        Self {
            available: true,
            unavailable_reason: None,
            count,
            sample: hits,
            truncated,
        }
    }
}

/// Borrowed inputs every channel needs to run. Built once by the
/// caller and shared across all channels for a single `vex impact`
/// invocation.
pub struct ChannelContext<'a> {
    pub reader: &'a IndexReader,
    pub root: &'a Path,
    pub symbol: &'a str,
    pub file_paths: &'a [String],
    /// Def-site set used by FST + grep channels to strip the
    /// symbol's own declaration row (declarations are not "uses").
    /// Pre-computed once via [`build_def_sites`] so each channel
    /// avoids duplicate symbol-FST lookups.
    pub def_sites: &'a HashSet<(String, u32)>,
    pub path_scope: &'a PathScope,
    pub excludes: &'a [String],
    /// Opt-in: when true, FST + grep channels drop hits in
    /// `*.md`/`*.markdown`/`*.txt`/`*.rst`/`*.adoc` paths
    /// (see [`crate::util::paths::is_doc_path`]). v1.20.1 (D4
    /// parity with `vex search --code-only`). Off by default —
    /// the `Uncertain`-verdict story for prose-only mentions
    /// (e.g. a name appearing only in CHANGELOG) is the whole
    /// point of `vex impact`; this flag opts out for agents that
    /// want a code-only blast radius.
    pub exclude_docs: bool,
}

/// Trait implemented by each reference channel. Returns
/// `Ok(ChannelResult)` even when the channel could not run — that
/// shape goes into the wire envelope. Reserve `Err` for genuinely
/// unexpected failures (I/O errors, regex compile failures) that
/// should abort the whole impact query.
pub trait Channel: Sync {
    fn name(&self) -> &'static str;
    fn tier(&self) -> ChannelTier;
    fn run(&self, ctx: &ChannelContext<'_>) -> Result<ChannelResult>;
}

/// Data-driven verdict input: one slot per channel that ran. The
/// caller builds this vector by mapping `Channel::run` across its
/// channel list; [`derive_verdict`] never reads the channel impls
/// themselves, only this DTO. That's what lets a 5th channel land
/// without touching the verdict code.
pub struct ChannelInvocation {
    pub name: &'static str,
    pub tier: ChannelTier,
    pub result: ChannelResult,
}

/// Build the `(file_path, line)` set of the queried symbol's own
/// definitions. Used by FST + grep channels to strip declarations
/// from their hit counts (a declaration is not a "use"). Shared so
/// the symbol-FST lookup happens once per `vex impact` invocation,
/// not per channel.
///
/// Path normalisation invariant: the strings here come from
/// `read_string(sym.file_offset)`, which the writer routed through
/// `util::paths::to_rel_posix`. Match against `file_paths.get(...)`
/// values is byte-identical on every platform.
#[must_use]
pub fn build_def_sites(reader: &IndexReader, symbol: &str) -> HashSet<(String, u32)> {
    let mut set = HashSet::new();
    if let Some(sym_fst) = reader.symbol_fst_reader() {
        for sym_idx in sym_fst.find(symbol) {
            if let Some(sym) = reader.symbol(sym_idx as usize) {
                let file_path = reader.read_string(sym.file_offset).to_string();
                set.insert((file_path, sym.line));
            }
        }
    }
    set
}

/// Three-valued impact verdict. Wire serialisation uses snake_case
/// (`"safe"`, `"unsafe"`, `"uncertain"`) to match the
/// `results.verdict` field documented in `docs/MCP-SCHEMA.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Safe,
    Unsafe,
    Uncertain,
}

/// Decide the verdict from the channel invocation list. Data-driven
/// over [`ChannelInvocation`] — adding a fifth channel does not
/// require touching this function, only appending to the channel
/// slice the caller passes to `Channel::run`.
///
/// Rules (see `docs/LIMITATIONS.md` §6 for the user-facing version):
///
/// - **`Unsafe`** ⇔ any `Binder`-tier channel reported `available:
///   true && count > 0`. Binder/graph confirmed real usage.
/// - **`Uncertain`** ⇔ only `Text`-tier channels hit (likely
///   string-literal / comment / dynamic-dispatch mentions) **OR**
///   every binder channel reported `available: false` (no binder
///   evidence either way — the verdict is informationless without
///   re-indexing).
/// - **`Safe`** ⇔ every channel that ran reported zero hits AND at
///   least one binder channel was available to confirm. An
///   unavailable binder channel's `count: 0` is not a confirmation.
///
/// `verdict_explanation` enumerates which channels actually ran and
/// which were unavailable so the agent can read the conclusion
/// without a false "all channels reported zero" claim when binder
/// sections are missing from a pre-v1.8 index.
#[must_use]
pub fn derive_verdict(invocations: &[ChannelInvocation]) -> (Verdict, String) {
    let mut binder_confirms: Vec<(&str, usize)> = Vec::new();
    let mut text_hits: Vec<(&str, usize)> = Vec::new();
    let mut unavailable: Vec<&str> = Vec::new();
    let mut available_names: Vec<&str> = Vec::new();
    let mut any_binder_available = false;

    for inv in invocations {
        if !inv.result.available {
            unavailable.push(inv.name);
            continue;
        }
        available_names.push(inv.name);
        match inv.tier {
            ChannelTier::Binder => {
                any_binder_available = true;
                if inv.result.count > 0 {
                    binder_confirms.push((inv.name, inv.result.count));
                }
            }
            ChannelTier::Text => {
                if inv.result.count > 0 {
                    text_hits.push((inv.name, inv.result.count));
                }
            }
        }
    }

    let unavailable_note = if unavailable.is_empty() {
        String::new()
    } else {
        format!(
            " (unavailable: {} — re-run `vex index` for stronger evidence)",
            unavailable.join(", ")
        )
    };

    if !binder_confirms.is_empty() {
        let reasons = binder_confirms
            .iter()
            .map(|(n, c)| format!("{n}={c}"))
            .collect::<Vec<_>>()
            .join(", ");
        return (
            Verdict::Unsafe,
            format!(
                "binder/graph confirmed real usage ({reasons}). Do not delete without rewriting call sites.",
            ),
        );
    }

    if !text_hits.is_empty() {
        let reasons = text_hits
            .iter()
            .map(|(n, c)| format!("{n}={c}"))
            .collect::<Vec<_>>()
            .join(", ");
        return (
            Verdict::Uncertain,
            format!(
                "text-only matches surfaced ({reasons}) but binder/call-graph saw none{unavailable_note}. \
                 Likely string-literal mentions, comments, or dynamic dispatch — manual inspection required.",
            ),
        );
    }

    if !any_binder_available {
        return (
            Verdict::Uncertain,
            "text channels reported zero hits, but no binder channel ran on this index \
             (pre-v1.8 / pre-Phase 10.2, or no binder coverage for this project's languages). \
             Re-run `vex index` to rebuild before relying on a verdict."
                .to_string(),
        );
    }

    (
        Verdict::Safe,
        format!(
            "{} of {} channels reported zero hits ({}){unavailable_note}. \
             Delete is highly likely safe.",
            available_names.len(),
            invocations.len(),
            available_names.join(", "),
        ),
    )
}

// ───────────────────────── Channel implementations ────────────────────────

/// Phase 11.1 v5 `reference_edges` — binder-resolved cross-file refs.
/// Available for Rust / TypeScript / Python / C# / C++; reports
/// `unavailable` on pre-v1.8 indexes or projects in languages
/// without a binder.
pub struct StrictRefsChannel;

impl Channel for StrictRefsChannel {
    fn name(&self) -> &'static str {
        "strict_refs"
    }
    fn tier(&self) -> ChannelTier {
        ChannelTier::Binder
    }
    fn run(&self, ctx: &ChannelContext<'_>) -> Result<ChannelResult> {
        if !ctx.reader.has_ref_edges() {
            return Ok(ChannelResult::unavailable(
                "index has no v5 reference_edges section (pre-v1.8 index, or no binder coverage \
                 for this project's languages) — re-run `vex index` to rebuild",
            ));
        }
        let sym_fst = ctx
            .reader
            .symbol_fst_reader()
            .context("symbol FST missing — re-run `vex index` to rebuild")?;
        let mut hits = Vec::new();
        for sym_idx in sym_fst.find(ctx.symbol) {
            for edge in ctx.reader.find_ref_edges_by_symbol(sym_idx) {
                let path = ctx
                    .file_paths
                    .get(edge.from_file_id as usize)
                    .cloned()
                    .unwrap_or_else(|| "?".to_string());
                if !ctx.path_scope.accept(&path) {
                    continue;
                }
                hits.push(HitLocation {
                    path,
                    line: edge.line,
                });
            }
        }
        Ok(ChannelResult::from_hits(hits))
    }
}

/// Legacy FST identifier scan. Catches every CamelCase / snake_case
/// occurrence of the name from AST identifier nodes. False-positives
/// on comments and string literals (Text tier). By default does NOT
/// apply `is_doc_path` — impact's job is to surface prose mentions
/// so a symbol referenced only in CHANGELOG yields `Uncertain`
/// instead of falsely-confident `Safe`. Opt-in `ChannelContext::exclude_docs`
/// (v1.20.1, D4 parity) strips prose paths for agents that explicitly
/// want a code-only blast radius. Def-site filter IS always applied
/// (declarations aren't "uses").
pub struct FstRefsChannel;

impl Channel for FstRefsChannel {
    fn name(&self) -> &'static str {
        "fst_refs"
    }
    fn tier(&self) -> ChannelTier {
        ChannelTier::Text
    }
    fn run(&self, ctx: &ChannelContext<'_>) -> Result<ChannelResult> {
        let Some(ref_reader) = ctx.reader.ref_reader() else {
            return Ok(ChannelResult::unavailable(
                "index has no refs FST — re-run `vex index`",
            ));
        };
        let hits: Vec<HitLocation> = ref_reader
            .find(ctx.symbol)
            .into_iter()
            .filter_map(|e| {
                let path = ctx.file_paths.get(e.file_id as usize).cloned()?;
                if ctx.def_sites.contains(&(path.clone(), e.line)) {
                    return None;
                }
                if !ctx.path_scope.accept(&path) {
                    return None;
                }
                if ctx.exclude_docs && crate::util::paths::is_doc_path(&path) {
                    return None;
                }
                Some(HitLocation { path, line: e.line })
            })
            .collect();
        Ok(ChannelResult::from_hits(hits))
    }
}

/// Word-boundary regex scan via `crate::grep`. Catches what the
/// AST-walking pipelines skip: string-literal dispatch, macros,
/// configuration files, prose mentions. Text tier. Like
/// `FstRefsChannel`, by default does NOT apply `is_doc_path` —
/// filtering prose paths here would defeat the `Uncertain` verdict
/// for symbols whose only references are external (covered by the
/// integration test `impact_verdict_uncertain_when_only_text_mentions_in_docs`).
/// Opt-in `ChannelContext::exclude_docs` strips prose paths
/// for agents that explicitly want a code-only blast radius.
pub struct GrepWordBoundaryChannel;

impl Channel for GrepWordBoundaryChannel {
    fn name(&self) -> &'static str {
        "grep_word_boundary"
    }
    fn tier(&self) -> ChannelTier {
        ChannelTier::Text
    }
    fn run(&self, ctx: &ChannelContext<'_>) -> Result<ChannelResult> {
        let escaped = regex::escape(ctx.symbol);
        let pattern = format!(r"\b{escaped}\b");
        let raw = crate::grep::search(ctx.root, &pattern, None, GREP_HARD_CAP, ctx.excludes)
            .context("grep word-boundary scan")?;
        let hits: Vec<HitLocation> = raw
            .into_iter()
            .filter_map(|m| {
                // Normalize grep's native-separator relative path to
                // POSIX so the def-site HashSet (built from
                // index-stored POSIX paths) matches on Windows too.
                let posix =
                    to_rel_posix(&ctx.root.join(&m.path), ctx.root).unwrap_or(m.path.clone());
                let line = u32::try_from(m.line).ok()?;
                if ctx.def_sites.contains(&(posix.clone(), line)) {
                    return None;
                }
                if !ctx.path_scope.accept(&posix) {
                    return None;
                }
                if ctx.exclude_docs && crate::util::paths::is_doc_path(&posix) {
                    return None;
                }
                Some(HitLocation { path: posix, line })
            })
            .collect();
        Ok(ChannelResult::from_hits(hits))
    }
}

/// Phase 10.2 v4 call graph — direct callers via
/// `find_callers_fast`. Binder tier (a call edge is a confirmed
/// reference). Reports `unavailable` on pre-v4 indexes or when the
/// call-edges section is empty.
pub struct CallGraphCallersChannel;

impl Channel for CallGraphCallersChannel {
    fn name(&self) -> &'static str {
        "call_graph_callers"
    }
    fn tier(&self) -> ChannelTier {
        ChannelTier::Binder
    }
    fn run(&self, ctx: &ChannelContext<'_>) -> Result<ChannelResult> {
        if !ctx.reader.has_call_graph() {
            return Ok(ChannelResult::unavailable(
                "index has no v4 call graph section (pre-Phase 10.2 index, or empty call-edges) \
                 — re-run `vex index`",
            ));
        }
        let callers = crate::store::call_graph::find_callers_fast(
            ctx.reader,
            ctx.symbol,
            crate::callgraph::CALLERS_FETCH_CAP,
        );
        let hits: Vec<HitLocation> = callers
            .into_iter()
            .filter_map(|m| {
                if !ctx.path_scope.accept(&m.path) {
                    return None;
                }
                let line = u32::try_from(m.line).ok()?;
                Some(HitLocation { path: m.path, line })
            })
            .collect();
        Ok(ChannelResult::from_hits(hits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_result(count: usize, available: bool) -> ChannelResult {
        if !available {
            return ChannelResult::unavailable("test");
        }
        let hits = (0..count)
            .map(|i| HitLocation {
                path: format!("f{i}.rs"),
                line: (i + 1) as u32,
            })
            .collect();
        ChannelResult::from_hits(hits)
    }

    /// Helper: build the standard 4-channel invocation list for
    /// verdict-logic tests. Tier classifications mirror the
    /// production `DEFAULT_CHANNELS` order in cmd_impact.rs:
    /// strict_refs (Binder), fst_refs (Text), grep_word_boundary
    /// (Text), call_graph_callers (Binder).
    fn standard_invocations(
        strict: ChannelResult,
        fst: ChannelResult,
        grep: ChannelResult,
        callers: ChannelResult,
    ) -> Vec<ChannelInvocation> {
        vec![
            ChannelInvocation {
                name: "strict_refs",
                tier: ChannelTier::Binder,
                result: strict,
            },
            ChannelInvocation {
                name: "fst_refs",
                tier: ChannelTier::Text,
                result: fst,
            },
            ChannelInvocation {
                name: "grep_word_boundary",
                tier: ChannelTier::Text,
                result: grep,
            },
            ChannelInvocation {
                name: "call_graph_callers",
                tier: ChannelTier::Binder,
                result: callers,
            },
        ]
    }

    #[test]
    fn verdict_safe_when_all_channels_zero() {
        let invs = standard_invocations(
            fake_result(0, true),
            fake_result(0, true),
            fake_result(0, true),
            fake_result(0, true),
        );
        let (v, why) = derive_verdict(&invs);
        assert!(matches!(v, Verdict::Safe), "got: {v:?}");
        assert!(
            why.contains("4 of 4 channels reported zero hits"),
            "explanation must enumerate every channel that ran, got: {why}"
        );
        assert!(
            !why.contains("unavailable"),
            "no unavailable_note when all channels ran, got: {why}"
        );
    }

    #[test]
    fn verdict_safe_explanation_lists_unavailable_channels() {
        // strict ran and saw 0, FST + callers unavailable, grep 0.
        // Safe is justified (strict_refs confirmed) but the
        // explanation must enumerate which channels were unavailable.
        let invs = standard_invocations(
            fake_result(0, true),
            fake_result(0, false),
            fake_result(0, true),
            fake_result(0, false),
        );
        let (v, why) = derive_verdict(&invs);
        assert!(matches!(v, Verdict::Safe), "got: {v:?}");
        assert!(
            why.contains("unavailable: fst_refs, call_graph_callers"),
            "explanation must enumerate unavailable channels in invocation order, got: {why}"
        );
        assert!(
            why.contains("2 of 4 channels"),
            "explanation must count only channels that ran, got: {why}"
        );
    }

    #[test]
    fn verdict_unsafe_when_strict_confirms() {
        let invs = standard_invocations(
            fake_result(3, true),
            fake_result(5, true),
            fake_result(7, true),
            fake_result(0, true),
        );
        let (v, why) = derive_verdict(&invs);
        assert!(matches!(v, Verdict::Unsafe), "got: {v:?}");
        assert!(
            why.contains("strict_refs=3"),
            "explanation must cite strict count, got: {why}"
        );
    }

    #[test]
    fn verdict_unsafe_when_callers_confirm_without_strict() {
        // Reflection / decorator path: binder doesn't index the ref
        // but the call-graph extractor sees the call. Must be unsafe.
        let invs = standard_invocations(
            fake_result(0, true),
            fake_result(0, true),
            fake_result(0, true),
            fake_result(2, true),
        );
        let (v, why) = derive_verdict(&invs);
        assert!(matches!(v, Verdict::Unsafe), "got: {v:?}");
        assert!(
            why.contains("call_graph_callers=2"),
            "explanation must cite callers count, got: {why}"
        );
    }

    #[test]
    fn verdict_uncertain_when_only_text_channels_hit() {
        let invs = standard_invocations(
            fake_result(0, true),
            fake_result(2, true),
            fake_result(3, true),
            fake_result(0, true),
        );
        let (v, why) = derive_verdict(&invs);
        assert!(matches!(v, Verdict::Uncertain), "got: {v:?}");
        assert!(
            why.contains("manual inspection"),
            "explanation must mention manual review, got: {why}"
        );
    }

    #[test]
    fn verdict_uncertain_when_all_binder_channels_unavailable_even_with_grep_zero() {
        let invs = standard_invocations(
            fake_result(0, false),
            fake_result(0, false),
            fake_result(0, true),
            fake_result(0, false),
        );
        let (v, why) = derive_verdict(&invs);
        assert!(
            matches!(v, Verdict::Uncertain),
            "all binders unavailable + grep=0 must NOT be safe; got: {v:?}"
        );
        assert!(
            why.contains("Re-run `vex index`"),
            "explanation must point at re-indexing, got: {why}"
        );
    }

    #[test]
    fn verdict_uncertain_explanation_notes_strict_unavailability() {
        let invs = standard_invocations(
            fake_result(0, false),
            fake_result(0, true),
            fake_result(1, true),
            fake_result(0, true),
        );
        let (v, why) = derive_verdict(&invs);
        assert!(matches!(v, Verdict::Uncertain), "got: {v:?}");
        assert!(
            why.contains("unavailable: strict_refs"),
            "uncertain explanation must mention which binder channel didn't run, got: {why}"
        );
    }

    #[test]
    fn unavailable_strict_channel_does_not_force_safe_on_grep_hit() {
        let invs = standard_invocations(
            fake_result(0, false),
            fake_result(0, false),
            fake_result(1, true),
            fake_result(0, false),
        );
        let (v, _) = derive_verdict(&invs);
        assert!(
            matches!(v, Verdict::Uncertain),
            "unavailable strict + grep hit must be uncertain, got: {v:?}"
        );
    }

    /// v1.21.0 — future-channel proof. Adding a 5th channel should
    /// drop into the verdict logic without touching it. This test
    /// runs `derive_verdict` against a 5-invocation list including
    /// a hypothetical binder-tier channel and asserts the explanation
    /// counts/lists the 5 channels correctly.
    #[test]
    fn verdict_is_data_driven_over_arbitrary_channel_count() {
        let invs = vec![
            ChannelInvocation {
                name: "strict_refs",
                tier: ChannelTier::Binder,
                result: fake_result(0, true),
            },
            ChannelInvocation {
                name: "fst_refs",
                tier: ChannelTier::Text,
                result: fake_result(0, true),
            },
            ChannelInvocation {
                name: "grep_word_boundary",
                tier: ChannelTier::Text,
                result: fake_result(0, true),
            },
            ChannelInvocation {
                name: "call_graph_callers",
                tier: ChannelTier::Binder,
                result: fake_result(0, true),
            },
            ChannelInvocation {
                name: "hypothetical_future_binder",
                tier: ChannelTier::Binder,
                result: fake_result(0, true),
            },
        ];
        let (v, why) = derive_verdict(&invs);
        assert!(matches!(v, Verdict::Safe), "got: {v:?}");
        assert!(
            why.contains("5 of 5 channels reported zero hits"),
            "explanation must scale with channel count, got: {why}"
        );
        assert!(
            why.contains("hypothetical_future_binder"),
            "new channel name must appear in the available-channels list, got: {why}"
        );
    }

    /// Trait identity sanity: tier classification on the 4 concrete
    /// channels must match the documented contract. Locks the
    /// Binder/Text split so a future refactor that swaps a tier
    /// (e.g. moving FST into the Binder pool) fires this test
    /// loudly rather than silently changing verdict semantics.
    #[test]
    fn channel_tier_classification_is_stable() {
        assert_eq!(StrictRefsChannel.tier(), ChannelTier::Binder);
        assert_eq!(FstRefsChannel.tier(), ChannelTier::Text);
        assert_eq!(GrepWordBoundaryChannel.tier(), ChannelTier::Text);
        assert_eq!(CallGraphCallersChannel.tier(), ChannelTier::Binder);
    }

    #[test]
    fn channel_names_are_stable_wire_keys() {
        // ImpactChannels::from_invocations matches by these strings;
        // changing them is a wire-format break.
        assert_eq!(StrictRefsChannel.name(), "strict_refs");
        assert_eq!(FstRefsChannel.name(), "fst_refs");
        assert_eq!(GrepWordBoundaryChannel.name(), "grep_word_boundary");
        assert_eq!(CallGraphCallersChannel.name(), "call_graph_callers");
    }
}
