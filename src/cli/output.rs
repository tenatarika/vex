use std::path::Path;

use serde::Serialize;

use crate::callgraph::bfs::{CallPath, ReachableMatch};
use crate::diff::SymbolChange;
use crate::protocol::{capabilities, MetaEnvelope, ResponseEnvelope, Signals, PROTOCOL_VERSION};
use crate::search::explain::Explanation;
use crate::search::similar::SimilarMatch;
use crate::search::SearchResult;

/// Generic Phase 13 envelope printer.
///
/// Used by every `--format json` subcommand except `search`, which has its
/// own path because it injects per-result `signals` + `rank_percentile` —
/// see [`print_search_envelope`]. Honors the `VEX_JSON_ENVELOPE=0` opt-out
/// for parity with `print_search_envelope`: when set, falls back to a bare
/// serialized `results` payload so pre-1.9 pipelines keep working.
///
/// Failures in `serde_json::to_string_pretty` collapse to an empty object;
/// the only way this fires is if a nested `Serialize` impl panics, which
/// none of ours do — but we still avoid `unwrap()` on the output path.
pub(crate) fn print_envelope<T: Serialize>(
    results: T,
    capabilities: crate::protocol::Capabilities,
    meta: MetaEnvelope,
) {
    if envelope_disabled_via_env() {
        let json = serde_json::to_string_pretty(&results).unwrap_or_else(|_| "{}".to_string());
        println!("{json}");
        return;
    }
    let envelope = ResponseEnvelope {
        protocol_version: PROTOCOL_VERSION,
        capabilities,
        meta,
        results,
    };
    let json = serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| "{}".to_string());
    println!("{json}");
}

/// Envelope payload for `vex search --format json`: each result carries the
/// original `SearchResult` shape (via `#[serde(flatten)]`) plus the Phase 13
/// `signals` block and a normalized `rank_percentile`.
#[derive(Serialize)]
struct SearchResultWithSignals<'a> {
    #[serde(flatten)]
    inner: &'a SearchResult,
    signals: Signals,
    rank_percentile: f32,
    /// v1.24.0 (PROTOCOL-EVOLUTION §4) — `"def"` when the query matched this
    /// symbol's name structurally (a definition of what was searched),
    /// `"neighbor"` when the row was surfaced only via lexical/semantic
    /// proximity (callers, imports, fuzzy). Gated by
    /// `capabilities.structured_result_kind`. `None` omits the key so the
    /// non-search / un-opted-in path stays byte-identical (§2a).
    #[serde(skip_serializing_if = "Option::is_none")]
    result_kind: Option<&'static str>,
}

/// Classify a result as a definition or a neighbour.
///
/// `"def"` requires an *exact / prefix* structural (FST) name match: the query
/// named this symbol. A `fst_hit` alone is not sufficient, because the
/// structural channel folds a Levenshtein *fuzzy* fallback into the same list
/// (`search::structural::search_with_fuzzy`) — a typo-corrected near-miss did
/// NOT match the name as typed and must read as a neighbour. Because that
/// fallback is all-or-nothing per query (`symbol_fst::search_with_fallback`
/// only reaches fuzzy when exact + prefix are both empty), a single
/// `structural_fuzzy` flag disqualifies the whole result set from `"def"`.
///
/// Everything else — rows surfaced only via the lexical (BM25) or semantic
/// channels (a caller, an import), and every row of a fuzzy-fallback query —
/// is a `"neighbor"`. This is the per-result form of the query-level drift
/// concept (see `cmd_search::SearchOutcome.drifted` /
/// `docs/PROTOCOL-EVOLUTION.md` §4).
fn classify_result_kind(sig: &Signals, structural_fuzzy: bool) -> &'static str {
    if sig.fst_hit && !structural_fuzzy {
        "def"
    } else {
        "neighbor"
    }
}

