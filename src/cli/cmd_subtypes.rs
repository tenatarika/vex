//! `vex subtypes` — transitive-down closure over `extends`/`implements`
//! edges (`docs/HIERARCHY-EDGES.md` §7, §8, P3).
//!
//! Unlike `vex implementations` (direct children only, one
//! `find_hierarchy_edges_by_symbol` lookup), `subtypes` walks the whole
//! descendant tree via a bounded BFS: `Extends`/`Implements` edges compose
//! ("if C extends B and B extends A, then C is a subtype of A"), but
//! `Uses` (trait/mixin composition, PHP `use` / Ruby include-extend-
//! prepend) does not — mixing in a trait doesn't make you a subtype of
//! everything the trait itself composes, so `Uses` edges are excluded from
//! the traversal even though the same section stores them.
//!
//! There is deliberately NO live-walk fallback here (unlike
//! `implementations`): a transitive closure needs the persisted graph —
//! the tree-sitter walk in `crate::hierarchy` only ever answers "who
//! directly names this literal identifier", and re-running it recursively
//! per intermediate type would mean re-parsing the whole tree once per
//! BFS layer, which is both slow and out of scope for this command. When
//! the index has no hierarchy section, `subtypes` reports an empty result
//! with a stderr hint instead of silently doing something much slower and
//! subtly different from what `implementations` does in the same
//! situation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::cmd_implementations::resolve_name_to_indices;
use super::common::{resolve_diff_filter, resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::scope;
use crate::protocol::capabilities;
use crate::store::format::EdgeKind;
use crate::store::reader::IndexReader;

/// One transitive subtype, as reported to the caller: the child symbol's
/// display data plus the BFS depth (hops) from the queried base type.
/// `depth == 1` means a direct child (same as `vex implementations` would
/// report); `depth == 2` a grandchild; etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubtypeMatch {
    pub name: String,
    pub path: String,
    pub line: usize,
    pub relation: &'static str,
    pub depth: usize,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn subtypes(
    ctx: &CmdCtx<'_>,
    name: String,
    path: Option<std::path::PathBuf>,
    limit: usize,
    depth: usize,
    auto_update: bool,
    no_stale_check: bool,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    // Canonicalize up front (matches `cmd_check.rs`/`cmd_impact.rs`/
    // `cmd_status.rs`/`cmd_usages.rs`, and `cmd_implementations.rs` as of
    // P3) — required for cache-path writer/reader symmetry. See the
    // matching comment in `cmd_implementations.rs::implementations` for
    // why an uncanonicalized `--path` can silently miss the real index.
    let root = resolve_root(path)?
        .canonicalize()
        .context("canonicalize root")?;
    // A depth of 0 excludes even direct subtypes (the BFS stops before
    // expanding the seed), which is never what a caller wants — reject it
    // with a clear message rather than returning a silently empty result.
    if depth == 0 {
        anyhow::bail!("--depth must be at least 1");
    }
    // Staleness / auto-update runs exactly once inside `try_open_reader` →
    // `ensure_index_ready` (see the matching note in
    // `cmd_implementations.rs`); no standalone `handle_staleness` here or it
    // would fire — and potentially rebuild — twice.
    let changed_paths = resolve_diff_filter(&root, &diff)?;
    let start = Instant::now();

    let reader = try_open_reader(&root, auto_update, no_stale_check, ctx);

    let matches: Vec<SubtypeMatch> = match reader {
        Some(reader) if reader.has_hierarchy_edges() => subtypes_from_index(&reader, &name, depth),
        _ => {
            // No-section behavior (§7, §8): no live-walk fallback exists
            // for a transitive query, so this is a legitimate empty
            // result, not an error. Surface a stderr hint so the caller
            // knows why, without failing the command.
            eprintln!(
                "hint: run `vex index` to enable `vex subtypes` (requires a v8+ index with hierarchy edges)"
            );
            Vec::new()
        }
    };

    let matches: Vec<_> = matches
        .into_iter()
        .filter(|m| {
            path_scope.accept(&m.path)
                && changed_paths.as_ref().is_none_or(|cp| cp.contains(&m.path))
        })
        .take(limit)
        .collect();
    let elapsed = start.elapsed();

    // Mirrors `implementations`' exit-code contract: an empty result set
    // (whether from a real empty transitive closure, or from having no
    // hierarchy section at all) signals "no results" the same way.
    if matches.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "base": name,
                        "relation": m.relation,
                        "path": m.path,
                        "line": m.line,
                        "depth": m.depth,
                    })
                })
                .collect();
            print_envelope(
                &json,
                capabilities::current(),
                super::output::default_meta_for(&root),
            );
        }
        OutputFormat::Text => {
            if matches.is_empty() {
                println!("No subtypes of \"{name}\" in {elapsed:.2?}");
            } else {
                println!("{name}: {} subtypes in {elapsed:.2?}\n", matches.len());
                for m in &matches {
                    println!(
                        "  {:<40} ({}, depth {})  {}:{}",
                        m.name, m.relation, m.depth, m.path, m.line
                    );
                }
            }
        }
        OutputFormat::Compact => {
            for m in &matches {
                println!(
                    "{} {} {} {} {}:{}",
                    m.relation, m.depth, name, m.name, m.path, m.line
                );
            }
        }
    }
    Ok(())
}

