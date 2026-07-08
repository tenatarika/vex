//! `vex bundle --mode symbol` assembler.
//!
//! Seeds an index lookup on `--symbol <NAME>`, then assembles a
//! bundle of related items (similar / callers / callees / tests) with
//! per-source signal annotations. Two response-shaping helpers live
//! here too: `signals_semantic` (HNSW-rank annotation) and
//! `probe_and_truncate` (capped probe with truncated bit).
//!
//! Isolated from `mod.rs` so the per-mode assembler doesn't share
//! screen space with the public types + dispatch. `extract_body` is
//! private to this file — only `assemble_symbol` slices a seed's
//! source for the bundle response.

use std::path::Path;

use anyhow::{Context, Result};

use crate::parse::language::Language;
use crate::protocol::{LexicalSignals, PostSignals, SemanticSignals, Signals, StructuralSignals};
use crate::search::SearchResult;

use super::{
    caller_kind, global_rank_percentile, signals_fst_hit, BundleArgs, BundleCoreItem, BundleCtx,
    BundleItem, BundleResponse, ModeSpecificMeta,
};

fn signals_semantic(rank_in_similar: u32) -> Signals {
    // NOTE (v1.20.0 D4): `semantic_cosine` is intentionally left as
    // `None` here even though `semantic_rank` is populated. The
    // bundle's `similar`-mode emit path already carries the raw
    // similarity score in the outer `BundleItem.similarity` field,
    // and threading it through `Signals` too would duplicate the
    // datum at two different keys without adding information. Search
    // (which has no equivalent outer field) IS the canonical home for
    // `semantic_cosine`. An agent inspecting bundle results should
    // read `item.similarity`, not `item.signals.semantic_cosine`.
    Signals::from_parts(
        StructuralSignals::default(),
        LexicalSignals::default(),
        SemanticSignals {
            semantic_rank: Some(rank_in_similar),
            semantic_cosine: None,
        },
        PostSignals::default(),
    )
}

/// Truncate a `probe` result (caller fetched with `cap = max + 1`) to
/// `max` items and return `(truncated_results, was_truncated)`. Lets the
/// caller report `*_truncated: true` only when the underlying source had
/// strictly more than `max` candidates — the `>=` check used previously
/// fired on exact-fit (max == N) cases too (review fix #1).
fn probe_and_truncate<T>(mut probe: Vec<T>, max: usize) -> (Vec<T>, bool) {
    let truncated = probe.len() > max;
    if truncated {
        probe.truncate(max);
    }
    (probe, truncated)
}

