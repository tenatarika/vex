//! `vex impact <Symbol>` — one-call blast-radius / delete-safety
//! report. v1.20.0 (F1).
//!
//! Composes four independent reference channels into a single
//! envelope so an agent can answer "is it safe to delete this?"
//! without hand-assembling the four sub-commands (strict usages,
//! non-strict FST refs, `grep \bName\b`, call-graph callers) and
//! cross-checking their disagreement.
//!
//! Why four channels? Each one misses something:
//!
//! - **Strict refs** (v5 `reference_edges`) — scope-binder confirms
//!   real cross-file references; supports Rust / TypeScript /
//!   Python / C# / C++. Drops to zero matches when the index lacks
//!   the section (pre-v1.8) or the symbol's language has no binder.
//! - **FST refs** — every CamelCase / snake_case occurrence of the
//!   name from the AST identifier nodes. Catches everything strict
//!   does plus matches in languages without a binder, at the cost
//!   of false positives (string literals, doc-comments).
//! - **grep `\b<Name>\b`** — pure text scan. Catches matches inside
//!   string-literal call sites (e.g. dynamic dispatch via
//!   `getattr(obj, "Name")`), macros, and configuration files —
//!   anything the AST-walking pipelines skip.
//! - **Call-graph callers** — confirms a real call edge (not just a
//!   reference). Misses decorator-bound, reflection-resolved, and
//!   string-dispatched call sites that strict / FST surface as
//!   references.
//!
//! The verdict joins these into one boolean-ish answer:
//!
//! - **safe**     — every channel reports zero hits (excluding the
//!   symbol's own definition line). Delete is highly likely safe.
//! - **unsafe**   — `strict_refs` OR `call_graph_callers` reports
//!   `> 0`. Binder/graph confirmed real usage; do not delete.
//! - **uncertain** — strict + callers report 0 but FST or grep
//!   surfaced something. Could be a string-literal mention, comment,
//!   or dynamic dispatch the binder doesn't see; manual inspection
//!   required.

use anyhow::{Context, Result};
use serde::Serialize;