/// Best-effort index open, identical policy to
/// `cmd_implementations::try_open_reader`: every failure (missing index,
/// stale-without-auto-update, corrupt mmap, ...) folds into `None`, which
/// the caller treats as "no hierarchy section" — the difference from
/// `implementations` is only in what happens next (no live-walk fallback
/// here, see module docs).
fn try_open_reader(
    root: &Path,
    auto_update: bool,
    no_stale_check: bool,
    ctx: &CmdCtx<'_>,
) -> Option<IndexReader> {
    let index_path = ensure_index_ready(
        root,
        auto_update,
        no_stale_check,
        false,
        ctx.local_cache_active,
        ctx.cfg,
    )
    .ok()?;
    IndexReader::open(&index_path).ok()
}

/// Resolve `name` to its starting symbol index(es) (a name can map to
/// several — see `resolve_name_to_indices`), then run the transitive BFS
/// (`transitive_subtypes`) against the real index via a small closure that
/// wraps `find_hierarchy_edges_by_symbol` + `EdgeKind` filtering. Symbol
/// index → display-data resolution (name via `reader.symbol`, path via
/// `reader.file_paths()`) happens once at the end, only for symbols that
/// actually survived the BFS — cheaper than resolving on every visit.
fn subtypes_from_index(reader: &IndexReader, name: &str, depth_cap: usize) -> Vec<SubtypeMatch> {
    let starts = resolve_name_to_indices(reader, name);
    if starts.is_empty() {
        return Vec::new();
    }

    let children_of = |sym_idx: u32| -> Vec<(u32, EdgeKind)> {
        reader
            .find_hierarchy_edges_by_symbol(sym_idx)
            .into_iter()
            .filter_map(|edge| {
                let kind = EdgeKind::try_from(edge.edge_kind_bits()).ok()?;
                // Only Extends/Implements compose transitively — Uses
                // (mixin/trait composition) does not (module docs above).
                if matches!(kind, EdgeKind::Extends | EdgeKind::Implements) {
                    Some((edge.from_sym_idx, kind))
                } else {
                    None
                }
            })
            .collect()
    };

    let hits = transitive_subtypes(&starts, children_of, depth_cap);

    let file_paths = reader.file_paths();
    // The declaration site (file/line) of a subtype's OWN extends/implements
    // clause isn't carried by the BFS tuple (it tracks only symbol indices +
    // best depth). Build a `child_sym_idx -> (file, line)` map ONCE from a
    // single full-edge scan rather than re-scanning `hierarchy_edges_all()`
    // per result row (which would be O(result_count × total_edges)). A child
    // can appear in several edges (multiple inheritance / interfaces); keep
    // the first, matching the previous `.find()` semantics — any one site is
    // an acceptable representative, same as `vex implementations`.
    let edge_site: HashMap<u32, (u32, u32)> = {
        let mut m: HashMap<u32, (u32, u32)> = HashMap::new();
        for e in reader.hierarchy_edges_all() {
            m.entry(e.from_sym_idx)
                .or_insert((e.from_file_id, e.line()));
        }
        m
    };
    let mut out = Vec::with_capacity(hits.len());
    for (sym_idx, kind, hop_depth) in hits {
        let Some(rec) = reader.symbol(sym_idx as usize) else {
            continue;
        };
        let Some(&(edge_file, edge_line)) = edge_site.get(&sym_idx) else {
            continue;
        };
        let Some(path) = file_paths.get(edge_file as usize) else {
            continue;
        };
        let relation = match kind {
            EdgeKind::Extends => "extends",
            EdgeKind::Implements => "implements",
            EdgeKind::Uses => continue, // unreachable given the BFS filter above; defensive
        };
        out.push(SubtypeMatch {
            name: reader.read_string(rec.name_offset).to_string(),
            path: path.clone(),
            line: edge_line as usize,
            relation,
            depth: hop_depth,
        });
    }
    out
}