/// Build the response `_meta` block for a search call.
///
/// `index_age_ms`: derived from the manifest's `indexed_at` Unix timestamp;
/// clamped to `≥ 0` so a tiny clock skew can't underflow. `None` when the
/// manifest is missing or has no timestamp.
///
/// `ttl_ms`: 30s — a sensible freshness window for an index that the CLI
/// auto-updates between calls.
///
/// `cache_scope`: `"project"` — constant for now; reserved for future
/// workspace/global distinctions.
///
/// `traceparent`: always `None` on the CLI path. Only the MCP wrapper may
/// propagate one from an inbound JSON-RPC request's `_meta.traceparent`.
pub fn build_search_meta(manifest_path: &Path) -> MetaEnvelope {
    let index_age_ms = compute_index_age_ms(manifest_path);
    let stale_reason = super::stale_signal::current();
    let stale = stale_reason.as_ref().map(|_| true);
    MetaEnvelope {
        index_age_ms,
        traceparent: None,
        ttl_ms: Some(30_000),
        cache_scope: Some("project".into()),
        // Phase 13.7-D3 diff-filter observability: caller mutates this
        // field when a `--since*` / `--changed-only` flag was passed.
        diff_filter: None,
        stale,
        stale_reason,
        why_trace: None,
        history_mode: None,
        semantic_channel: None,
        search_hint: None,
        // §4.2 result-completeness keys default to None (absent = "unknown",
        // the safety default); producers that can compute them (usages) set
        // them explicitly at the emission site.
        ..Default::default()
    }
}

fn compute_index_age_ms(manifest_path: &Path) -> Option<u64> {
    use crate::index::manifest::Manifest;
    let manifest = Manifest::load(manifest_path).ok()?;
    let indexed_at_s = manifest.indexed_at?;
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let age_s = now_s.saturating_sub(indexed_at_s);
    Some(age_s.saturating_mul(1_000))
}

/// `_meta` block for non-search subcommands.
///
/// Mirrors `build_search_meta` minus the per-result `signals`/diff-filter
/// observability (those are search-only). Resolves the manifest from the
/// project root the same way the search path does, so `index_age_ms`,
/// `ttl_ms`, and `cache_scope` are populated for every JSON envelope —
/// not just `search` and `bundle`. `compute_index_age_ms` already returns
/// `None` on any failure (missing file, corrupt manifest, no `indexed_at`),
/// so a missing index naturally produces `index_age_ms: None` without an
/// extra `exists()` probe — which would also race a concurrent
/// `vex index` writing the manifest.
pub(crate) fn default_meta_for(root: &Path) -> MetaEnvelope {
    let manifest_path = crate::util::config::manifest_path(root);
    let stale_reason = super::stale_signal::current();
    let stale = stale_reason.as_ref().map(|_| true);
    MetaEnvelope {
        index_age_ms: compute_index_age_ms(&manifest_path),
        traceparent: None,
        ttl_ms: Some(30_000),
        cache_scope: Some("project".into()),
        diff_filter: None,
        stale,
        stale_reason,
        why_trace: None,
        history_mode: None,
        semantic_channel: None,
        search_hint: None,
        // §4.2 result-completeness keys default to None (absent = "unknown",
        // the safety default); producers that can compute them (usages) set
        // them explicitly at the emission site.
        ..Default::default()
    }
}

/// Returns true when `VEX_JSON_ENVELOPE` is set to a disable value
/// (`0` / `false` / `off`, case-insensitive). The default is to emit the
/// Phase 13 envelope; the opt-out exists so pre-1.9 scripts that pipe
/// `vex search --format json | jq '.[0].name'` keep working until v2.0.
fn envelope_disabled_via_env() -> bool {
    match std::env::var("VEX_JSON_ENVELOPE").ok().as_deref() {
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        ),
        None => false,
    }
}