use super::args::{OutputFormat, ScopeArgs};
use super::common::{resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::scope;
use crate::protocol::capabilities;
use crate::store::reader::IndexReader;
use crate::util::paths::to_rel_posix;

/// Cap on rows displayed per channel. The verdict logic only needs
/// "≥1" / "0" granularity, but the JSON envelope surfaces a sample
/// so the agent can see *where* the hits are.
const SAMPLE_LIMIT: usize = 10;

/// Hard cap on the grep channel — `\b<Name>\b` against the whole
/// project should never produce thousands of hits for a real
/// symbol name, but the regex scans every file so a bound matters.
const GREP_HARD_CAP: usize = 500;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ImpactVerdict {
    Safe,
    Unsafe,
    Uncertain,
}

#[derive(Debug, Serialize)]
struct HitLocation {
    path: String,
    line: u32,
}

#[derive(Debug, Serialize)]
struct ChannelResult {
    /// Whether the channel could run. `false` when the index lacks
    /// the requisite section (e.g. `strict_refs` against a v4 index
    /// pre-Phase 11.1).
    available: bool,
    /// Human-readable reason when `available == false`. `None`
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<String>,
    /// Total post-filter hit count for this channel.
    count: usize,
    /// First `SAMPLE_LIMIT` hits, for the agent to inspect without
    /// chasing per-channel commands. Truncation is reported via the
    /// `truncated` field.
    sample: Vec<HitLocation>,
    /// `true` when `count > sample.len()` — the rest is omitted.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

impl ChannelResult {
    fn unavailable(reason: &str) -> Self {
        Self {
            available: false,
            unavailable_reason: Some(reason.to_string()),
            count: 0,
            sample: Vec::new(),
            truncated: false,
        }
    }

    fn from_hits(mut hits: Vec<HitLocation>) -> Self {
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

#[derive(Debug, Serialize)]
struct ImpactChannels {
    strict_refs: ChannelResult,
    fst_refs: ChannelResult,
    grep_word_boundary: ChannelResult,
    call_graph_callers: ChannelResult,
}

#[derive(Debug, Serialize)]
struct ImpactReport {
    symbol: String,
    verdict: ImpactVerdict,
    /// One-line human-readable derivation of the verdict so an
    /// agent can quote it back to the user without re-running the
    /// rule. Mirrors the doc-comment table at the top of this file.
    verdict_explanation: String,
    channels: ImpactChannels,
}

/// Decide the verdict from the four channel counts. Only counts
/// that came from `available` channels participate — an
/// unavailable strict-refs channel doesn't drag the verdict to
/// "safe" just because its count is zero.
///
/// The `verdict_explanation` enumerates which channels actually ran
/// vs. were unavailable so an agent can read the conclusion without
/// a false-positive "all four reported zero" claim when (e.g.) the
/// index pre-dates v1.8 and the binder channel never ran.
fn derive_verdict(channels: &ImpactChannels) -> (ImpactVerdict, String) {
    let strict_confirms = channels.strict_refs.available && channels.strict_refs.count > 0;
    let callers_confirm =
        channels.call_graph_callers.available && channels.call_graph_callers.count > 0;
    let fst_text = channels.fst_refs.available && channels.fst_refs.count > 0;
    let grep_text = channels.grep_word_boundary.count > 0; // grep is always available

    let any_binder_available =
        channels.strict_refs.available || channels.call_graph_callers.available;
    let mut unavailable_channels = Vec::new();
    if !channels.strict_refs.available {
        unavailable_channels.push("strict_refs");
    }
    if !channels.fst_refs.available {
        unavailable_channels.push("fst_refs");
    }
    if !channels.call_graph_callers.available {
        unavailable_channels.push("call_graph_callers");
    }
    let unavailable_note = if unavailable_channels.is_empty() {
        String::new()
    } else {
        format!(
            " (unavailable: {} — re-run `vex index` for stronger evidence)",
            unavailable_channels.join(", ")
        )
    };

    if strict_confirms || callers_confirm {
        let mut reasons = Vec::new();
        if strict_confirms {
            reasons.push(format!("strict_refs={}", channels.strict_refs.count));
        }
        if callers_confirm {
            reasons.push(format!(
                "call_graph_callers={}",
                channels.call_graph_callers.count
            ));
        }
        return (
            ImpactVerdict::Unsafe,
            format!(
                "binder/graph confirmed real usage ({}). Do not delete without rewriting call sites.",
                reasons.join(", ")
            ),
        );
    }

    if fst_text || grep_text {
        let mut reasons = Vec::new();
        if fst_text {
            reasons.push(format!("fst_refs={}", channels.fst_refs.count));
        }
        if grep_text {
            reasons.push(format!(
                "grep_word_boundary={}",
                channels.grep_word_boundary.count
            ));
        }
        return (
            ImpactVerdict::Uncertain,
            format!(
                "text-only matches surfaced ({}) but binder/call-graph saw none{unavailable_note}. \
                 Likely string-literal mentions, comments, or dynamic dispatch — manual inspection required.",
                reasons.join(", "),
            ),
        );
    }

    // Every channel that ran reported zero hits. If no binder channel
    // ran at all, "safe" is unjustifiably optimistic — downgrade to
    // uncertain with an honest reason.
    if !any_binder_available {
        return (
            ImpactVerdict::Uncertain,
            "text channels reported zero hits, but neither strict_refs nor call_graph_callers \
             ran on this index (pre-v1.8 / pre-Phase 10.2, or no binder coverage for this \
             project's languages). Re-run `vex index` to rebuild before relying on a verdict."
                .to_string(),
        );
    }

    let mut available_channels = Vec::new();
    if channels.strict_refs.available {
        available_channels.push("strict_refs");
    }
    if channels.fst_refs.available {
        available_channels.push("fst_refs");
    }
    available_channels.push("grep_word_boundary"); // always runs
    if channels.call_graph_callers.available {
        available_channels.push("call_graph_callers");
    }
    (
        ImpactVerdict::Safe,
        format!(
            "{} of 4 channels reported zero hits ({}){unavailable_note}. \
             Delete is highly likely safe.",
            available_channels.len(),
            available_channels.join(", "),
        ),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn impact(
    ctx: &CmdCtx<'_>,
    name: String,
    path: Option<std::path::PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
    scope: ScopeArgs,
) -> Result<()> {
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

    // Build the def-site set up front. Used to strip the symbol's own
    // declaration row from fst_refs and grep_word_boundary so neither
    // channel false-positives the "is this in use?" question.
    let def_sites: std::collections::HashSet<(String, u32)> = {
        let mut set = std::collections::HashSet::new();
        if let Some(sym_fst) = reader.symbol_fst_reader() {
            for sym_idx in sym_fst.find(&name) {
                if let Some(sym) = reader.symbol(sym_idx as usize) {
                    let file_path = reader.read_string(sym.file_offset).to_string();
                    set.insert((file_path, sym.line));
                }
            }
        }
        set
    };

    // ── Channel 1: strict refs (v5 reference_edges) ───────────────
    let strict_refs = if reader.has_ref_edges() {
        let sym_fst = reader
            .symbol_fst_reader()
            .context("symbol FST missing — re-run `vex index` to rebuild")?;
        let mut hits = Vec::new();
        for sym_idx in sym_fst.find(&name) {
            for edge in reader.find_ref_edges_by_symbol(sym_idx) {
                let path = file_paths
                    .get(edge.from_file_id as usize)
                    .cloned()
                    .unwrap_or_else(|| "?".to_string());
                if !path_scope.accept(&path) {
                    continue;
                }
                hits.push(HitLocation {
                    path,
                    line: edge.line,
                });
            }
        }
        ChannelResult::from_hits(hits)
    } else {
        ChannelResult::unavailable(
            "index has no v5 reference_edges section (pre-v1.8 index, or no binder coverage for this project's languages) — re-run `vex index` to rebuild",
        )
    };

    // ── Channel 2: FST refs (legacy refs FST) ─────────────────────
    let fst_refs = match reader.ref_reader() {
        Some(ref_reader) => {
            let hits: Vec<HitLocation> = ref_reader
                .find(&name)
                .into_iter()
                .filter_map(|e| {
                    let path = file_paths.get(e.file_id as usize).cloned()?;
                    // Apply the same D2 def-site filter — the symbol's
                    // own declaration row is never a "use" for impact.
                    if def_sites.contains(&(path.clone(), e.line)) {
                        return None;
                    }
                    if !path_scope.accept(&path) {
                        return None;
                    }
                    Some(HitLocation { path, line: e.line })
                })
                .collect();
            ChannelResult::from_hits(hits)
        }
        None => ChannelResult::unavailable("index has no refs FST — re-run `vex index`"),
    };

    // ── Channel 3: grep \b<Name>\b across project ─────────────────
    let escaped = regex::escape(&name);
    let pattern = format!(r"\b{escaped}\b");
    let grep_hits_raw = crate::grep::search(&root, &pattern, None, GREP_HARD_CAP, ctx.excludes)
        .context("grep word-boundary scan")?;
    let grep_word_boundary = {
        let hits: Vec<HitLocation> = grep_hits_raw
            .into_iter()
            .filter_map(|m| {
                // Normalize grep's native-separator relative path to
                // POSIX so the def-site HashSet (built from
                // index-stored POSIX paths) hits on Windows too.
                let posix = to_rel_posix(&root.join(&m.path), &root).unwrap_or(m.path.clone());
                let line = u32::try_from(m.line).ok()?;
                if def_sites.contains(&(posix.clone(), line)) {
                    return None;
                }
                if !path_scope.accept(&posix) {
                    return None;
                }
                Some(HitLocation { path: posix, line })
            })
            .collect();
        ChannelResult::from_hits(hits)
    };

    // ── Channel 4: call-graph direct callers ──────────────────────
    let call_graph_callers = if reader.has_call_graph() {
        let callers = crate::store::call_graph::find_callers_fast(
            &reader,
            &name,
            crate::callgraph::CALLERS_FETCH_CAP,
        );
        let hits: Vec<HitLocation> = callers
            .into_iter()
            .filter_map(|m| {
                if !path_scope.accept(&m.path) {
                    return None;
                }
                let line = u32::try_from(m.line).ok()?;
                Some(HitLocation { path: m.path, line })
            })
            .collect();
        ChannelResult::from_hits(hits)
    } else {
        ChannelResult::unavailable(
            "index has no v4 call graph section (pre-Phase 10.2 index, or empty call-edges) — re-run `vex index`",
        )
    };

    let channels = ImpactChannels {
        strict_refs,
        fst_refs,
        grep_word_boundary,
        call_graph_callers,
    };
    let (verdict, verdict_explanation) = derive_verdict(&channels);
    let report = ImpactReport {
        symbol: name.clone(),
        verdict,
        verdict_explanation,
        channels,
    };

    // Verdict alone never triggers `signal_no_results` — even "safe"
    // is a meaningful answer that the exit code should reflect as
    // success (0). Pin this contract here so the CI / scripts that
    // gate on `vex impact ... && delete` interpret the verdict by
    // reading the envelope, not the exit code.

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
                    ImpactVerdict::Safe => "safe",
                    ImpactVerdict::Unsafe => "unsafe",
                    ImpactVerdict::Uncertain => "uncertain",
                },
                report.verdict_explanation
            );
            print_channel("strict_refs", &report.channels.strict_refs);
            print_channel("fst_refs", &report.channels.fst_refs);
            print_channel("grep_word_boundary", &report.channels.grep_word_boundary);
            print_channel("call_graph_callers", &report.channels.call_graph_callers);
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

    fn channel_with(count: usize, available: bool) -> ChannelResult {
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

    #[test]
    fn verdict_safe_when_all_channels_zero() {
        let channels = ImpactChannels {
            strict_refs: channel_with(0, true),
            fst_refs: channel_with(0, true),
            grep_word_boundary: channel_with(0, true),
            call_graph_callers: channel_with(0, true),
        };
        let (v, why) = derive_verdict(&channels);
        assert!(matches!(v, ImpactVerdict::Safe), "got: {v:?}");
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
        // Safe is justified (strict_refs confirmed) but the explanation
        // must enumerate which channels were unavailable so the agent
        // doesn't quote a false "all four channels reported zero".
        let channels = ImpactChannels {
            strict_refs: channel_with(0, true),
            fst_refs: channel_with(0, false),
            grep_word_boundary: channel_with(0, true),
            call_graph_callers: channel_with(0, false),
        };
        let (v, why) = derive_verdict(&channels);
        assert!(matches!(v, ImpactVerdict::Safe), "got: {v:?}");
        assert!(
            why.contains("unavailable: fst_refs, call_graph_callers"),
            "explanation must enumerate unavailable channels, got: {why}"
        );
        assert!(
            why.contains("2 of 4 channels"),
            "explanation must count only channels that ran, got: {why}"
        );
    }

    #[test]
    fn verdict_uncertain_when_all_binder_channels_unavailable_even_with_grep_zero() {
        // Pre-v1.8 index with no binder, no call graph, no FST refs —
        // only grep ran and saw nothing. "Safe" would be unjustifiably
        // optimistic; verdict must downgrade to uncertain with an
        // honest re-index hint.
        let channels = ImpactChannels {
            strict_refs: channel_with(0, false),
            fst_refs: channel_with(0, false),
            grep_word_boundary: channel_with(0, true),
            call_graph_callers: channel_with(0, false),
        };
        let (v, why) = derive_verdict(&channels);
        assert!(
            matches!(v, ImpactVerdict::Uncertain),
            "all binders unavailable + grep=0 must NOT be safe; got: {v:?}"
        );
        assert!(
            why.contains("Re-run `vex index`"),
            "explanation must point at re-indexing, got: {why}"
        );
    }

    #[test]
    fn verdict_uncertain_explanation_notes_strict_unavailability() {
        // strict unavailable + text channel hit. The verdict enum value
        // is the same as the strict-available case but the explanation
        // must surface the unavailability so the agent doesn't read
        // identical text for two meaningfully different states.
        let channels = ImpactChannels {
            strict_refs: channel_with(0, false),
            fst_refs: channel_with(0, true),
            grep_word_boundary: channel_with(1, true),
            call_graph_callers: channel_with(0, true),
        };
        let (v, why) = derive_verdict(&channels);
        assert!(matches!(v, ImpactVerdict::Uncertain), "got: {v:?}");
        assert!(
            why.contains("unavailable: strict_refs"),
            "uncertain explanation must mention which binder channel didn't run, got: {why}"
        );
    }

    #[test]
    fn verdict_unsafe_when_strict_confirms() {
        let channels = ImpactChannels {
            strict_refs: channel_with(3, true),
            fst_refs: channel_with(5, true),
            grep_word_boundary: channel_with(7, true),
            call_graph_callers: channel_with(0, true),
        };
        let (v, why) = derive_verdict(&channels);
        assert!(matches!(v, ImpactVerdict::Unsafe), "got: {v:?}");
        assert!(
            why.contains("strict_refs=3"),
            "explanation must cite strict count, got: {why}"
        );
    }

    #[test]
    fn verdict_unsafe_when_callers_confirm_without_strict() {
        // Reflection / decorator path: binder doesn't index the ref
        // but the call-graph extractor sees the call. Must be unsafe.
        let channels = ImpactChannels {
            strict_refs: channel_with(0, true),
            fst_refs: channel_with(0, true),
            grep_word_boundary: channel_with(0, true),
            call_graph_callers: channel_with(2, true),
        };
        let (v, why) = derive_verdict(&channels);
        assert!(matches!(v, ImpactVerdict::Unsafe), "got: {v:?}");
        assert!(
            why.contains("call_graph_callers=2"),
            "explanation must cite callers count, got: {why}"
        );
    }

    #[test]
    fn verdict_uncertain_when_only_text_channels_hit() {
        // String-literal / comment / config-file mention: FST and
        // grep see it but binder + call graph don't. Uncertain —
        // manual inspection required.
        let channels = ImpactChannels {
            strict_refs: channel_with(0, true),
            fst_refs: channel_with(2, true),
            grep_word_boundary: channel_with(3, true),
            call_graph_callers: channel_with(0, true),
        };
        let (v, why) = derive_verdict(&channels);
        assert!(matches!(v, ImpactVerdict::Uncertain), "got: {v:?}");
        assert!(
            why.contains("manual inspection"),
            "explanation must mention manual review, got: {why}"
        );
    }

    #[test]
    fn unavailable_strict_channel_does_not_force_safe_on_grep_hit() {
        // If strict is unavailable (pre-v1.8 index) and grep sees a
        // hit, the verdict must NOT be "safe" — strict's zero is
        // informationless. Should be uncertain.
        let channels = ImpactChannels {
            strict_refs: channel_with(0, false),
            fst_refs: channel_with(0, false),
            grep_word_boundary: channel_with(1, true),
            call_graph_callers: channel_with(0, false),
        };
        let (v, _) = derive_verdict(&channels);
        assert!(
            matches!(v, ImpactVerdict::Uncertain),
            "unavailable strict + grep hit must be uncertain, got: {v:?}"
        );
    }
}