/// Pure, reader-independent transitive-BFS core. Takes a closure
/// `children_of(sym_idx) -> Vec<(child_sym_idx, EdgeKind)>` instead of a
/// live `&IndexReader` so the cycle-guard and depth-cap logic — the two
/// safety-critical properties of this whole command — can be unit-tested
/// against a plain in-memory fixture, with no mmap/on-disk index
/// involved.
///
/// Returns `(symbol_idx, edge_kind_that_discovered_it, depth)` triples,
/// one per symbol reached, each symbol appearing at most once (BFS
/// dedup — first-reached depth wins, which is also the SHORTEST depth
/// since BFS explores in depth order).
///
/// **Cycle guard (mandatory, `docs/HIERARCHY-EDGES.md` §7):** `visited`
/// is checked-and-inserted before a symbol is enqueued as a new frontier
/// node, not before it's dequeued — this is what makes self-edges
/// (`A extends A`) and 2-cycles (`A extends B, B extends A`) terminate:
/// a symbol can only ever be enqueued once, so the frontier is bounded by
/// the total symbol count regardless of how tangled the edge graph is.
///
/// **Depth cap (mandatory, belt-and-suspenders alongside the cycle
/// guard):** `depth_cap` bounds how many hops the BFS will take from the
/// start set, independent of `visited` — even on an acyclic but very deep
/// or pathologically wide graph, this keeps the walk bounded.
pub(crate) fn transitive_subtypes(
    starts: &[u32],
    children_of: impl Fn(u32) -> Vec<(u32, EdgeKind)>,
    depth_cap: usize,
) -> Vec<(u32, EdgeKind, usize)> {
    let mut visited: HashSet<u32> = starts.iter().copied().collect();
    let mut queue: VecDeque<(u32, usize)> = starts.iter().map(|&s| (s, 0)).collect();
    let mut out = Vec::new();

    while let Some((parent, parent_depth)) = queue.pop_front() {
        if parent_depth >= depth_cap {
            continue;
        }
        let child_depth = parent_depth + 1;
        for (child, kind) in children_of(parent) {
            if !visited.insert(child) {
                // Already visited (or is itself one of the start symbols,
                // e.g. a self-edge `A extends A`) — skip re-enqueueing.
                // This is the cycle guard: without it, `A extends B, B
                // extends A` would bounce between the two forever.
                continue;
            }
            out.push((child, kind, child_depth));
            queue.push_back((child, child_depth));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `children_of` closure from a plain adjacency map — the
    /// fixture shape for all BFS unit tests below. `edges` maps a parent
    /// symbol idx to its direct children (as the section's
    /// `find_hierarchy_edges_by_symbol` would return them).
    fn fixture(
        edges: std::collections::HashMap<u32, Vec<(u32, EdgeKind)>>,
    ) -> impl Fn(u32) -> Vec<(u32, EdgeKind)> {
        move |sym_idx| edges.get(&sym_idx).cloned().unwrap_or_default()
    }

    #[test]
    fn transitive_descent_across_two_hops() {
        // A <- B <- C  (B extends A, C extends B). Subtypes of A: B at
        // depth 1, C at depth 2.
        let mut edges = std::collections::HashMap::new();
        edges.insert(0u32, vec![(1u32, EdgeKind::Extends)]); // A's children: B
        edges.insert(1u32, vec![(2u32, EdgeKind::Extends)]); // B's children: C
        let f = fixture(edges);

        let mut hits = transitive_subtypes(&[0], f, 64);
        hits.sort_by_key(|(idx, _, _)| *idx);

        assert_eq!(hits.len(), 2, "expected B and C, got {hits:?}");
        assert_eq!(hits[0], (1, EdgeKind::Extends, 1));
        assert_eq!(hits[1], (2, EdgeKind::Extends, 2));
    }

    #[test]
    fn cycle_guard_terminates_on_mutual_cycle() {
        // A extends B, B extends A — must terminate, not hang, and must
        // not report either symbol as its own subtype.
        let mut edges = std::collections::HashMap::new();
        edges.insert(0u32, vec![(1u32, EdgeKind::Extends)]); // A -> B
        edges.insert(1u32, vec![(0u32, EdgeKind::Extends)]); // B -> A
        let f = fixture(edges);

        let hits = transitive_subtypes(&[0], f, 64);

        // Only B should be reported: A is the start symbol (pre-seeded
        // into `visited`), so the B->A edge is a no-op re-visit, not a
        // fresh hit. Must terminate (this test itself would hang forever
        // without the cycle guard).
        assert_eq!(hits, vec![(1, EdgeKind::Extends, 1)]);
    }

    #[test]
    fn self_edge_does_not_loop_or_self_report() {
        // A extends A (malformed/adversarial input) — must not infinite
        // loop and must not report A as its own subtype.
        let mut edges = std::collections::HashMap::new();
        edges.insert(0u32, vec![(0u32, EdgeKind::Extends)]);
        let f = fixture(edges);

        let hits = transitive_subtypes(&[0], f, 64);
        assert!(
            hits.is_empty(),
            "self-edge must not produce a hit: {hits:?}"
        );
    }

    #[test]
    fn depth_cap_limits_to_direct_children_only() {
        // A <- B <- C, same chain as the two-hop test, but depth_cap = 1
        // must return only B (depth 1), never C (depth 2).
        let mut edges = std::collections::HashMap::new();
        edges.insert(0u32, vec![(1u32, EdgeKind::Extends)]);
        edges.insert(1u32, vec![(2u32, EdgeKind::Extends)]);
        let f = fixture(edges);

        let hits = transitive_subtypes(&[0], f, 1);
        assert_eq!(hits, vec![(1, EdgeKind::Extends, 1)]);
    }

    #[test]
    fn uses_edges_are_excluded_by_the_caller_filter() {
        // The BFS core itself is kind-agnostic (it takes whatever
        // children_of hands it) — the Extends/Implements-only filter
        // lives in `subtypes_from_index`'s closure, not here. This test
        // documents that a fixture pre-filtered the same way (as the real
        // closure does) simply never surfaces a Uses child.
        let mut edges = std::collections::HashMap::new();
        edges.insert(0u32, vec![(1u32, EdgeKind::Extends)]);
        // A Uses-composing symbol is absent from the map entirely,
        // mirroring what the real closure's `filter_map` would produce.
        let f = fixture(edges);

        let hits = transitive_subtypes(&[0], f, 64);
        assert_eq!(hits, vec![(1, EdgeKind::Extends, 1)]);
    }

    #[test]
    fn diamond_inheritance_reports_each_symbol_once() {
        // A <- B, A <- C, B <- D, C <- D (D extends both B and C, both of
        // which extend A). D must be reported exactly once despite being
        // reachable via two paths.
        let mut edges = std::collections::HashMap::new();
        edges.insert(
            0u32,
            vec![(1u32, EdgeKind::Extends), (2u32, EdgeKind::Extends)],
        ); // A -> B, A -> C
        edges.insert(1u32, vec![(3u32, EdgeKind::Extends)]); // B -> D
        edges.insert(2u32, vec![(3u32, EdgeKind::Extends)]); // C -> D
        let f = fixture(edges);

        let hits = transitive_subtypes(&[0], f, 64);
        let d_hits: Vec<_> = hits.iter().filter(|(idx, _, _)| *idx == 3).collect();
        assert_eq!(d_hits.len(), 1, "D must appear exactly once: {hits:?}");
    }
}
