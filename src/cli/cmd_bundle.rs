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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::args::ScopeArgs;
use crate::cli::output::print_envelope;
use crate::parse::language::Language;
use crate::protocol::{capabilities, MetaEnvelope, Signals};
use crate::search::SearchResult;
use crate::store::reader::IndexReader;

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
        BundleModeFlag::PrImpact => assemble_pr_impact(&args, &ctx)?,
        BundleModeFlag::Project => assemble_project(&args, &ctx)?,
    };

    let meta = MetaEnvelope {
        diff_filter: mode_meta.diff_filter,
        ..MetaEnvelope::default()
    };

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

// ---------------------------------------------------------------------------
// `--mode symbol` (Inc 2)
// ---------------------------------------------------------------------------

/// Default rank-percentile semantics: `1.0` for the top result, `0.0` for
/// the bottom, linear in between. Matches `print_search_envelope` in
/// `cli/output.rs` so callers can compare bundles to search envelopes
/// without translating ranks (architect-review A6 — preserves the
/// `search_envelope_rank_percentile_monotonic_descending` invariant).
fn global_rank_percentile(idx: usize, total: usize) -> f32 {
    if total <= 1 {
        1.0
    } else {
        1.0 - (idx as f32 / (total - 1) as f32)
    }
}

fn signals_fst_hit() -> Signals {
    Signals {
        fst_hit: true,
        ..Signals::default()
    }
}

/// Phase 14.1 — `CallMatch` doesn't carry `SymbolKind`, so derive the
/// bundled `kind` field from the caller name. Synthetic per-file Module
/// symbols are named `<module:path>` by `parse::parse_file`; everything
/// else is a real callable (fn / method / closure).
fn caller_kind(name: &str) -> &'static str {
    if name.starts_with("<module:") {
        "module"
    } else {
        "function"
    }
}