/// Emit the Phase 13 response envelope for `vex search --format json`.
///
/// Per-result `rank_percentile` spans `[0.0, 1.0]` inclusive: the top result
/// sits at `1.0` and the bottom at `0.0`. For `N == 1` we emit `1.0` (a lone
/// result is the best result). With zero results we emit an empty payload —
/// callers still see the `protocol_version` and `_meta` block.
///
/// When `VEX_JSON_ENVELOPE=0` (or `false`/`off`), we fall back to the
/// pre-Phase-13 bare-array shape so existing pipelines keep working. This
/// opt-out is slated for removal in v2.0.
///
/// `structural_fuzzy` marks that the structural channel drifted to a
/// Levenshtein fuzzy fallback for this query (all-or-nothing per query), which
/// disqualifies every row from the `"def"` `result_kind` — see
/// [`classify_result_kind`].
pub fn print_search_envelope(
    results: &[SearchResult],
    signals: &[Signals],
    meta: MetaEnvelope,
    structural_fuzzy: bool,
) {
    if envelope_disabled_via_env() {
        let json = serde_json::to_string_pretty(results).unwrap_or_default();
        println!("{json}");
        return;
    }
    let total = results.len();
    let payload: Vec<SearchResultWithSignals<'_>> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let rank_percentile = if total <= 1 {
                1.0_f32
            } else {
                1.0_f32 - (i as f32 / (total - 1) as f32)
            };
            // signals length matches results length when build_signals is
            // called with the same merged slice — fall back to default so a
            // mismatch never panics in the output path.
            let sig = signals.get(i).cloned().unwrap_or_default();
            let result_kind = Some(classify_result_kind(&sig, structural_fuzzy));
            SearchResultWithSignals {
                inner: r,
                signals: sig,
                rank_percentile,
                result_kind,
            }
        })
        .collect();

    let envelope = ResponseEnvelope {
        protocol_version: PROTOCOL_VERSION,
        capabilities: capabilities::current(),
        meta,
        results: payload,
    };
    // serde_json::to_string_pretty on a serializable envelope cannot fail
    // unless one of the inner values has a custom Serialize that panics —
    // which none of ours do. Fall back to empty-object on the off-chance.
    let json = serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| "{}".to_string());
    println!("{json}");
}

pub fn print_results(results: &[SearchResult], format: &super::args::OutputFormat) {
    match format {
        super::args::OutputFormat::Json => {
            let json = serde_json::to_string_pretty(results).unwrap_or_default();
            println!("{json}");
        }
        super::args::OutputFormat::Text => {
            for r in results {
                println!(
                    "{kind:<12} {name:<40} {path}:{line}",
                    kind = r.kind,
                    name = r.name,
                    path = r.path,
                    line = r.line,
                );
                if let Some(sig) = &r.signature {
                    println!("             {sig}");
                }
            }
        }
        super::args::OutputFormat::Compact => {
            for r in results {
                let kind = compact_kind(&r.kind);
                print!(
                    "{kind} {name} {path}:{line}",
                    name = r.name,
                    path = r.path,
                    line = r.line
                );
                if let Some(sig) = &r.signature {
                    print!(" {sig}");
                }
                println!();
            }
        }
    }
}

pub fn print_similar(
    matches: &[SimilarMatch],
    target: &str,
    explanations: Option<&[Explanation]>,
    format: &super::args::OutputFormat,
    root: &Path,
) {
    match format {
        super::args::OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let mut obj = serde_json::json!({
                        "name": m.name,
                        "kind": m.kind,
                        "path": m.path,
                        "line": m.line,
                        "similarity": m.similarity,
                        "signature": m.signature,
                    });
                    if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                        obj["explanation"] = explanation_json(ex);
                    }
                    obj
                })
                .collect();
            print_envelope(&json, capabilities::current(), default_meta_for(root));
        }
        super::args::OutputFormat::Text => {
            if matches.is_empty() {
                println!("No similar symbols found for \"{target}\"");
                return;
            }
            println!("Similar to \"{target}\":");
            for (i, m) in matches.iter().enumerate() {
                println!(
                    " {sim:>5.3}  {kind:<10} {name:<40} {path}:{line}",
                    sim = m.similarity,
                    kind = m.kind,
                    name = m.name,
                    path = m.path,
                    line = m.line
                );
                if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                    print_explanation_text(ex);
                }
            }
        }
        super::args::OutputFormat::Compact => {
            for (i, m) in matches.iter().enumerate() {
                let kind = compact_kind(&m.kind);
                print!(
                    "{sim:.3} {kind} {name} {path}:{line}",
                    sim = m.similarity,
                    name = m.name,
                    path = m.path,
                    line = m.line
                );
                if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                    print_explanation_compact(ex);
                }
                println!();
            }
        }
    }
}

