//! `vex bundle --mode pr-impact` assembler.
//!
//! Builds an impact bundle from a git diff (default `base = main`):
//! discovers changed symbols, walks the persistent callgraph
//! (`callers_of` × depth), dedupes, and emits a single capped envelope.
//! The aggregate node cap (`MAX_PR_IMPACT_NODES`) is the only thing
//! protecting an agent from a runaway PR — keep it visible.
//!
//! Isolated from `mod.rs` so the per-mode assembler bodies don't
//! fight for screen real estate with the public types + dispatch.

use std::collections::{BTreeSet, HashSet};

use anyhow::{Context, Result};

use crate::store::reader::IndexReader;

use super::{
    caller_kind, global_rank_percentile, signals_fst_hit, BundleArgs, BundleCoreItem, BundleCtx,
    BundleItem, BundleResponse, ModeSpecificMeta,
};

/// Aggregate node cap for `vex bundle --mode pr-impact` (review H9, v1.10.1).
///
/// Before v1.10.1 the BFS bound was per-changed-symbol via
/// `CALLERS_FETCH_CAP`, so a refactor PR touching N symbols could surface up
/// to `N × CALLERS_FETCH_CAP` callers — well past what an agent can consume.
/// The aggregate cap is intentionally generous to absorb typical refactors
/// (deduped transitive set rarely exceeds a few hundred per change) while
/// still putting a ceiling on pathological PRs.
pub const MAX_PR_IMPACT_NODES: usize = 5_000;

/// PR-impact-mode assembler. Public for bench/test access — see
/// `symbol::assemble_symbol` for the rationale.
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
    //
    // Aggregate budget (review H9, v1.10.1): the legacy per-change BFS
    // cap multiplied with the number of changed symbols, so a refactor PR
    // touching N functions could pull up to N × CALLERS_FETCH_CAP nodes.
    // We now also gate the total result size so large PRs don't blow
    // past agent-friendly bundle sizes. The cap is intentionally generous
    // (changed + transitive + test combined) and surfaces via
    // `mode_hints.budget_exceeded` so callers can react instead of
    // silently truncating.
    let mut items: Vec<BundleItem> = Vec::new();
    let mut changed_count = 0usize;
    let mut transitive_callers: Vec<(String, String, usize)> = Vec::new();
    let mut transitive_seen: HashSet<(String, String, usize)> = HashSet::new();
    let mut test_items: Vec<(String, String, usize)> = Vec::new();
    let mut test_seen: HashSet<(String, String, usize)> = HashSet::new();
    let mut unreachable_changes: Vec<String> = Vec::new();
    let mut changed_paths: BTreeSet<String> = BTreeSet::new();
    let mut budget_exceeded = false;

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
            // Aggregate-budget gate (review H9). When the combined node
            // count exceeds the cap we stop *adding* new transitive /
            // test callers but keep processing the outer change loop so
            // every changed symbol still appears in the bundle. The
            // `budget_exceeded` flag is surfaced in `mode_hints`.
            let total_so_far = changed_count + transitive_callers.len() + test_items.len();
            if total_so_far >= MAX_PR_IMPACT_NODES {
                budget_exceeded = true;
                break;
            }
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
        if budget_exceeded {
            break;
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

    // `empty_reason` is set when no items would surface at all:
    //   - `no_changes`         — diff produced zero touched symbols, so
    //                            there is nothing to walk callers from.
    //   - `pr_impact_budget_exceeded` — the aggregate node cap fired
    //                            before any callers were added AND no
    //                            changed symbols survived diff filtering.
    //                            In practice this is rare because changed
    //                            symbols always go in first; we keep it
    //                            in the vocabulary so review H9's wording
    //                            stays honest.
    let empty_reason = if changed_count == 0 && transitive_count == 0 && test_count == 0 {
        if budget_exceeded {
            Some("pr_impact_budget_exceeded")
        } else {
            Some("no_changes")
        }
    } else {
        None
    };
    let mode_hints = serde_json::json!({
        "base": base,
        "depth": args.depth,
        "changed_count": changed_count,
        "transitive_caller_count": transitive_count,
        "test_count": test_count,
        "tests_truncated": tests_truncated,
        "budget_exceeded": budget_exceeded,
        "max_pr_impact_nodes": MAX_PR_IMPACT_NODES,
        "unreachable_changes": unreachable_changes,
        "empty_reason": empty_reason,
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