fn signals_semantic(rank_in_similar: u32) -> Signals {
    Signals {
        fst_hit: false,
        semantic_rank: Some(rank_in_similar),
        ..Signals::default()
    }
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
/// [`cmd_bundle`].
#[doc(hidden)]
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

// ---------------------------------------------------------------------------
// `--mode pr-impact` (Inc 3)
// ---------------------------------------------------------------------------

/// Substrings that mark a file as a test home. Inline per architect-
/// review A8 — no extraction into `util/heuristics` until 13.10 lands a
/// concrete second caller.
fn is_test_path(path: &str) -> bool {
    const TEST_PATH_MARKERS: &[&str] = &[
        "/tests/",
        "/test/",
        "_test.",
        ".test.",
        "/spec/",
        "/__tests__/",
    ];
    TEST_PATH_MARKERS.iter().any(|m| path.contains(m))
}

/// Heuristic test-attribute scan over the captured signature. Catches
/// the common Rust / unit-test annotations that put a test function
/// outside a conventional `tests/` directory. Conservative: only
/// recognises three patterns to keep false positives low.
fn signature_marks_test(signature: Option<&str>) -> bool {
    match signature {
        Some(sig) => {
            let trimmed = sig.trim_start();
            trimmed.starts_with("#[test]")
                || trimmed.starts_with("#[tokio::test")
                || trimmed.starts_with("#[cfg(test)]")
        }
        None => false,
    }
}

/// Look up a symbol's signature via the FST → symbol record path.
/// Returns `None` when the name isn't in the FST or when the resolved
/// record has no captured signature. Best-effort by design — the
/// classifier degrades to path-only heuristics when this returns `None`.
fn lookup_signature(reader: &IndexReader, name: &str) -> Option<String> {
    let fst = reader.symbol_fst_reader()?;
    let lower = name.to_lowercase();
    let indices = fst.find(&lower);
    let idx = indices.first().copied()?;
    let rec = reader.symbol(idx as usize)?;
    let sig = reader.read_string(rec.signature_offset);
    if sig.is_empty() {
        None
    } else {
        Some(sig.to_string())
    }
}

/// PR-impact-mode assembler. Public for bench/test access — see
/// [`assemble_symbol`] for the rationale.
#[doc(hidden)]
pub fn assemble_pr_impact(
    args: &BundleArgs<'_>,
    ctx: &BundleCtx<'_>,
) -> Result<(BundleResponse, ModeSpecificMeta)> {
    use crate::diff::ChangeKind;

    let base = args
        .base
        .context("`vex bundle --mode pr-impact` requires `--base <REV>`")?;

    // Mirror `Commands::Reachable` — call graph is a hard requirement
    // because the BFS layer needs persistent caller edges. Bail with a
    // clear remediation message instead of degrading silently.
    if !ctx.reader.has_call_graph() {
        anyhow::bail!(
            "no call graph in index — `vex bundle --mode pr-impact` requires a v4 index built \
             without `--no-call-graph`. Rebuild with `vex index`."
        );
    }

    // Step 1 — diff against the requested base. Errors propagate (bad
    // ref, not a git repo) — those are config problems, not empty
    // diffs. Per-query glob filter applies as a post-step so the
    // semantics match `Commands::Diff` at `cli/mod.rs:1632`.
    let path_scope =
        crate::cli::scope::PathScope::from_args(&ctx.scope.include, &ctx.scope.exclude)?;
    let mut changes = crate::diff::diff_against_base(&ctx.root, base, ctx.excludes, usize::MAX)?;
    changes.retain(|c| path_scope.accept(&c.path));

    // Bind the BFS layer's `callers_of` closure to the persistent call
    // graph. The saturation warning mirrors `Commands::Paths` —
    // important signal for "your walk missed callers" without aborting.
    let reader = ctx.reader;
    let cap = crate::callgraph::CALLERS_FETCH_CAP;
    let callers_of = |name: &str| {
        let callers = crate::store::call_graph::find_callers_fast(reader, name, cap);
        if callers.len() == cap {
            eprintln!(
                "warning: `{name}` has at least {cap} direct callers; \
                 pr-impact walk may be incomplete for this node"
            );
        }
        callers
    };

    // Step 2 — direct changed items + transitive caller BFS per change.
    // We dedupe transitive callers across multiple changed symbols by
    // `(name, path, line)` so a hot caller of many touched functions
    // surfaces once.
    let mut items: Vec<BundleItem> = Vec::new();
    let mut changed_count = 0usize;
    let mut transitive_callers: Vec<(String, String, usize)> = Vec::new();
    let mut transitive_seen: std::collections::HashSet<(String, String, usize)> =
        std::collections::HashSet::new();
    let mut test_items: Vec<(String, String, usize)> = Vec::new();
    let mut test_seen: std::collections::HashSet<(String, String, usize)> =
        std::collections::HashSet::new();
    let mut unreachable_changes: Vec<String> = Vec::new();
    let mut changed_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for change in &changes {
        changed_paths.insert(change.path.clone());

        items.push(BundleItem {
            core: BundleCoreItem {
                name: change.name.clone(),
                kind: change.symbol_kind.clone(),
                path: change.path.clone(),
                line: change.line,
                signature: None,
            },
            signals: signals_fst_hit(),
            rank_percentile: 0.0,
            role_rank: changed_count as u32,
            role: "changed",
            body: None,
            similarity: None,
        });
        changed_count += 1;

        // Only walk transitive callers for *still-present* symbols.
        // Removed symbols have nothing the BFS can reach from in the
        // current codebase — including them would yield stale callers.
        let walk = matches!(
            change.kind,
            ChangeKind::BodyChanged | ChangeKind::Moved | ChangeKind::Added
        );
        if !walk {
            continue;
        }

        // Passing `callers_of` by value compiles because the closure
        // captures only `reader: &IndexReader` + `cap: usize` — both
        // `Copy` — making the closure itself `Copy`. If a future
        // refactor adds a non-Copy capture (e.g. a `String` logging
        // context), switch to `&callers_of` so the auto-derived
        // `&F: Fn` impl applies (review fix H1).
        let reachable = crate::callgraph::bfs::find_reachable(
            callers_of,
            &change.name,
            args.depth,
            crate::callgraph::CALLERS_FETCH_CAP,
        );
        // `walk` above already gated out `ChangeKind::Removed`, so any
        // remaining change here is reachable-eligible.
        if reachable.is_empty() {
            unreachable_changes.push(change.name.clone());
        }

        for r in reachable {
            let key = (r.name.clone(), r.path.clone(), r.line);
            let sig = lookup_signature(reader, &r.name);
            let is_test = is_test_path(&r.path) || signature_marks_test(sig.as_deref());
            if is_test {
                if test_seen.insert(key.clone()) {
                    test_items.push(key);
                }
            } else if transitive_seen.insert(key.clone()) {
                transitive_callers.push(key);
            }
        }
    }

    // Step 3 — append role=transitive_caller items (cap is implicit via
    // crate::callgraph::CALLERS_FETCH_CAP-per-change; the dedupe set above
    // collapses cross-change overlap). The transitive set is unordered
    // beyond insertion order — keep that order so a re-run on the same
    // diff produces a stable bundle.
    let transitive_count = transitive_callers.len();
    for (i, (name, path, line)) in transitive_callers.into_iter().enumerate() {
        items.push(BundleItem {
            core: BundleCoreItem {
                kind: caller_kind(&name).to_string(),
                name,
                path,
                line,
                signature: None,
            },
            signals: signals_fst_hit(),
            rank_percentile: 0.0,
            role_rank: i as u32,
            role: "transitive_caller",
            body: None,
            similarity: None,
        });
    }

    // Step 4 — append role=test items, truncated at tests_max. This is
    // the only role we hard-cap (transitive callers cap implicitly via
    // the BFS limit; changes never cap — agents need to see every
    // touched symbol).
    let tests_total = test_items.len();
    let tests_truncated = tests_total > args.tests_max;
    for (i, (name, path, line)) in test_items.into_iter().take(args.tests_max).enumerate() {
        items.push(BundleItem {
            core: BundleCoreItem {
                name,
                kind: "function".to_string(),
                path,
                line,
                signature: None,
            },
            signals: signals_fst_hit(),
            rank_percentile: 0.0,
            role_rank: i as u32,
            role: "test",
            body: None,
            similarity: None,
        });
    }
    let test_count = items.iter().filter(|i| i.role == "test").count();

    // Step 5 — global monotonic-descending rank_percentile across all
    // items (A6 invariant). Body always rank=1.0 in symbol mode; here
    // the highest-rank slot belongs to the first changed symbol.
    let total = items.len();
    for (i, item) in items.iter_mut().enumerate() {
        item.rank_percentile = global_rank_percentile(i, total);
    }

    let mode_hints = serde_json::json!({
        "base": base,
        "depth": args.depth,
        "changed_count": changed_count,
        "transitive_caller_count": transitive_count,
        "test_count": test_count,
        "tests_truncated": tests_truncated,
        "unreachable_changes": unreachable_changes,
        "empty_reason": if changed_count == 0 { Some("no_changes") } else { None },
    });

    // Per-mode meta: surfaces the scope of the diff so agents that
    // consume both `_meta.vex.dev/diff_filter` and the bundle items
    // can cross-correlate without re-running `git diff`. Shape mirrors
    // the Phase 13.7-D3 envelope contract in `protocol/mod.rs:54-64`.
    let diff_filter_meta = serde_json::json!({
        "scope": format!("pr-impact:{base}"),
        "changed_paths": changed_paths.into_iter().collect::<Vec<_>>(),
        "retained": total,
        "dropped": 0_usize,
    });

    Ok((
        BundleResponse {
            mode: args.mode.as_str(),
            items,
            mode_hints: Some(mode_hints),
        },
        ModeSpecificMeta {
            diff_filter: Some(diff_filter_meta),
        },
    ))
}

// ---------------------------------------------------------------------------
// `--mode project` (Inc 4) — top-N by reverse indegree
// ---------------------------------------------------------------------------
//
// Architect-review A5: NOT PageRank. Just `count(distinct callers)` per
// callee, sorted descending. Documented in `mode_hints.scoring` as
// `"reverse_indegree"` and labelled experimental in the CLI help — the
// "structural importance" framing is honest about what indegree
// measures (and doesn't).

/// Project-mode assembler. Public for bench/test access — see
/// [`assemble_symbol`] for the rationale.
#[doc(hidden)]
pub fn assemble_project(
    args: &BundleArgs<'_>,
    ctx: &BundleCtx<'_>,
) -> Result<(BundleResponse, ModeSpecificMeta)> {
    let has_call_graph = ctx.reader.has_call_graph();

    // Soft-degrade (architect-review): no call graph → empty items +
    // empty_reason, NOT a hard error. Unlike pr-impact, the agent can
    // still use other modes without rebuilding the index.
    if !has_call_graph {
        return Ok((
            BundleResponse {
                mode: args.mode.as_str(),
                items: Vec::new(),
                mode_hints: Some(serde_json::json!({
                    "empty_reason": "no_call_graph",
                    "scoring": "reverse_indegree",
                    "has_call_graph": false,
                    "top_n": args.top_n,
                    "path_glob": args.path_glob,
                    "total_ranked_symbols": 0,
                })),
            },
            ModeSpecificMeta::default(),
        ));
    }

    // Path-glob → PathScope (single include, no excludes). Build once
    // here so the helper module stays generic; `--path-glob 'src/**'`
    // is the typical scoping use case.
    let path_scope = if let Some(glob) = args.path_glob {
        let includes = vec![glob.to_string()];
        Some(crate::cli::scope::PathScope::from_args(&includes, &[])?)
    } else {
        None
    };

    let report =
        crate::callgraph::indegree::top_n_by_indegree(ctx.reader, args.top_n, path_scope.as_ref());

    let mut items: Vec<BundleItem> = Vec::with_capacity(report.rows.len());
    for (i, row) in report.rows.iter().enumerate() {
        let Some(rec) = ctx.reader.symbol(row.sym_idx as usize) else {
            continue;
        };
        let name = ctx.reader.read_string(rec.name_offset).to_string();
        let path = ctx.reader.read_string(rec.file_offset).to_string();
        let signature_raw = ctx.reader.read_string(rec.signature_offset);
        let signature = if signature_raw.is_empty() {
            None
        } else {
            Some(signature_raw.to_string())
        };
        let kind = crate::index::symbols::SymbolKind::try_from(rec.kind)
            .map(|k| k.as_str().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        items.push(BundleItem {
            core: BundleCoreItem {
                name,
                kind,
                path,
                line: rec.line as usize,
                signature,
            },
            signals: Signals {
                fst_hit: true,
                indegree: Some(row.indegree),
                ..Signals::default()
            },
            rank_percentile: 0.0, // overwritten below
            role_rank: i as u32,
            role: "top",
            body: None,
            similarity: None,
        });
    }

    let total = items.len();
    for (i, item) in items.iter_mut().enumerate() {
        item.rank_percentile = global_rank_percentile(i, total);
    }

    let empty_reason = if items.is_empty() {
        if report.total_ranked == 0 {
            Some("no_call_edges")
        } else {
            Some("path_glob_filtered_all")
        }
    } else {
        None
    };

    let mode_hints = serde_json::json!({
        "scoring": "reverse_indegree",
        "top_n": args.top_n,
        "path_glob": args.path_glob,
        "total_ranked_symbols": report.total_ranked,
        "has_call_graph": true,
        "empty_reason": empty_reason,
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
    fn signals_semantic_sets_rank_and_clears_fst() {
        let s = signals_semantic(3);
        assert!(!s.fst_hit);
        assert_eq!(s.semantic_rank, Some(3));
    }

    #[test]
    fn resolve_bundle_root_uses_explicit_some() {
        let r = resolve_bundle_root(Some(PathBuf::from("/tmp"))).unwrap();
        assert_eq!(r, Path::new("/tmp"));
    }
}