pub fn print_duplicates(
    pairs: &[(SimilarMatch, SimilarMatch)],
    explanations: Option<&[Explanation]>,
    format: &super::args::OutputFormat,
    root: &Path,
) {
    match format {
        super::args::OutputFormat::Json => {
            let json: Vec<serde_json::Value> = pairs
                .iter()
                .enumerate()
                .map(|(i, (a, b))| {
                    let mut obj = serde_json::json!({
                        "similarity": a.similarity,
                        "a": {
                            "name": a.name,
                            "kind": a.kind,
                            "path": a.path,
                            "line": a.line,
                        },
                        "b": {
                            "name": b.name,
                            "kind": b.kind,
                            "path": b.path,
                            "line": b.line,
                        },
                    });
                    if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                        obj["explanation"] = explanation_json(ex);
                    }
                    obj
                })
                .collect();
            print_envelope(&json, capabilities::current(), default_meta_for(root));
        }
        super::args::OutputFormat::Text => {
            if pairs.is_empty() {
                println!("No duplicates found");
                return;
            }
            for (i, (a, b)) in pairs.iter().enumerate() {
                println!(
                    " {sim:>5.3}  {name}  {path}:{line}",
                    sim = a.similarity,
                    name = a.name,
                    path = a.path,
                    line = a.line
                );
                println!(
                    "         {name}  {path}:{line}",
                    name = b.name,
                    path = b.path,
                    line = b.line
                );
                if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                    print_explanation_text(ex);
                }
            }
        }
        super::args::OutputFormat::Compact => {
            for (i, (a, b)) in pairs.iter().enumerate() {
                let ak = compact_kind(&a.kind);
                let bk = compact_kind(&b.kind);
                print!(
                    "{sim:.3} {ak} {an} {ap}:{al} | {bk} {bn} {bp}:{bl}",
                    sim = a.similarity,
                    an = a.name,
                    ap = a.path,
                    al = a.line,
                    bn = b.name,
                    bp = b.path,
                    bl = b.line
                );
                if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                    print_explanation_compact(ex);
                }
                println!();
            }
        }
    }
}

fn explanation_json(ex: &Explanation) -> serde_json::Value {
    serde_json::json!({
        "identifier_jaccard": ex.identifier_jaccard,
        "diff_added": ex.added,
        "diff_removed": ex.removed,
        "diff": ex.diff,
    })
}

fn print_explanation_text(ex: &Explanation) {
    println!(
        "         jaccard {jac:.2}  diff +{add} -{rem}",
        jac = ex.identifier_jaccard,
        add = ex.added,
        rem = ex.removed,
    );
    for line in ex.diff.lines() {
        println!("           {line}");
    }
}

pub fn print_diff(
    changes: &[SymbolChange],
    base: &str,
    format: &super::args::OutputFormat,
    root: &Path,
) {
    match format {
        super::args::OutputFormat::Json => {
            print_envelope(changes, capabilities::current(), default_meta_for(root));
        }
        super::args::OutputFormat::Text => {
            if changes.is_empty() {
                println!("No symbol-level changes between `{base}` and working tree");
                return;
            }
            // Group by path for readability.
            let mut by_path: std::collections::BTreeMap<&str, Vec<&SymbolChange>> =
                std::collections::BTreeMap::new();
            for c in changes {
                by_path.entry(c.path.as_str()).or_default().push(c);
            }
            println!("{} symbol change(s) vs `{base}`:", changes.len());
            for (path, group) in by_path {
                println!("  {path}");
                for c in group {
                    let extra = match c.old_line {
                        Some(old) => format!("  (was line {old})"),
                        None => String::new(),
                    };
                    println!(
                        "    {kind:<12}  {sym_kind:<10} {name}:{line}{extra}",
                        kind = c.kind.as_str(),
                        sym_kind = c.symbol_kind,
                        name = c.name,
                        line = c.line,
                    );
                }
            }
        }
        super::args::OutputFormat::Compact => {
            for c in changes {
                let suffix = match c.old_line {
                    Some(old) => format!(" was:{old}"),
                    None => String::new(),
                };
                println!(
                    "{kind} {sym_kind} {name} {path}:{line}{suffix}",
                    kind = c.kind.as_str(),
                    sym_kind = c.symbol_kind,
                    name = c.name,
                    path = c.path,
                    line = c.line,
                );
            }
        }
    }
}