/// Symbol-mode assembler. Exposed `pub` so `benches/bundle.rs` and
/// out-of-crate integration tests can call it without going through the
/// CLI dispatch — the production entry point is still
/// `crate::cli::cmd_bundle::cmd_bundle`.
pub fn assemble_symbol(
    args: &BundleArgs<'_>,
    ctx: &BundleCtx<'_>,
) -> Result<(BundleResponse, ModeSpecificMeta)> {
    // `--mode symbol` requires `--symbol <name>`. Clap allows the flag to
    // be missing because it is `Option<String>` (other modes don't need
    // it); validate here so the error message points at the right flag
    // instead of clap's generic "required".
    let name = args
        .symbol
        .context("`vex bundle --mode symbol` requires `--symbol <NAME>`")?;

    let has_call_graph = ctx.reader.has_call_graph();
    let has_vectors = ctx.reader.has_vectors();

    // Step 1 — resolve the seed via FST. `search_with_fuzzy` returns up to
    // `limit` candidates; we only want the top one.
    let seeds = crate::search::structural::search_with_fuzzy(ctx.reader, name, 1);
    let Some(seed) = seeds.into_iter().next() else {
        return Ok((
            BundleResponse {
                mode: args.mode.as_str(),
                items: Vec::new(),
                mode_hints: Some(serde_json::json!({
                    "empty_reason": "symbol_not_found",
                    "has_call_graph": has_call_graph,
                    "has_vectors": has_vectors,
                })),
            },
            ModeSpecificMeta::default(),
        ));
    };

    // Step 2 — body extraction. Full body, no truncation (A7). Failures
    // here don't abort the bundle — the agent still benefits from the
    // callers / callees / similar blocks; mirror `fetch_symbol_body`'s
    // soft-degrade behaviour from `cli/mod.rs:355`. Resolve `seed.path`
    // against `ctx.root` rather than process cwd — paths in the index
    // are repo-relative, and assuming cwd == project root is a latent
    // bug surfaced by Inc 7 bench (review fix C1).
    let body = extract_body(&seed, &ctx.root);

    // Step 3 — callers. `find_callers_fast` returns an empty vec when
    // there's no call graph. Probe with `max + 1` so we can distinguish
    // "exactly N callers" from "N+, truncated" without inflating the
    // payload by more than one row (review fix #1: avoids the
    // `len() >= max` false-positive).
    let (callers, callers_truncated) = if has_call_graph {
        probe_and_truncate(
            crate::store::call_graph::find_callers_fast(
                ctx.reader,
                name,
                args.callers_max.saturating_add(1),
            ),
            args.callers_max,
        )
    } else {
        (Vec::new(), false)
    };

    // Step 4 — callees. Same probe-then-truncate pattern.
    let (callees, callees_truncated) = if has_call_graph {
        probe_and_truncate(
            crate::store::call_graph::find_callees_fast(
                ctx.reader,
                name,
                args.callees_max.saturating_add(1),
            ),
            args.callees_max,
        )
    } else {
        (Vec::new(), false)
    };

    // Step 5 — semantic similar. `find_similar` bails when the index has
    // no vectors, so guard explicitly and swallow the (separately
    // surfaced) error: `has_vectors == false` is a benign degraded mode,
    // not a failure. Same `+1` probe semantics.
    let (similar, similar_truncated) = if has_vectors {
        let probe = crate::search::similar::find_similar(
            ctx.reader,
            &ctx.hnsw_path,
            name,
            args.similar_max.saturating_add(1),
            0.0,
            ctx.vectors_normalized,
        )
        .unwrap_or_default();
        probe_and_truncate(probe, args.similar_max)
    } else {
        (Vec::new(), false)
    };

    // Step 6 — assemble the items list. Order matters: body first
    // (highest rank), then callers, callees, similar. The global
    // monotonic-descending `rank_percentile` is assigned post-hoc once
    // the total count is known.
    let mut items = Vec::with_capacity(1 + callers.len() + callees.len() + similar.len());

    items.push(BundleItem {
        core: BundleCoreItem {
            name: seed.name.clone(),
            kind: seed.kind.clone(),
            path: seed.path.clone(),
            line: seed.line,
            signature: seed.signature.clone(),
        },
        signals: signals_fst_hit(),
        rank_percentile: 0.0, // overwritten below
        role_rank: 0,
        role: "body",
        body,
        similarity: None,
    });

    for (i, cm) in callers.iter().enumerate() {
        items.push(BundleItem {
            core: BundleCoreItem {
                // Phase 14.1: synthetic Module callers carry kind="module".
                // `CallMatch` doesn't carry kind, so derive from the name
                // prefix — `<module:` is reserved for these synthetic rows.
                kind: caller_kind(&cm.name).to_string(),
                name: cm.name.clone(),
                path: cm.path.clone(),
                line: cm.line,
                signature: None,
            },
            signals: signals_fst_hit(),
            rank_percentile: 0.0,
            role_rank: i as u32,
            role: "caller",
            body: None,
            similarity: None,
        });
    }

    for (i, cm) in callees.iter().enumerate() {
        items.push(BundleItem {
            core: BundleCoreItem {
                name: cm.name.clone(),
                kind: "function".to_string(),
                path: cm.path.clone(),
                line: cm.line,
                signature: None,
            },
            signals: signals_fst_hit(),
            rank_percentile: 0.0,
            role_rank: i as u32,
            role: "callee",
            body: None,
            similarity: None,
        });
    }

    for (i, sm) in similar.iter().enumerate() {
        items.push(BundleItem {
            core: BundleCoreItem {
                name: sm.name.clone(),
                kind: sm.kind.clone(),
                path: sm.path.clone(),
                line: sm.line,
                signature: sm.signature.clone(),
            },
            signals: signals_semantic(i as u32),
            rank_percentile: 0.0,
            role_rank: i as u32,
            role: "similar",
            body: None,
            similarity: Some(sm.similarity),
        });
    }

    let total = items.len();
    for (i, item) in items.iter_mut().enumerate() {
        item.rank_percentile = global_rank_percentile(i, total);
    }

    let mode_hints = serde_json::json!({
        "callers_count": callers.len(),
        "callees_count": callees.len(),
        "similar_count": similar.len(),
        "callers_truncated": callers_truncated,
        "callees_truncated": callees_truncated,
        "similar_truncated": similar_truncated,
        "has_call_graph": has_call_graph,
        "has_vectors": has_vectors,
    });

    Ok((
        BundleResponse {
            mode: args.mode.as_str(),
            items,
            mode_hints: Some(mode_hints),
        },
        ModeSpecificMeta::default(),
    ))
}

/// Best-effort body extraction. Returns `None` when reading or parsing
/// fails — the seed identity is still surfaced to the agent, and a
/// stderr warning marks the degraded path so a regression (file deleted
/// under the index, parse failure) is visible.
///
/// `seed.path` is repo-relative as stored in the index; we join with
/// `root` so the extractor doesn't depend on process cwd. This is the
/// fix for the cwd-coupling that surfaced when the Inc 7 bench tried
/// to assemble a symbol from outside the project root.
fn extract_body(seed: &SearchResult, root: &Path) -> Option<String> {
    let abs_path = root.join(&seed.path);
    let content = match std::fs::read_to_string(&abs_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "warning: bundle could not read {}:{} for body extraction ({e}); \
                 body omitted from response",
                abs_path.display(),
                seed.line
            );
            return None;
        }
    };

    let ext = Path::new(&seed.path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let body = if seed.kind == "heading" {
        crate::parse::body::extract_heading_body(&content, seed.line, 0)
    } else if let Some(lang) = Language::from_extension(ext) {
        crate::parse::body::extract_symbol_body_ts(&content, seed.line, lang, 0)
    } else {
        crate::parse::body::extract_symbol_body(&content, seed.line, 0)
    };

    match body {
        Ok(b) => Some(b.body),
        Err(e) => {
            eprintln!(
                "warning: bundle could not extract body for {}:{} ({e}); \
                 body omitted from response",
                abs_path.display(),
                seed.line
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_semantic_sets_rank_and_clears_fst() {
        let s = signals_semantic(3);
        assert!(!s.fst_hit);
        assert_eq!(s.semantic_rank, Some(3));
    }
}
