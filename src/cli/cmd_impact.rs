//! `vex impact <Symbol>` — one-call blast-radius / delete-safety
//! report. v1.20.0 (F1), refactored in v1.21.0 to route through the
//! `Channel` trait (`src/channel/mod.rs`).
//!
//! This file is now a thin orchestrator: build a [`ChannelContext`],
//! map [`DEFAULT_CHANNELS`] across it to collect a `Vec<ChannelInvocation>`,
//! pass to [`derive_verdict`], and serialize the result via the wire
//! envelope ([`ImpactReport`] / [`ImpactChannels`]). Per-channel
//! iteration logic lives in `src/channel/mod.rs` so future channels
//! (e.g. a Go binder) drop in without touching this file or the
//! verdict logic — only `DEFAULT_CHANNELS` grows.
//!
//! See the `channel` module docs for the trait shape and tier
//! classification rule. `docs/LIMITATIONS.md` §6 documents the
//! user-facing verdict contract.

use anyhow::{Context, Result};
use serde::Serialize;

use super::args::{OutputFormat, ScopeArgs};
use super::common::{resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::scope;
use crate::channel::{
    build_def_sites, derive_verdict, CallGraphCallersChannel, Channel, ChannelContext,
    ChannelInvocation, ChannelResult, FstRefsChannel, GrepWordBoundaryChannel, StrictRefsChannel,
    TransitiveCallersChannel, Verdict,
};
use crate::protocol::capabilities;
use crate::store::reader::IndexReader;

/// Default channel set in invocation order. Order matters for the
/// `verdict_explanation` text — "unavailable: ..." and "X of Y
/// channels (...)" lists items in this order. Adding a 5th channel:
/// append here; verdict logic and wire format
/// ([`ImpactChannels::from_invocations`]) need no changes if the new
/// channel reports through the same `Channel` trait.
const DEFAULT_CHANNELS: &[&dyn Channel] = &[
    &StrictRefsChannel,
    &FstRefsChannel,
    &GrepWordBoundaryChannel,
    &CallGraphCallersChannel,
    &TransitiveCallersChannel,
];

/// Upper bound for `--depth N`. `vex reachable` defaults its hop
/// budget to 6; `vex impact --depth` allows a more conservative 16
/// ceiling for delete-safety blast radius (deeper walks are rarely
/// useful on monorepos and the BFS visited set prevents cycles, but
/// 16 is the practical cliff). The default value (`1`) is owned by
/// clap (`default_value = "1"` on the CLI arg) so the constant here
/// is purely the safety ceiling.
const MAX_DEPTH: u32 = 16;

/// Wire-format envelope for the four invocation results. v1.21.0
/// ships a struct (one field per channel) so MCP clients can dot-access
/// via `channels.strict_refs` rather than scanning an array; this keeps
/// the receiver-side ergonomics even though the channel pipeline is
/// data-driven. Adding a fifth channel here is a wire-format addition
/// — the field name must match the channel's `name()` and the JSON
/// stays additive.
#[derive(Debug, Serialize)]
struct ImpactChannels {
    strict_refs: ChannelResult,
    fst_refs: ChannelResult,
    grep_word_boundary: ChannelResult,
    call_graph_callers: ChannelResult,
    /// v1.21.0 — transitive callers up to `--depth N` (default 1
    /// reports `available: false` here so the wire envelope stays
    /// additive without changing the depth-1 default story).
    transitive_callers: ChannelResult,
}

impl ImpactChannels {
    /// Build the wire-format struct from the data-driven invocation
    /// list. Returns `Err` when a required channel name is absent —
    /// that means `DEFAULT_CHANNELS` and this struct's fields drifted
    /// (each `take` here MUST match a `name()` in the slice). The
    /// error propagates through the orchestrator as a clean
    /// `anyhow::Error` instead of panicking in production.
    ///
    /// O(N²) over the channel count (each `take` scans the vec
    /// linearly). N is the channel count — currently 4, capped well
    /// below 10 in any conceivable future — so the quadratic shape
    /// is irrelevant; struct-field ergonomics are worth more than
    /// the asymptotic.
    fn from_invocations(mut invocations: Vec<ChannelInvocation>) -> Result<Self> {
        fn take(invs: &mut Vec<ChannelInvocation>, name: &str) -> Result<ChannelResult> {
            let pos = invs.iter().position(|i| i.name == name).with_context(|| {
                format!(
                    "channel `{name}` missing from invocation list — \
                     DEFAULT_CHANNELS and ImpactChannels drifted; the \
                     wire envelope would silently lose a channel"
                )
            })?;
            Ok(invs.swap_remove(pos).result)
        }
        Ok(Self {
            strict_refs: take(&mut invocations, "strict_refs")?,
            fst_refs: take(&mut invocations, "fst_refs")?,
            grep_word_boundary: take(&mut invocations, "grep_word_boundary")?,
            call_graph_callers: take(&mut invocations, "call_graph_callers")?,
            transitive_callers: take(&mut invocations, "transitive_callers")?,
        })
    }
}

#[derive(Debug, Serialize)]
struct ImpactReport {
    symbol: String,
    verdict: Verdict,
    /// One-line human-readable derivation of the verdict so an
    /// agent can quote it back to the user without re-running the
    /// rule. See `crate::channel::derive_verdict` for the rule.
    verdict_explanation: String,
    channels: ImpactChannels,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn impact(
    ctx: &CmdCtx<'_>,
    name: String,
    path: Option<std::path::PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
    exclude_docs: bool,
    depth: u32,
    scope: ScopeArgs,
) -> Result<()> {
    // Clamp `--depth` to `[1, MAX_DEPTH]`. `0` is meaningless (no
    // callers ever) and explicit user input above `MAX_DEPTH` would
    // run the BFS unbounded on monorepos. Silent clamp instead of
    // bailing — agents that ask for depth=100 still want a useful
    // answer at depth=16. Emit a `tracing::warn!` on out-of-range so
    // humans running interactively see the clamp; agents reading
    // JSON ignore stderr.
    let original_depth = depth;
    let depth = depth.clamp(1, MAX_DEPTH);
    if original_depth != depth {
        tracing::warn!(
            requested = original_depth,
            applied = depth,
            max = MAX_DEPTH,
            "vex impact: --depth clamped to [1, {MAX_DEPTH}]"
        );
    }
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(path)?.canonicalize()?;
    let index_path = ensure_index_ready(
        &root,
        auto_update,
        no_stale_check,
        false,
        ctx.local_cache_active,
        ctx.cfg,
    )?;
    let reader = IndexReader::open(&index_path).context("open index")?;
    let file_paths = reader.file_paths();
    let def_sites = build_def_sites(&reader, &name);

    let channel_ctx = ChannelContext {
        reader: &reader,
        root: &root,
        symbol: &name,
        file_paths: &file_paths,
        def_sites: &def_sites,
        path_scope: &path_scope,
        excludes: ctx.excludes,
        filter_def_sites: true,
        exclude_docs,
        depth,
    };

    // Run every channel; collect into a vec for the data-driven
    // verdict. Channels report `Ok(ChannelOutput::unavailable(...))`
    // when they can't run (pre-v1.8 index, missing call graph), so an
    // `Err` here is a genuine fault (I/O failure, corrupt regex) and
    // bubbles up to the caller. Phase 2: the trait now returns
    // `ChannelOutput` (full hits + drop counters); the wire envelope
    // takes the `SAMPLE_LIMIT` cap via `ChannelResult::from_output`.
    let invocations: Vec<ChannelInvocation> = DEFAULT_CHANNELS
        .iter()
        .map(|ch| {
            Ok::<_, anyhow::Error>(ChannelInvocation {
                name: ch.name(),
                tier: ch.tier(),
                result: ChannelResult::from_output(ch.run(&channel_ctx)?),
            })
        })
        .collect::<Result<_>>()?;

    let (verdict, verdict_explanation) = derive_verdict(&invocations);
    let channels = ImpactChannels::from_invocations(invocations)?;
    let report = ImpactReport {
        symbol: name.clone(),
        verdict,
        verdict_explanation,
        channels,
    };

    // Verdict alone never triggers `signal_no_results` — even `safe`
    // is a meaningful answer that the exit code should reflect as
    // success (0). Scripts and CI gates that gate on
    // `vex impact ... && delete` interpret the verdict by reading the
    // envelope, not the exit code. See `docs/EXIT-CODES.md` for the
    // contract.

    match ctx.format {
        OutputFormat::Json => {
            print_envelope(
                &report,
                capabilities::current(),
                super::output::default_meta_for(&root),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            println!("impact: {}", report.symbol);
            println!(
                "  verdict: {} — {}",
                match report.verdict {
                    Verdict::Safe => "safe",
                    Verdict::Unsafe => "unsafe",
                    Verdict::Uncertain => "uncertain",
                },
                report.verdict_explanation
            );
            print_channel("strict_refs", &report.channels.strict_refs);
            print_channel("fst_refs", &report.channels.fst_refs);
            print_channel("grep_word_boundary", &report.channels.grep_word_boundary);
            print_channel("call_graph_callers", &report.channels.call_graph_callers);
            print_channel("transitive_callers", &report.channels.transitive_callers);
        }
    }

    Ok(())
}

fn print_channel(name: &str, ch: &ChannelResult) {
    if !ch.available {
        println!(
            "  {name}: unavailable ({})",
            ch.unavailable_reason.as_deref().unwrap_or("unknown reason")
        );
        return;
    }
    println!(
        "  {name}: {}{}",
        ch.count,
        if ch.truncated {
            format!(" (showing first {} of {})", ch.sample.len(), ch.count)
        } else {
            String::new()
        }
    );
    for hit in &ch.sample {
        println!("    {}:{}", hit.path, hit.line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelResult, ChannelTier, HitLocation};

    /// Wire-format bridge sanity: `ImpactChannels::from_invocations`
    /// must drop each `ChannelInvocation.result` into the
    /// correctly-named struct field. A drift between
    /// `DEFAULT_CHANNELS` and `ImpactChannels` fields would silently
    /// break the JSON envelope — the per-channel verdict-logic tests
    /// can't catch that because they operate on the invocation list,
    /// not the serialized struct.
    #[test]
    fn from_invocations_routes_results_by_name() {
        let invocations = vec![
            ChannelInvocation {
                name: "strict_refs",
                tier: ChannelTier::Binder,
                result: ChannelResult::from_hits(vec![HitLocation {
                    path: "strict.rs".into(),
                    line: 1,
                }]),
            },
            ChannelInvocation {
                name: "fst_refs",
                tier: ChannelTier::Text,
                result: ChannelResult::from_hits(vec![HitLocation {
                    path: "fst.rs".into(),
                    line: 2,
                }]),
            },
            ChannelInvocation {
                name: "grep_word_boundary",
                tier: ChannelTier::Text,
                result: ChannelResult::from_hits(vec![HitLocation {
                    path: "grep.rs".into(),
                    line: 3,
                }]),
            },
            ChannelInvocation {
                name: "call_graph_callers",
                tier: ChannelTier::Binder,
                result: ChannelResult::from_hits(vec![HitLocation {
                    path: "callers.rs".into(),
                    line: 4,
                }]),
            },
            ChannelInvocation {
                name: "transitive_callers",
                tier: ChannelTier::Binder,
                result: ChannelResult::from_hits(vec![HitLocation {
                    path: "transitive.rs".into(),
                    line: 5,
                }]),
            },
        ];
        let channels = ImpactChannels::from_invocations(invocations)
            .expect("from_invocations must succeed for a complete invocation list");
        assert_eq!(channels.strict_refs.sample[0].path, "strict.rs");
        assert_eq!(channels.fst_refs.sample[0].path, "fst.rs");
        assert_eq!(channels.grep_word_boundary.sample[0].path, "grep.rs");
        assert_eq!(channels.call_graph_callers.sample[0].path, "callers.rs");
        assert_eq!(channels.transitive_callers.sample[0].path, "transitive.rs");
    }

    #[test]
    fn default_channels_list_matches_impact_channels_fields() {
        // Tier classification is part of the verdict contract.
        // Locks the (name → tier) mapping so a future refactor that
        // reclassifies a channel can't silently change verdict
        // semantics — the `channel::tests::channel_tier_classification_is_stable`
        // test also pins this on the impl side; this test pins it on
        // the orchestrator-list side so a future `DEFAULT_CHANNELS`
        // edit can't drift from the channel module.
        let names: Vec<_> = DEFAULT_CHANNELS.iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            vec![
                "strict_refs",
                "fst_refs",
                "grep_word_boundary",
                "call_graph_callers",
                "transitive_callers",
            ],
            "DEFAULT_CHANNELS order changed — `verdict_explanation` text and \
             integration test substring assertions will drift"
        );
    }

    #[test]
    fn from_invocations_errors_when_required_channel_absent() {
        // Defence in depth: if DEFAULT_CHANNELS and ImpactChannels
        // drift in a future refactor, the wire envelope would
        // silently lose a channel. The Err is the loud signal —
        // surfaces through the anyhow chain as a clean failure
        // instead of a panic.
        let invocations = vec![
            ChannelInvocation {
                name: "fst_refs",
                tier: ChannelTier::Text,
                result: ChannelResult::from_hits(vec![]),
            },
            ChannelInvocation {
                name: "grep_word_boundary",
                tier: ChannelTier::Text,
                result: ChannelResult::from_hits(vec![]),
            },
            ChannelInvocation {
                name: "call_graph_callers",
                tier: ChannelTier::Binder,
                result: ChannelResult::from_hits(vec![]),
            },
            ChannelInvocation {
                name: "transitive_callers",
                tier: ChannelTier::Binder,
                result: ChannelResult::from_hits(vec![]),
            },
        ];
        let err = ImpactChannels::from_invocations(invocations)
            .expect_err("missing `strict_refs` must surface as Err, not panic");
        let msg = format!("{err}");
        assert!(
            msg.contains("strict_refs"),
            "error must name the missing channel; got: {msg}"
        );
        assert!(
            msg.contains("DEFAULT_CHANNELS"),
            "error must point at the drift root cause; got: {msg}"
        );
    }
}