pub fn print_paths(
    paths: &[CallPath],
    from: &str,
    to: &str,
    format: &super::args::OutputFormat,
    root: &Path,
) {
    match format {
        super::args::OutputFormat::Json => {
            let json: Vec<serde_json::Value> = paths
                .iter()
                .map(|p| {
                    let steps: Vec<serde_json::Value> = p
                        .steps
                        .iter()
                        .map(|s| {
                            serde_json::json!({
                                "name": s.name,
                                "path": s.path,
                                "line": s.line,
                            })
                        })
                        .collect();
                    serde_json::json!({ "steps": steps })
                })
                .collect();
            print_envelope(&json, capabilities::current(), default_meta_for(root));
        }
        super::args::OutputFormat::Text => {
            if paths.is_empty() {
                println!("No path from \"{from}\" to \"{to}\"");
                return;
            }
            println!("{} path(s) from \"{from}\" to \"{to}\":", paths.len());
            for (i, p) in paths.iter().enumerate() {
                println!(" #{}:", i + 1);
                for (j, step) in p.steps.iter().enumerate() {
                    let arrow = if j + 1 < p.steps.len() { "→" } else { " " };
                    let site = if step.line > 0 && !step.path.is_empty() {
                        format!("  ({}:{})", step.path, step.line)
                    } else {
                        String::new()
                    };
                    println!("    {arrow} {name}{site}", name = step.name);
                }
            }
        }
        super::args::OutputFormat::Compact => {
            for p in paths {
                let parts: Vec<String> = p
                    .steps
                    .iter()
                    .map(|s| {
                        if s.line > 0 && !s.path.is_empty() {
                            format!("{}:{}", s.name, s.line)
                        } else {
                            s.name.clone()
                        }
                    })
                    .collect();
                println!("{}", parts.join(" -> "));
            }
        }
    }
}

/// One row in the `vex tests-for` result set: same shape as
/// `ReachableMatch` plus a `framework` label inferred from the path.
#[derive(Debug, Clone)]
pub struct TestsForRow {
    pub name: String,
    pub path: String,
    pub line: usize,
    pub depth: usize,
    pub framework: &'static str,
}

pub fn print_tests_for(
    rows: &[TestsForRow],
    target: &str,
    format: &super::args::OutputFormat,
    root: &Path,
) {
    match format {
        super::args::OutputFormat::Json => {
            let json: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r.name,
                        "path": r.path,
                        "line": r.line,
                        "depth": r.depth,
                        "framework": r.framework,
                    })
                })
                .collect();
            print_envelope(&json, capabilities::current(), default_meta_for(root));
        }
        super::args::OutputFormat::Text => {
            if rows.is_empty() {
                println!("No tests reach \"{target}\"");
                return;
            }
            println!("{} test(s) reach \"{target}\":", rows.len());
            for r in rows {
                println!(
                    "  [{depth}]  {name:<40} ({framework})  {path}:{line}",
                    depth = r.depth,
                    name = r.name,
                    framework = r.framework,
                    path = r.path,
                    line = r.line
                );
            }
        }
        super::args::OutputFormat::Compact => {
            for r in rows {
                println!(
                    "{depth} {framework} {name} {path}:{line}",
                    depth = r.depth,
                    framework = r.framework,
                    name = r.name,
                    path = r.path,
                    line = r.line
                );
            }
        }
    }
}

pub fn print_reachable(
    matches: &[ReachableMatch],
    target: &str,
    format: &super::args::OutputFormat,
    root: &Path,
) {
    match format {
        super::args::OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "path": m.path,
                        "line": m.line,
                        "depth": m.depth,
                    })
                })
                .collect();
            print_envelope(&json, capabilities::current(), default_meta_for(root));
        }
        super::args::OutputFormat::Text => {
            if matches.is_empty() {
                println!("Nothing reaches \"{target}\"");
                return;
            }
            println!("{} symbol(s) reach \"{target}\":", matches.len());
            for m in matches {
                println!(
                    "  [{depth}]  {name:<40} {path}:{line}",
                    depth = m.depth,
                    name = m.name,
                    path = m.path,
                    line = m.line
                );
            }
        }
        super::args::OutputFormat::Compact => {
            for m in matches {
                println!(
                    "{depth} {name} {path}:{line}",
                    depth = m.depth,
                    name = m.name,
                    path = m.path,
                    line = m.line
                );
            }
        }
    }
}

fn print_explanation_compact(ex: &Explanation) {
    // Compact format already uses ` | ` as the a/b separator in
    // `print_duplicates`; using `;` here keeps `--explain --format compact`
    // unambiguous for ` | `-splitting consumers.
    print!(
        " ; jac {jac:.2} +{add} -{rem}",
        jac = ex.identifier_jaccard,
        add = ex.added,
        rem = ex.removed,
    );
}

/// Single-char kind code for compact output.
fn compact_kind(kind: &str) -> char {
    match kind {
        "function" => 'F',
        "method" => 'M',
        "struct" => 'S',
        "class" => 'C',
        "interface" => 'I',
        "trait" => 'T',
        "enum" => 'E',
        "type_alias" => 'A',
        "impl" => 'i',
        "constant" => 'K',
        "property" => 'P',
        "package" => 'G',
        "heading" => 'H',
        _ => '?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::MatchType;

    fn sample_result() -> SearchResult {
        SearchResult {
            name: "alpha_handler".into(),
            kind: "function".into(),
            path: "src/a.rs".into(),
            line: 1,
            signature: None,
            score: 0.9,
            match_type: MatchType::Structural,
        }
    }

    #[test]
    fn classify_result_kind_maps_exact_fst_hit_to_def_else_neighbor() {
        let hit = Signals {
            fst_hit: true,
            ..Signals::default()
        };
        // Exact/prefix structural name match (not a fuzzy fallback) → def.
        assert_eq!(classify_result_kind(&hit, false), "def");

        // Surfaced only via a proximity channel (BM25 rank present, no FST
        // hit) → neighbour.
        let bm25_only = Signals {
            fst_hit: false,
            bm25_rank: Some(0),
            ..Signals::default()
        };
        assert_eq!(classify_result_kind(&bm25_only, false), "neighbor");

        // A bare default (no channel signal) still classifies as neighbour,
        // never panics.
        assert_eq!(classify_result_kind(&Signals::default(), false), "neighbor");
    }

    /// Regression: a fuzzy-fallback query (`structural_fuzzy == true`) produces
    /// `fst_hit == true` rows that did NOT match the name as typed — they must
    /// read as `"neighbor"`, not `"def"`. Caught in code review before merge.
    #[test]
    fn classify_result_kind_fuzzy_fallback_hit_is_neighbor_not_def() {
        let fuzzy_hit = Signals {
            fst_hit: true,
            ..Signals::default()
        };
        assert_eq!(
            classify_result_kind(&fuzzy_hit, true),
            "neighbor",
            "a fuzzy-fallback structural hit is a typo-corrected near-miss, not a def"
        );
    }

    /// §2a un-opted-in byte-identity guard: a `SearchResultWithSignals` whose
    /// `result_kind` is `None` MUST serialize *without* the key, so the
    /// non-search / pre-feature path stays byte-identical. This cannot be
    /// satisfied by editing a golden — the key must genuinely be absent.
    #[test]
    fn result_kind_none_omits_the_wire_key() {
        let inner = sample_result();
        let row = SearchResultWithSignals {
            inner: &inner,
            signals: Signals::default(),
            rank_percentile: 1.0,
            result_kind: None,
        };
        let v = serde_json::to_value(&row).unwrap();
        assert!(
            v.get("result_kind").is_none(),
            "result_kind: None must omit the key entirely; got: {v}"
        );
    }

    #[test]
    fn result_kind_some_emits_the_wire_key() {
        let inner = sample_result();
        let row = SearchResultWithSignals {
            inner: &inner,
            signals: Signals::default(),
            rank_percentile: 1.0,
            result_kind: Some("def"),
        };
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(
            v.get("result_kind").and_then(|k| k.as_str()),
            Some("def"),
            "result_kind: Some must emit the string key; got: {v}"
        );
    }
}
