//! Graph projection (`docs/V9-FORMAT.md` §3.2, §13 R6/R7/R8).
//!
//! Turns the writer's already-resolved call/ref/hierarchy edges into a
//! single undirected, weighted, symmetric graph over *eligible* symbols
//! — the input [`leiden::run`](super::leiden::run) clusters. This module
//! never touches writer internals: [`ProjectionInput`] is a small,
//! self-contained set of plain records so P4 only needs to translate its
//! own in-memory builders into this shape.

use std::collections::HashMap;

use crate::parse::language::Language;

use super::leiden::LeidenGraph;

/// Kind weight for a `CallEdgeBuilder`-sourced site (§3.2 table).
pub const CALL_WEIGHT: u32 = 2;
/// Kind weight for a resolved ref edge whose `kind` is `RefKind::Call`.
pub const REF_CALL_WEIGHT: u32 = 2;
/// Kind weight for every other resolved ref kind (Type/Value/Macro).
pub const REF_OTHER_WEIGHT: u32 = 1;
/// Kind weight for a hierarchy edge (Q4 verdict: include at weight 1).
pub const HIERARCHY_WEIGHT: u32 = 1;
/// `w(u,v) = min(sum of kind weights, PAIR_CAP)` (§3.2).
pub const PAIR_CAP: u32 = 8;

/// `RefKind::Call`'s on-disk discriminant (`src/parse/scope/mod.rs`),
/// duplicated here as a bare constant rather than importing `RefKind` to
/// keep this module decoupled from the binder — `ProjectionRefEdge::kind`
/// is already a raw `u8` mirroring the real `RefEdgeBuilder` shape.
const REF_KIND_CALL: u8 = 2;

/// One symbol record, as the writer already has it in memory (path,
/// line, kind, name, `sym_idx`, language) — see `docs/V9-FORMAT.md` §3.2.
/// Borrowed (`path`/`name` are `&'a str`, not `String`) so the writer can
/// feed straight from its own `ParsedFile`/`ParsedSymbol` data without an
/// extra per-symbol allocation — this is [`ProjectionInput`]'s own
/// "without an extra clone" promise, applied to this struct too.
#[derive(Debug, Clone, Copy)]
pub struct ProjectionSymbol<'a> {
    pub sym_idx: u32,
    pub path: &'a str,
    pub line: u32,
    /// `crate::index::symbols::SymbolKind` discriminant.
    pub kind: u8,
    pub name: &'a str,
    pub language: Option<Language>,
}

/// `CallEdgeBuilder`-shaped: caller `sym_idx` (exact) + callee *name*
/// (needs resolving, §3.2). `callee_name` is borrowed straight from the
/// writer's own `CallEdgeBuilder.callee_name` — no per-edge clone.
#[derive(Debug, Clone, Copy)]
pub struct ProjectionCallEdge<'a> {
    pub caller_sym_idx: u32,
    pub callee_name: &'a str,
    pub line: u32,
}

/// A resolved ref edge (`RefEdgeBuilder`-shaped). `ambiguous` is a
/// parallel slice on [`ProjectionInput`], not a field here, mirroring
/// the writer's transient `Vec<bool>` (§13 R7).
#[derive(Debug, Clone)]
pub struct ProjectionRefEdge {
    pub from_file_id: u32,
    pub line: u32,
    pub to_sym_idx: u32,
    /// `crate::parse::scope::RefKind` discriminant.
    pub kind: u8,
}

/// A resolved hierarchy edge (`HierarchyEdgeBuilder`-shaped, trimmed to
/// just the two endpoints — hierarchy edges carry no "site" used for
/// dedup against call/ref edges, §3.2 table).
#[derive(Debug, Clone, Copy)]
pub struct ProjectionHierarchyEdge {
    pub from_sym_idx: u32,
    pub to_sym_idx: u32,
}

/// Everything [`project`] needs. Borrowed, so the writer (P4) can feed
/// straight from its own builder `Vec`s without an extra clone.
#[derive(Debug, Clone, Copy)]
pub struct ProjectionInput<'a> {
    /// Total `SymbolRecord` count (the writer's `symbol_count`) — sizes
    /// the final sentinel-filled `assign` array; may exceed
    /// `symbols.len()` when a test / partial caller only supplies the
    /// symbols relevant to it (every omitted `sym_idx` behaves as
    /// NOT_ELIGIBLE, same as a genuinely ineligible symbol, since there
    /// is nothing to look it up in `symbols`).
    pub symbol_count: u32,
    pub symbols: &'a [ProjectionSymbol<'a>],
    pub call_edges: &'a [ProjectionCallEdge<'a>],
    pub ref_edges: &'a [ProjectionRefEdge],
    /// Parallel to `ref_edges`: `true` means "drop, ambiguous resolution"
    /// (§13 R7). Must be `ref_edges.len()` long, or empty to mean "none
    /// ambiguous" (convenience for callers/tests with no ambiguity to
    /// express).
    pub ambiguous: &'a [bool],
    pub hierarchy_edges: &'a [ProjectionHierarchyEdge],
    /// `from_file_id -> path`, used only to attribute ref edges via
    /// nearest-preceding symbol in the same file (§3.2, F8) and to key
    /// dedup/pair-weight accumulation by file. Index == `from_file_id`.
    /// May be empty if `ref_edges` is also empty.
    pub file_paths: &'a [String],
}

/// Output of [`project`]: a dense undirected weighted graph over
/// eligible symbols, plus the mapping back to `sym_idx` space that
/// `mod.rs`'s `finalize` needs.
#[derive(Debug, Clone)]
pub struct ProjectedGraph {
    pub symbol_count: u32,
    /// Dense node id -> `sym_idx`, in canonical order (R6:
    /// `(path, line, kind, name, sym_idx)`) — this order *is* the Leiden
    /// node order and is what ties are broken against throughout
    /// [`super::leiden`].
    pub node_sym_idx: Vec<u32>,
    /// The ready-to-cluster graph (`LeidenGraph::from_pairs` over
    /// `pairs`, sizes all 1).
    pub graph: LeidenGraph,
    /// The deduped, capped, undirected pair list in node-id space
    /// (`(min_node, max_node, weight)`), kept around so `mod.rs` can
    /// recompute internal/cut weight against the final partition
    /// without re-running projection.
    pub pairs: Vec<(u32, u32, u32)>,
}

/// §3.2: excluded kinds are Module(13), Heading(12), Package(11);
/// excluded languages are Markdown/Yaml/Toml/Css/Html.
pub fn is_eligible(kind: u8, language: Option<Language>) -> bool {
    const MODULE: u8 = 13;
    const HEADING: u8 = 12;
    const PACKAGE: u8 = 11;
    if matches!(kind, MODULE | HEADING | PACKAGE) {
        return false;
    }
    !matches!(
        language,
        Some(Language::Markdown)
            | Some(Language::Yaml)
            | Some(Language::Toml)
            | Some(Language::Css)
            | Some(Language::Html)
    )
}

/// Sentinel for "no node" in the dense `sym_idx -> node id` array
/// (`sym_to_node` below) — real node ids are always `< n <= u32::MAX`
/// in practice (symbol counts never approach 4 billion), so this never
/// collides with a genuine node id.
const NOT_NODE: u32 = u32::MAX;

/// `(min(a,b), max(a,b))` — the undirected pair key shared by the site
/// dedup and pair-weight accumulation passes below.
fn order_pair(a: u32, b: u32) -> (u32, u32) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

/// One call/ref-edge "site" candidate, pre-dedup (§3.2, §13 R8). Plain
/// integers only — no borrowed path string — so the whole dedup pass
/// below is a sort over `Copy` data instead of a `HashMap` keyed by a
/// freshly allocated `String` per edge.
#[derive(Clone, Copy)]
struct Site {
    /// Dense per-path id (see `node_file_id`/`path_ranges` below), not
    /// the writer's own file id — call edges don't have one of those,
    /// only a caller `sym_idx`, so both edge kinds are translated into
    /// this shared id space up front.
    file_id: u32,
    line: u32,
    to_node: u32,
    /// Global processing order: call edges first (in `call_edges`
    /// order), then ref edges (in `ref_edges` order) — mirrors the old
    /// `HashMap::entry(..).or_insert(..)` "first write wins" rule (R8:
    /// call edges, processed first, win any collision with a ref edge
    /// at the same site) without needing a stable sort.
    seq: u32,
    from_node: u32,
    weight: u32,
}

/// Dense per-eligible-node and per-file lookup structures built once
/// from the canonically sorted eligible list (R6) — shared by the
/// call-edge and ref-edge site-collection passes
/// ([`collect_call_edge_sites`]/[`collect_ref_edge_sites`]) below. Every
/// field here replaces one `HashMap`/per-edge cost from the old
/// implementation (see `project_reference` in the test module, kept as
/// a byte-identical oracle) with an `O(1)` array index or a hash paid
/// once per distinct key instead of once per edge:
/// - `sym_to_node`: a `HashMap<u32, u32>` lookup for `sym_idx -> node
///   id` becomes indexing a dense `Vec<u32>` (`NOT_NODE`-filled);
/// - `node_line`/`path_ranges`: the per-node `HashMap<&str, Vec<(u32,
///   u32)>>` insert for ref-edge nearest-preceding attribution becomes
///   one array slice per distinct path, addressed by a dense file id;
/// - `by_name_in_file`/`eligible_by_name`: call-edge name resolution,
///   kept as `HashMap`s (a sorted-array + binary-search alternative was
///   tried and measured *slower*: with real identifier-shaped names,
///   each `partition_point` probe dereferences a different candidate's
///   string scattered across the heap, paying for a cache miss per
///   probe, where a `HashMap` touches the query string once to hash it
///   and the one matching stored string once to confirm it) — but
///   keyed by `(dense_file_id, name)` instead of `(path, name)`, one
///   string hash per lookup instead of two;
/// - `path_of_sym`: `sym_idx -> path` over *every* symbol (eligible or
///   not), in the writer's original order, last-write-wins — the one
///   piece of the old implementation kept almost as-is (still
///   `O(symbol_count)`, never the bottleneck), just as a dense array
///   instead of a `HashMap` (no hashing at all). The oracle proptest
///   found that a call edge's caller path must be resolved exactly
///   this way — by `sym_idx`, independent of which *node* that
///   `sym_idx` happens to resolve to — for byte-identical output when
///   a garbage input lets two symbols share one `sym_idx` (see
///   `regression_duplicate_sym_idx_*` below for the pinned cases).
struct DenseIndex<'a> {
    /// Dense node id -> `sym_idx`, canonical order — also `mod.rs`'s
    /// `finalize` input via [`ProjectedGraph::node_sym_idx`].
    node_sym_idx: Vec<u32>,
    /// The symbols array's own indices, in the same canonical node
    /// order as `node_sym_idx` — lets the call-edge pass read a node's
    /// own path straight back out of `input.symbols` for the `ptr::eq`
    /// fast-path check, with no separate `Vec<&str>` duplicating it.
    eligible_indices: Vec<usize>,
    sym_to_node: Vec<u32>,
    node_line: Vec<u32>,
    node_file_id: Vec<u32>,
    /// dense_file_id -> `[start, end)` in node-id space.
    path_ranges: Vec<(u32, u32)>,
    /// Grown by [`collect_call_edge_sites`] the first time a caller's
    /// `path_of_sym` path doesn't match any eligible symbol's path (a
    /// `sym_idx` collision with an ineligible symbol) — every other
    /// reader only ever looks a path up, never relies on its length.
    path_to_dense: HashMap<&'a str, u32>,
    by_name_in_file: HashMap<(u32, &'a str), u32>,
    eligible_by_name: HashMap<&'a str, Vec<u32>>,
    path_of_sym: Vec<Option<&'a str>>,
    /// `from_file_id -> dense_file_id`, snapshotted once right after
    /// the main per-node loop (before `path_to_dense` can grow further)
    /// — ref edges only ever care about paths eligible symbols live in.
    from_file_dense: Vec<Option<u32>>,
}

impl<'a> DenseIndex<'a> {
    fn get_node(&self, sym_idx: u32) -> Option<u32> {
        match self.sym_to_node.get(sym_idx as usize) {
            Some(&node_id) if node_id != NOT_NODE => Some(node_id),
            _ => None,
        }
    }
}

/// R6 canonical order: eligible symbol indices into `symbols`, sorted
/// by `(path, line, kind, name, sym_idx)`.
fn canonical_eligible_order(symbols: &[ProjectionSymbol<'_>]) -> Vec<usize> {
    let mut eligible_indices: Vec<usize> = (0..symbols.len())
        .filter(|&i| is_eligible(symbols[i].kind, symbols[i].language))
        .collect();
    eligible_indices.sort_unstable_by(|&a, &b| {
        let sa = &symbols[a];
        let sb = &symbols[b];
        sa.path
            .cmp(sb.path)
            .then(sa.line.cmp(&sb.line))
            .then(sa.kind.cmp(&sb.kind))
            .then(sa.name.cmp(sb.name))
            .then(sa.sym_idx.cmp(&sb.sym_idx))
    });
    eligible_indices
}

/// `max(declared symbol_count, every symbol's sym_idx + 1)` — sizes
/// every dense array in [`DenseIndex`]. `saturating_add` (not `+`, the
/// reference implementation's own arithmetic, kept verbatim there):
/// a `sym_idx` of `u32::MAX` is already nonsensical input no real
/// writer produces, but production `project` must degrade rather than
/// panic/wrap on it, same posture as every other out-of-range `sym_idx`
/// in this module.
fn effective_symbol_count(input: &ProjectionInput<'_>) -> u32 {
    input.symbol_count.max(
        input
            .symbols
            .iter()
            .map(|s| s.sym_idx.saturating_add(1))
            .max()
            .unwrap_or(0),
    )
}

/// Builds every [`DenseIndex`] field in one pass over the already
/// canonically ordered `eligible_indices` (consumed, not cloned — moved
/// in from [`canonical_eligible_order`]'s caller), plus the two
/// `O(symbol_count)`/`O(file_paths.len())` passes that don't depend on
/// node order (`path_of_sym`, `from_file_dense`).
fn build_dense_index<'a>(
    input: &ProjectionInput<'a>,
    eligible_indices: Vec<usize>,
    symbol_count: u32,
) -> DenseIndex<'a> {
    let node_sym_idx: Vec<u32> = eligible_indices
        .iter()
        .map(|&i| input.symbols[i].sym_idx)
        .collect();

    let mut sym_to_node = vec![NOT_NODE; symbol_count as usize];
    for (node_id, &sym_idx) in node_sym_idx.iter().enumerate() {
        if let Some(slot) = sym_to_node.get_mut(sym_idx as usize) {
            *slot = node_id as u32;
        }
    }

    let mut node_line: Vec<u32> = Vec::with_capacity(eligible_indices.len());
    let mut node_file_id: Vec<u32> = Vec::with_capacity(eligible_indices.len());
    let mut path_ranges: Vec<(u32, u32)> = Vec::new();
    let mut path_to_dense: HashMap<&str, u32> = HashMap::new();
    let mut by_name_in_file: HashMap<(u32, &str), u32> = HashMap::new();
    let mut eligible_by_name: HashMap<&str, Vec<u32>> = HashMap::new();

    let mut current_path: Option<&str> = None;
    let mut current_dense: u32 = 0;
    for (node_id, &idx) in eligible_indices.iter().enumerate() {
        let s = &input.symbols[idx];
        node_line.push(s.line);
        if current_path != Some(s.path) {
            current_dense = path_ranges.len() as u32;
            path_ranges.push((node_id as u32, node_id as u32 + 1));
            path_to_dense.insert(s.path, current_dense);
            current_path = Some(s.path);
        } else {
            path_ranges[current_dense as usize].1 = node_id as u32 + 1;
        }
        node_file_id.push(current_dense);

        eligible_by_name.entry(s.name).or_default().push(s.sym_idx);
        by_name_in_file
            .entry((current_dense, s.name))
            .and_modify(|cur| *cur = (*cur).min(s.sym_idx))
            .or_insert(s.sym_idx);
    }

    let from_file_dense: Vec<Option<u32>> = input
        .file_paths
        .iter()
        .map(|p| path_to_dense.get(p.as_str()).copied())
        .collect();

    let mut path_of_sym: Vec<Option<&str>> = vec![None; symbol_count as usize];
    for s in input.symbols {
        if let Some(slot) = path_of_sym.get_mut(s.sym_idx as usize) {
            *slot = Some(s.path);
        }
    }

    DenseIndex {
        node_sym_idx,
        eligible_indices,
        sym_to_node,
        node_line,
        node_file_id,
        path_ranges,
        path_to_dense,
        by_name_in_file,
        eligible_by_name,
        path_of_sym,
        from_file_dense,
    }
}

/// Resolves every `call_edges` entry to a [`Site`] (§3.2 call-name
/// rule, R7 "eligible candidates only"), appending to `sites` in order
/// with an increasing `seq` — call edges must run before
/// [`collect_ref_edge_sites`] so R8 ("call edges win any site
/// collision") holds via `seq` alone.
fn collect_call_edge_sites<'a>(
    input: &ProjectionInput<'a>,
    index: &mut DenseIndex<'a>,
    seq: &mut u32,
    sites: &mut Vec<Site>,
) {
    for e in input.call_edges {
        let Some(from_node) = index.get_node(e.caller_sym_idx) else {
            continue; // caller itself ineligible
        };
        // The site key's file component — like the reference
        // implementation's `path_of_sym.get(&caller_sym_idx)` — is
        // keyed by `caller_sym_idx` through the *original* per-symbol
        // path, not through the already-resolved `from_node`'s own
        // canonical path. These normally agree; they can only diverge
        // when `sym_idx` values collide across symbols (garbage input,
        // §13 R7 territory), in which case byte-identical output means
        // matching whichever path the reference implementation's
        // `HashMap` happened to retain (last write in original order).
        let Some(caller_path) = index
            .path_of_sym
            .get(e.caller_sym_idx as usize)
            .copied()
            .flatten()
        else {
            continue;
        };
        // Fast path (every real writer input, `sym_idx` unique): the
        // `path_of_sym` result IS this node's own path — literally the
        // same `&str` (`ptr::eq`, not a byte-by-byte compare, let alone
        // a hash) — so reuse the already-computed `node_file_id` with
        // zero hashing. Only a `sym_idx` collision (garbage input) can
        // make `caller_path` point elsewhere, in which case fall back
        // to `path_to_dense` (hash `caller_path`, growing the id space
        // the first time it doesn't match any *eligible* symbol's path
        // — same as the reference implementation's `by_name_in_file`
        // simply having no entry for a path with no eligible symbols).
        let node_own_path = input.symbols[index.eligible_indices[from_node as usize]].path;
        let dense_id = if std::ptr::eq(caller_path, node_own_path) {
            index.node_file_id[from_node as usize]
        } else {
            match index.path_to_dense.get(caller_path) {
                Some(&id) => id,
                None => {
                    let id = index.path_to_dense.len() as u32;
                    index.path_to_dense.insert(caller_path, id);
                    id
                }
            }
        };
        let target = index
            .by_name_in_file
            .get(&(dense_id, e.callee_name))
            .copied()
            .or_else(|| {
                index
                    .eligible_by_name
                    .get(e.callee_name)
                    .filter(|cands| cands.len() == 1)
                    .map(|cands| cands[0])
            });
        let Some(target_sym) = target else { continue };
        let Some(to_node) = index.get_node(target_sym) else {
            continue;
        };
        if to_node == from_node {
            continue; // self-loop
        }
        sites.push(Site {
            file_id: dense_id,
            line: e.line,
            to_node,
            seq: *seq,
            from_node,
            weight: CALL_WEIGHT,
        });
        *seq += 1;
    }
}

/// Resolves every non-ambiguous `ref_edges` entry to a [`Site`] via
/// nearest-preceding-symbol attribution (§3.2 F8), appending to `sites`
/// in order with an increasing `seq` continuing from
/// [`collect_call_edge_sites`].
fn collect_ref_edge_sites<'a>(
    input: &ProjectionInput<'a>,
    index: &DenseIndex<'a>,
    seq: &mut u32,
    sites: &mut Vec<Site>,
) {
    for (idx, e) in input.ref_edges.iter().enumerate() {
        if input.ambiguous.get(idx).copied().unwrap_or(false) {
            continue; // R7: ambiguous resolutions are not edges
        }
        let Some(to_node) = index.get_node(e.to_sym_idx) else {
            continue;
        };
        let Some(Some(dense_id)) = index.from_file_dense.get(e.from_file_id as usize).copied()
        else {
            continue; // no eligible symbol in this file at all (or out-of-range file id)
        };
        let (start, end) = index.path_ranges[dense_id as usize];
        // Nearest-preceding: greatest node with line <= e.line.
        let pos =
            index.node_line[start as usize..end as usize].partition_point(|&line| line <= e.line);
        if pos == 0 {
            continue; // before the first eligible symbol in the file
        }
        // The oracle resolves nearest-preceding to a `sym_idx` (via its
        // `by_path: HashMap<&str, Vec<(line, sym_idx)>>`) and *then*
        // maps that `sym_idx` back to a node through `sym_to_node` — a
        // second indirection that matters only when two symbols share
        // one `sym_idx` (writer-bug/garbage input territory, exercised
        // by the oracle proptest): the node at this file position and
        // the node `sym_to_node` resolves its `sym_idx` to can differ.
        // Replicated exactly here rather than shortcut to `start + pos
        // - 1` directly, so a duplicate `sym_idx` degrades identically.
        let pos_node = start + pos as u32 - 1;
        let from_sym = index.node_sym_idx[pos_node as usize];
        let Some(from_node) = index.get_node(from_sym) else {
            continue;
        };
        if from_node == to_node {
            continue; // self-loop
        }
        let weight = if e.kind == REF_KIND_CALL {
            REF_CALL_WEIGHT
        } else {
            REF_OTHER_WEIGHT
        };
        sites.push(Site {
            file_id: dense_id,
            line: e.line,
            to_node,
            seq: *seq,
            from_node,
            weight,
        });
        *seq += 1;
    }
}

/// Dedups `sites` by `(file_id, line, to_node)` (keeping the lowest
/// `seq` in each run, R8), folds in `hierarchy_edges`, and caps the
/// result at [`PAIR_CAP`] — the final, canonically sorted pair list
/// [`project`] hands to [`LeidenGraph::from_pairs`].
fn accumulate_pairs(
    mut sites: Vec<Site>,
    hierarchy_edges: &[ProjectionHierarchyEdge],
    index: &DenseIndex<'_>,
) -> Vec<(u32, u32, u32)> {
    // Dedup sites by (file_id, line, to_node), keeping the lowest `seq`
    // in each run. `seq` as the last sort key makes each run's first
    // element deterministic (the earliest-processed site), reproducing
    // the old `HashMap::entry(..).or_insert(..)` "first write wins"
    // semantics without requiring a stable sort.
    sites.sort_unstable_by_key(|s| (s.file_id, s.line, s.to_node, s.seq));

    // Pair-weight accumulation: every deduped site plus every hierarchy
    // edge becomes an (min_node, max_node, weight) triple, summed by a
    // second sort + grouped-sum instead of a `HashMap<(u32, u32), u32>`.
    let mut pair_entries: Vec<(u32, u32, u32)> =
        Vec::with_capacity(sites.len() + hierarchy_edges.len());
    for group in
        sites.chunk_by(|a, b| (a.file_id, a.line, a.to_node) == (b.file_id, b.line, b.to_node))
    {
        // group[0] carries the lowest `seq` in this run (sort order).
        let site = group[0];
        let (a, b) = order_pair(site.from_node, site.to_node);
        pair_entries.push((a, b, site.weight));
    }

    for he in hierarchy_edges {
        let (Some(from_node), Some(to_node)) = (
            index.get_node(he.from_sym_idx),
            index.get_node(he.to_sym_idx),
        ) else {
            continue;
        };
        if from_node == to_node {
            continue;
        }
        let (a, b) = order_pair(from_node, to_node);
        pair_entries.push((a, b, HIERARCHY_WEIGHT));
    }

    // Cap and emit the canonical (sorted) pair list. Summation order is
    // irrelevant: every weight is non-negative, so a `saturating_add`
    // chain reaches the same saturated (or exact) total regardless of
    // grouping order.
    pair_entries.sort_unstable_by_key(|&(a, b, _)| (a, b));
    let mut pairs: Vec<(u32, u32, u32)> = Vec::new();
    for group in pair_entries.chunk_by(|x, y| (x.0, x.1) == (y.0, y.1)) {
        let (a, b, _) = group[0];
        let total = group
            .iter()
            .fold(0u32, |acc, &(_, _, w)| acc.saturating_add(w));
        pairs.push((a, b, total.min(PAIR_CAP)));
    }
    pairs
}

/// Turns the writer's resolved call/ref/hierarchy edges into the final
/// undirected weighted graph (§3.2). Byte-identical to a naive
/// `HashMap`-per-site implementation (see `project_reference` in the
/// test module below, verified by `project_matches_reference_oracle`
/// and `project_matches_reference_oracle_dense_collisions` over
/// arbitrary inputs) — see [`DenseIndex`]'s docs for what each piece
/// replaces and why. Pure orchestration: canonical order, build the
/// dense index, collect sites from both edge kinds in R8 order, then
/// dedup/accumulate/cap into the final pair list.
pub fn project(input: &ProjectionInput<'_>) -> ProjectedGraph {
    let eligible_indices = canonical_eligible_order(input.symbols);
    let symbol_count = effective_symbol_count(input);
    let n = eligible_indices.len() as u32;

    let mut index = build_dense_index(input, eligible_indices, symbol_count);

    let mut sites: Vec<Site> = Vec::with_capacity(input.call_edges.len() + input.ref_edges.len());
    let mut seq = 0u32;
    collect_call_edge_sites(input, &mut index, &mut seq, &mut sites);
    collect_ref_edge_sites(input, &index, &mut seq, &mut sites);

    let pairs = accumulate_pairs(sites, input.hierarchy_edges, &index);
    let graph = LeidenGraph::from_pairs(n, &pairs);

    ProjectedGraph {
        symbol_count,
        node_sym_idx: index.node_sym_idx,
        graph,
        pairs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym<'a>(sym_idx: u32, path: &'a str, line: u32, name: &'a str) -> ProjectionSymbol<'a> {
        ProjectionSymbol {
            sym_idx,
            path,
            line,
            kind: 0,
            name,
            language: Some(Language::Rust),
        }
    }

    /// Verbatim pre-optimization implementation of [`super::project`],
    /// kept as a correctness oracle: every `HashMap`-based site/pair
    /// dedup this module replaced with sorted integer keys must still
    /// produce byte-identical `ProjectedGraph`s (`project_matches_
    /// reference_oracle` below checks this over arbitrary inputs).
    fn project_reference(input: &ProjectionInput<'_>) -> ProjectedGraph {
        fn add_weight(pair_weight: &mut HashMap<(u32, u32), u32>, a: u32, b: u32, w: u32) {
            let key = if a < b { (a, b) } else { (b, a) };
            let entry = pair_weight.entry(key).or_insert(0);
            *entry = entry.saturating_add(w);
        }

        let mut eligible_indices: Vec<usize> = (0..input.symbols.len())
            .filter(|&i| is_eligible(input.symbols[i].kind, input.symbols[i].language))
            .collect();
        eligible_indices.sort_unstable_by(|&a, &b| {
            let sa = &input.symbols[a];
            let sb = &input.symbols[b];
            sa.path
                .cmp(sb.path)
                .then(sa.line.cmp(&sb.line))
                .then(sa.kind.cmp(&sb.kind))
                .then(sa.name.cmp(sb.name))
                .then(sa.sym_idx.cmp(&sb.sym_idx))
        });

        let node_sym_idx: Vec<u32> = eligible_indices
            .iter()
            .map(|&i| input.symbols[i].sym_idx)
            .collect();
        let n = node_sym_idx.len() as u32;

        let symbol_count = input.symbol_count.max(
            input
                .symbols
                .iter()
                .map(|s| s.sym_idx + 1)
                .max()
                .unwrap_or(0),
        );
        let mut sym_to_node: HashMap<u32, u32> = HashMap::with_capacity(eligible_indices.len());
        for (node_id, &sym_idx) in node_sym_idx.iter().enumerate() {
            sym_to_node.insert(sym_idx, node_id as u32);
        }

        let mut by_path: HashMap<&str, Vec<(u32, u32)>> = HashMap::new();
        for &i in &eligible_indices {
            let s = &input.symbols[i];
            by_path.entry(s.path).or_default().push((s.line, s.sym_idx));
        }

        let mut by_name_in_file: HashMap<(&str, &str), u32> = HashMap::new();
        let mut eligible_by_name: HashMap<&str, Vec<u32>> = HashMap::new();
        for &i in &eligible_indices {
            let s = &input.symbols[i];
            eligible_by_name.entry(s.name).or_default().push(s.sym_idx);
            by_name_in_file
                .entry((s.path, s.name))
                .and_modify(|cur| *cur = (*cur).min(s.sym_idx))
                .or_insert(s.sym_idx);
        }
        let mut path_of_sym: HashMap<u32, &str> = HashMap::new();
        for s in input.symbols {
            path_of_sym.insert(s.sym_idx, s.path);
        }

        let mut sites: HashMap<(String, u32, u32), (u32, u32)> = HashMap::new();

        for e in input.call_edges {
            let Some(&from_node) = sym_to_node.get(&e.caller_sym_idx) else {
                continue;
            };
            let Some(&caller_path) = path_of_sym.get(&e.caller_sym_idx) else {
                continue;
            };
            let target = by_name_in_file
                .get(&(caller_path, e.callee_name))
                .copied()
                .or_else(|| {
                    eligible_by_name
                        .get(e.callee_name)
                        .filter(|cands| cands.len() == 1)
                        .map(|cands| cands[0])
                });
            let Some(target_sym) = target else { continue };
            let Some(&to_node) = sym_to_node.get(&target_sym) else {
                continue;
            };
            if to_node == from_node {
                continue;
            }
            let key = (caller_path.to_string(), e.line, to_node);
            sites.entry(key).or_insert((from_node, CALL_WEIGHT));
        }

        for (idx, e) in input.ref_edges.iter().enumerate() {
            if input.ambiguous.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let Some(&to_node) = sym_to_node.get(&e.to_sym_idx) else {
                continue;
            };
            let Some(path) = input.file_paths.get(e.from_file_id as usize) else {
                continue;
            };
            let Some(members) = by_path.get(path.as_str()) else {
                continue;
            };
            let pos = members.partition_point(|&(line, _)| line <= e.line);
            if pos == 0 {
                continue;
            }
            let (_, from_sym) = members[pos - 1];
            let Some(&from_node) = sym_to_node.get(&from_sym) else {
                continue;
            };
            if from_node == to_node {
                continue;
            }
            let weight = if e.kind == REF_KIND_CALL {
                REF_CALL_WEIGHT
            } else {
                REF_OTHER_WEIGHT
            };
            let key = (path.clone(), e.line, to_node);
            sites.entry(key).or_insert((from_node, weight));
        }

        let mut pair_weight: HashMap<(u32, u32), u32> = HashMap::new();
        for ((_, _, to_node), (from_node, weight)) in sites {
            add_weight(&mut pair_weight, from_node, to_node, weight);
        }

        for he in input.hierarchy_edges {
            let (Some(&from_node), Some(&to_node)) = (
                sym_to_node.get(&he.from_sym_idx),
                sym_to_node.get(&he.to_sym_idx),
            ) else {
                continue;
            };
            if from_node == to_node {
                continue;
            }
            add_weight(&mut pair_weight, from_node, to_node, HIERARCHY_WEIGHT);
        }

        let mut pairs: Vec<(u32, u32, u32)> = pair_weight
            .into_iter()
            .map(|((a, b), w)| (a, b, w.min(PAIR_CAP)))
            .collect();
        pairs.sort_unstable_by_key(|&(a, b, _)| (a, b));

        let graph = LeidenGraph::from_pairs(n, &pairs);

        ProjectedGraph {
            symbol_count,
            node_sym_idx,
            graph,
            pairs,
        }
    }

    #[test]
    fn eligibility_excludes_module_heading_package() {
        assert!(!is_eligible(13, Some(Language::Rust))); // Module
        assert!(!is_eligible(12, Some(Language::Rust))); // Heading
        assert!(!is_eligible(11, Some(Language::Rust))); // Package
        assert!(is_eligible(0, Some(Language::Rust))); // Function
    }

    #[test]
    fn eligibility_excludes_markup_languages() {
        for lang in [
            Language::Markdown,
            Language::Yaml,
            Language::Toml,
            Language::Css,
            Language::Html,
        ] {
            assert!(!is_eligible(0, Some(lang)), "{lang:?} should be ineligible");
        }
        assert!(is_eligible(0, Some(Language::Python)));
        assert!(is_eligible(0, None));
    }

    #[test]
    fn call_edge_resolves_same_file_smallest() {
        let symbols = vec![
            sym(0, "src/a.rs", 1, "helper"),
            sym(1, "src/a.rs", 10, "helper"), // duplicate name, same file — smallest sym_idx wins
            sym(2, "src/a.rs", 20, "caller"),
        ];
        let call_edges = vec![ProjectionCallEdge {
            caller_sym_idx: 2,
            callee_name: "helper",
            line: 21,
        }];
        let input = ProjectionInput {
            symbol_count: 3,
            symbols: &symbols,
            call_edges: &call_edges,
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &[],
        };
        let out = project(&input);
        assert_eq!(out.pairs, vec![(0, 2, CALL_WEIGHT)]);
    }

    #[test]
    fn call_edge_drops_when_ambiguous_project_wide() {
        let symbols = vec![
            sym(0, "src/a.rs", 1, "helper"),
            sym(1, "src/b.rs", 1, "helper"), // two distinct files, no caller-file match
            sym(2, "src/c.rs", 1, "caller"),
        ];
        let call_edges = vec![ProjectionCallEdge {
            caller_sym_idx: 2,
            callee_name: "helper",
            line: 2,
        }];
        let input = ProjectionInput {
            symbol_count: 3,
            symbols: &symbols,
            call_edges: &call_edges,
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &[],
        };
        let out = project(&input);
        assert!(out.pairs.is_empty());
    }

    #[test]
    fn ref_edge_attributes_to_nearest_preceding_symbol() {
        let symbols = vec![
            sym(0, "src/a.rs", 1, "f1"),
            sym(1, "src/a.rs", 10, "f2"),
            sym(2, "src/b.rs", 1, "target"),
        ];
        let ref_edges = vec![ProjectionRefEdge {
            from_file_id: 0,
            line: 12, // after f2 (line 10), before any later symbol
            to_sym_idx: 2,
            kind: REF_KIND_CALL,
        }];
        let file_paths = vec!["src/a.rs".to_string()];
        let input = ProjectionInput {
            symbol_count: 3,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &ref_edges,
            ambiguous: &[false],
            hierarchy_edges: &[],
            file_paths: &file_paths,
        };
        let out = project(&input);
        assert_eq!(out.pairs, vec![(1, 2, REF_CALL_WEIGHT)]);
    }

    #[test]
    fn ref_edge_before_first_symbol_in_file_is_dropped() {
        let symbols = vec![
            sym(0, "src/a.rs", 10, "f1"),
            sym(1, "src/b.rs", 1, "target"),
        ];
        let ref_edges = vec![ProjectionRefEdge {
            from_file_id: 0,
            line: 1, // before f1 at line 10
            to_sym_idx: 1,
            kind: REF_KIND_CALL,
        }];
        let file_paths = vec!["src/a.rs".to_string()];
        let input = ProjectionInput {
            symbol_count: 2,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &ref_edges,
            ambiguous: &[false],
            hierarchy_edges: &[],
            file_paths: &file_paths,
        };
        let out = project(&input);
        assert!(out.pairs.is_empty());
    }

    #[test]
    fn ambiguous_ref_edge_is_dropped() {
        let symbols = vec![sym(0, "src/a.rs", 1, "f1"), sym(1, "src/b.rs", 1, "target")];
        let ref_edges = vec![ProjectionRefEdge {
            from_file_id: 0,
            line: 2,
            to_sym_idx: 1,
            kind: REF_KIND_CALL,
        }];
        let file_paths = vec!["src/a.rs".to_string()];
        let input = ProjectionInput {
            symbol_count: 2,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &ref_edges,
            ambiguous: &[true],
            hierarchy_edges: &[],
            file_paths: &file_paths,
        };
        let out = project(&input);
        assert!(out.pairs.is_empty());
    }

    #[test]
    fn dedup_call_and_ref_edge_at_same_site_counts_once() {
        let symbols = vec![
            sym(0, "src/a.rs", 1, "caller"),
            sym(1, "src/b.rs", 1, "target"),
        ];
        let call_edges = vec![ProjectionCallEdge {
            caller_sym_idx: 0,
            callee_name: "target",
            line: 5,
        }];
        let ref_edges = vec![ProjectionRefEdge {
            from_file_id: 0,
            line: 5,
            to_sym_idx: 1,
            kind: REF_KIND_CALL,
        }];
        let file_paths = vec!["src/a.rs".to_string()];
        let input = ProjectionInput {
            symbol_count: 2,
            symbols: &symbols,
            call_edges: &call_edges,
            ref_edges: &ref_edges,
            ambiguous: &[false],
            hierarchy_edges: &[],
            file_paths: &file_paths,
        };
        let out = project(&input);
        assert_eq!(
            out.pairs,
            vec![(0, 1, CALL_WEIGHT)],
            "same (file,line,to) site must count once"
        );
    }

    #[test]
    fn pair_cap_limits_accumulated_weight() {
        // Names materialized up front (not inline `&format!(...)`) so the
        // borrows `sym()` hands back into `symbols` outlive this function.
        let names: Vec<String> = (0..20u32).map(|i| format!("t{i}")).collect();
        let mut symbols = vec![sym(0, "src/a.rs", 1, "caller")];
        let mut ref_edges = Vec::new();
        for i in 0..20u32 {
            symbols.push(sym(i + 1, "src/b.rs", i + 1, &names[i as usize]));
        }
        // All ref edges target the SAME symbol (sym_idx 1) from many
        // distinct lines so they are NOT deduped, to exercise the cap.
        for line in 0..20u32 {
            ref_edges.push(ProjectionRefEdge {
                from_file_id: 0,
                line: line + 1,
                to_sym_idx: 1,
                kind: REF_KIND_CALL,
            });
        }
        let file_paths = vec!["src/a.rs".to_string()];
        let ambiguous = vec![false; ref_edges.len()];
        let input = ProjectionInput {
            symbol_count: 21,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &ref_edges,
            ambiguous: &ambiguous,
            hierarchy_edges: &[],
            file_paths: &file_paths,
        };
        let out = project(&input);
        let (_, _, w) = out
            .pairs
            .iter()
            .find(|&&(a, b, _)| a == 0 || b == 0)
            .copied()
            .expect("pair present");
        assert_eq!(w, PAIR_CAP);
    }

    #[test]
    fn hierarchy_edge_weight_is_one() {
        let symbols = vec![
            sym(0, "src/a.rs", 1, "child"),
            sym(1, "src/a.rs", 2, "parent"),
        ];
        let hierarchy_edges = vec![ProjectionHierarchyEdge {
            from_sym_idx: 0,
            to_sym_idx: 1,
        }];
        let input = ProjectionInput {
            symbol_count: 2,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &hierarchy_edges,
            file_paths: &[],
        };
        let out = project(&input);
        assert_eq!(out.pairs, vec![(0, 1, HIERARCHY_WEIGHT)]);
    }

    #[test]
    fn self_loop_is_dropped_for_every_edge_source() {
        let symbols = vec![sym(0, "src/a.rs", 1, "f")];
        let call_edges = vec![ProjectionCallEdge {
            caller_sym_idx: 0,
            callee_name: "f",
            line: 1,
        }];
        let hierarchy_edges = vec![ProjectionHierarchyEdge {
            from_sym_idx: 0,
            to_sym_idx: 0,
        }];
        let input = ProjectionInput {
            symbol_count: 1,
            symbols: &symbols,
            call_edges: &call_edges,
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &hierarchy_edges,
            file_paths: &[],
        };
        let out = project(&input);
        assert!(out.pairs.is_empty());
    }

    #[test]
    fn canonical_order_is_independent_of_input_sym_idx_order() {
        // Same symbols, but sym_idx assigned in the REVERSE of canonical
        // (path, line) order — node ids must still come out in
        // ascending (path, line) order (R6).
        let symbols = vec![
            sym(5, "src/a.rs", 1, "f1"),
            sym(2, "src/a.rs", 2, "f2"),
            sym(9, "src/b.rs", 1, "f3"),
        ];
        let input = ProjectionInput {
            symbol_count: 10,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &[],
        };
        let out = project(&input);
        assert_eq!(out.node_sym_idx, vec![5, 2, 9]);
    }

    // -----------------------------------------------------------------
    // Oracle-equivalence proptest: `project` must produce a
    // byte-identical `ProjectedGraph` (symbol_count, node_sym_idx,
    // pairs) to `project_reference` (the pre-optimization HashMap-based
    // implementation) over arbitrary small `ProjectionInput`s — same
    // generator shapes as `super::super::tests::
    // project_and_cluster_never_panic_on_arbitrary_input`.
    // -----------------------------------------------------------------

    /// Owned mirror of [`ProjectionSymbol`] — `ProjectionSymbol` itself
    /// borrows `path`/`name` as `&str`, which a `prop_map` closure
    /// cannot return directly. Materialized as an ordinary owned local
    /// that outlives the whole test body so `as_projection` can validly
    /// borrow from it.
    #[derive(Debug, Clone)]
    struct OwnedSymbol {
        sym_idx: u32,
        path: String,
        line: u32,
        kind: u8,
        name: String,
        language: Option<Language>,
    }

    impl OwnedSymbol {
        fn as_projection(&self) -> ProjectionSymbol<'_> {
            ProjectionSymbol {
                sym_idx: self.sym_idx,
                path: &self.path,
                line: self.line,
                kind: self.kind,
                name: &self.name,
                language: self.language,
            }
        }
    }

    fn arb_symbol() -> impl proptest::strategy::Strategy<Value = OwnedSymbol> {
        use proptest::prelude::*;
        (
            0u32..30,
            "[a-z/]{0,12}",
            0u32..2000,
            0u8..20,
            "[a-zA-Z_]{0,8}",
            proptest::option::of(proptest::sample::select(vec![
                Language::Rust,
                Language::Markdown,
                Language::Python,
            ])),
        )
            .prop_map(|(sym_idx, path, line, kind, name, language)| OwnedSymbol {
                sym_idx,
                path,
                line,
                kind,
                name,
                language,
            })
    }

    #[derive(Debug, Clone)]
    struct OwnedCallEdge {
        caller_sym_idx: u32,
        callee_name: String,
        line: u32,
    }

    impl OwnedCallEdge {
        fn as_projection(&self) -> ProjectionCallEdge<'_> {
            ProjectionCallEdge {
                caller_sym_idx: self.caller_sym_idx,
                callee_name: &self.callee_name,
                line: self.line,
            }
        }
    }

    fn arb_call_edge() -> impl proptest::strategy::Strategy<Value = OwnedCallEdge> {
        use proptest::prelude::*;
        (0u32..35, "[a-zA-Z_]{0,8}", 0u32..2000).prop_map(|(caller_sym_idx, callee_name, line)| {
            OwnedCallEdge {
                caller_sym_idx,
                callee_name,
                line,
            }
        })
    }

    fn arb_ref_edge() -> impl proptest::strategy::Strategy<Value = ProjectionRefEdge> {
        use proptest::prelude::*;
        (0u32..12, 0u32..2000, 0u32..35, 0u8..6).prop_map(
            |(from_file_id, line, to_sym_idx, kind)| ProjectionRefEdge {
                from_file_id,
                line,
                to_sym_idx,
                kind,
            },
        )
    }

    fn arb_hierarchy_edge() -> impl proptest::strategy::Strategy<Value = ProjectionHierarchyEdge> {
        use proptest::prelude::*;
        (0u32..35, 0u32..35).prop_map(|(from_sym_idx, to_sym_idx)| ProjectionHierarchyEdge {
            from_sym_idx,
            to_sym_idx,
        })
    }

    proptest::proptest! {
        #[test]
        fn project_matches_reference_oracle(
            symbols in proptest::collection::vec(arb_symbol(), 0..15),
            call_edges in proptest::collection::vec(arb_call_edge(), 0..15),
            ref_edges in proptest::collection::vec(arb_ref_edge(), 0..15),
            // Deliberately independent of `ref_edges.len()` — exercises
            // the "mismatched ambiguous length" case too. Arbitrary
            // per-element bools (not a fixed alternating pattern) so
            // every true/false run length and position gets shrunk and
            // explored, not just evens.
            ambiguous in proptest::collection::vec(proptest::prelude::any::<bool>(), 0..18),
            hierarchy_edges in proptest::collection::vec(arb_hierarchy_edge(), 0..10),
            file_paths in proptest::collection::vec("[a-z/]{0,10}", 0..8),
            declared_symbol_count in 0u32..20,
        ) {
            let projection_symbols: Vec<ProjectionSymbol<'_>> =
                symbols.iter().map(OwnedSymbol::as_projection).collect();
            let projection_call_edges: Vec<ProjectionCallEdge<'_>> =
                call_edges.iter().map(OwnedCallEdge::as_projection).collect();

            let input = ProjectionInput {
                symbol_count: declared_symbol_count,
                symbols: &projection_symbols,
                call_edges: &projection_call_edges,
                ref_edges: &ref_edges,
                ambiguous: &ambiguous,
                hierarchy_edges: &hierarchy_edges,
                file_paths: &file_paths,
            };

            let fast = project(&input);
            let oracle = project_reference(&input);

            proptest::prop_assert_eq!(fast.symbol_count, oracle.symbol_count);
            proptest::prop_assert_eq!(&fast.node_sym_idx, &oracle.node_sym_idx);
            proptest::prop_assert_eq!(&fast.pairs, &oracle.pairs);
        }
    }

    // -----------------------------------------------------------------
    // Collision-dense proptest variant: `arb_symbol`/`arb_call_edge`/
    // `arb_ref_edge`/`arb_hierarchy_edge` above draw `sym_idx`/line/name
    // from wide ranges, so two edges landing on the exact same
    // `(file, line, to)` site, or two pairs landing on the exact same
    // `(a, b)` node pair enough times to hit `PAIR_CAP`, are rare.
    // These `_dense` generators draw from a tiny universe (a handful of
    // `sym_idx`s, two file paths, two names) instead, so with enough
    // edges per case both PAIR_CAP saturation and call-vs-ref-edge site
    // precedence (R8) are exercised on nearly every run, not just the
    // occasional shrink.
    // -----------------------------------------------------------------

    fn arb_symbol_dense() -> impl proptest::strategy::Strategy<Value = OwnedSymbol> {
        use proptest::prelude::*;
        (
            0u32..4,
            proptest::sample::select(vec!["a".to_string(), "b".to_string()]),
            0u32..3,
            Just(0u8), // always Function — eligibility isn't what this variant stresses
            proptest::sample::select(vec!["f".to_string(), "g".to_string()]),
            Just(Some(Language::Rust)),
        )
            .prop_map(|(sym_idx, path, line, kind, name, language)| OwnedSymbol {
                sym_idx,
                path,
                line,
                kind,
                name,
                language,
            })
    }

    fn arb_call_edge_dense() -> impl proptest::strategy::Strategy<Value = OwnedCallEdge> {
        use proptest::prelude::*;
        (
            0u32..4,
            proptest::sample::select(vec!["f".to_string(), "g".to_string()]),
            0u32..3,
        )
            .prop_map(|(caller_sym_idx, callee_name, line)| OwnedCallEdge {
                caller_sym_idx,
                callee_name,
                line,
            })
    }

    fn arb_ref_edge_dense() -> impl proptest::strategy::Strategy<Value = ProjectionRefEdge> {
        use proptest::prelude::*;
        (0u32..3, 0u32..3, 0u32..4, 0u8..3).prop_map(|(from_file_id, line, to_sym_idx, kind)| {
            ProjectionRefEdge {
                from_file_id,
                line,
                to_sym_idx,
                kind,
            }
        })
    }

    fn arb_hierarchy_edge_dense(
    ) -> impl proptest::strategy::Strategy<Value = ProjectionHierarchyEdge> {
        use proptest::prelude::*;
        (0u32..4, 0u32..4).prop_map(|(from_sym_idx, to_sym_idx)| ProjectionHierarchyEdge {
            from_sym_idx,
            to_sym_idx,
        })
    }

    proptest::proptest! {
        #[test]
        fn project_matches_reference_oracle_dense_collisions(
            symbols in proptest::collection::vec(arb_symbol_dense(), 0..8),
            call_edges in proptest::collection::vec(arb_call_edge_dense(), 0..40),
            ref_edges in proptest::collection::vec(arb_ref_edge_dense(), 0..40),
            ambiguous in proptest::collection::vec(proptest::prelude::any::<bool>(), 0..40),
            hierarchy_edges in proptest::collection::vec(arb_hierarchy_edge_dense(), 0..20),
            file_paths in proptest::collection::vec(
                proptest::sample::select(vec!["a".to_string(), "b".to_string(), String::new()]),
                0..4,
            ),
            declared_symbol_count in 0u32..6,
        ) {
            let projection_symbols: Vec<ProjectionSymbol<'_>> =
                symbols.iter().map(OwnedSymbol::as_projection).collect();
            let projection_call_edges: Vec<ProjectionCallEdge<'_>> =
                call_edges.iter().map(OwnedCallEdge::as_projection).collect();

            let input = ProjectionInput {
                symbol_count: declared_symbol_count,
                symbols: &projection_symbols,
                call_edges: &projection_call_edges,
                ref_edges: &ref_edges,
                ambiguous: &ambiguous,
                hierarchy_edges: &hierarchy_edges,
                file_paths: &file_paths,
            };

            let fast = project(&input);
            let oracle = project_reference(&input);

            proptest::prop_assert_eq!(fast.symbol_count, oracle.symbol_count);
            proptest::prop_assert_eq!(&fast.node_sym_idx, &oracle.node_sym_idx);
            proptest::prop_assert_eq!(&fast.pairs, &oracle.pairs);
        }
    }

    // -----------------------------------------------------------------
    // Regressions: `project_matches_reference_oracle` shrunk these three
    // cases out of the arbitrary-input space before the `DenseIndex`
    // fixes above landed — every one involves two symbols sharing one
    // `sym_idx` (§13 R7 "garbage input" territory no real writer
    // produces, but exactly what the oracle proptest exists to find).
    // Pinned here as deterministic tests instead of in
    // `proptest-regressions/` (no other `src/` unit test in this repo
    // keeps a proptest regression file; `cargo nextest run` already
    // reruns these as ordinary `#[test]`s on every invocation).
    // -----------------------------------------------------------------

    #[test]
    fn regression_duplicate_sym_idx_ref_edge_nearest_preceding() {
        // Two *eligible* symbols share sym_idx 18 ("/" and "a"). The
        // reference implementation resolves ref-edge nearest-preceding
        // attribution to a `sym_idx` (via `by_path`) and *then* maps
        // that `sym_idx` back to a node through `sym_to_node` — for a
        // duplicate `sym_idx`, that re-map can land on the *other*
        // symbol's node, not the one physically at the file position.
        let symbols = vec![
            ProjectionSymbol {
                sym_idx: 18,
                path: "/",
                line: 0,
                kind: 0,
                name: "",
                language: None,
            },
            ProjectionSymbol {
                sym_idx: 18,
                path: "a",
                line: 0,
                kind: 0,
                name: "",
                language: None,
            },
        ];
        let ref_edges = vec![
            ProjectionRefEdge {
                from_file_id: 0,
                line: 0,
                to_sym_idx: 0,
                kind: 0,
            },
            ProjectionRefEdge {
                from_file_id: 3,
                line: 0,
                to_sym_idx: 18,
                kind: 0,
            },
        ];
        let file_paths = vec![String::new(), String::new(), String::new(), "/".to_string()];
        let input = ProjectionInput {
            symbol_count: 0,
            symbols: &symbols,
            call_edges: &[],
            ref_edges: &ref_edges,
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &file_paths,
        };
        let fast = project(&input);
        let oracle = project_reference(&input);
        assert_eq!(fast.symbol_count, oracle.symbol_count);
        assert_eq!(fast.node_sym_idx, oracle.node_sym_idx);
        assert_eq!(fast.pairs, oracle.pairs);
    }

    #[test]
    fn regression_duplicate_sym_idx_eligible_and_ineligible_caller_path() {
        // sym_idx 13 is shared by an eligible symbol (path "/") and a
        // later, ineligible one (path "//", kind 11 = Package) — the
        // reference implementation's `path_of_sym` is last-write-wins
        // over *every* symbol regardless of eligibility, so the call
        // edge's caller path resolves to the ineligible symbol's path.
        // sym_idx 0 is also duplicated (two distinct eligible symbols)
        // to additionally exercise `sym_to_node`'s own last-write-wins.
        let symbols = vec![
            ProjectionSymbol {
                sym_idx: 13,
                path: "/",
                line: 0,
                kind: 0,
                name: "a",
                language: None,
            },
            ProjectionSymbol {
                sym_idx: 13,
                path: "//",
                line: 0,
                kind: 11, // Package: ineligible
                name: "",
                language: None,
            },
            ProjectionSymbol {
                sym_idx: 0,
                path: "/",
                line: 0,
                kind: 0,
                name: "",
                language: None,
            },
            ProjectionSymbol {
                sym_idx: 0,
                path: "",
                line: 0,
                kind: 0,
                name: "",
                language: None,
            },
        ];
        let call_edges = vec![ProjectionCallEdge {
            caller_sym_idx: 13,
            callee_name: "",
            line: 0,
        }];
        let input = ProjectionInput {
            symbol_count: 0,
            symbols: &symbols,
            call_edges: &call_edges,
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &[],
        };
        let fast = project(&input);
        let oracle = project_reference(&input);
        assert_eq!(fast.symbol_count, oracle.symbol_count);
        assert_eq!(fast.node_sym_idx, oracle.node_sym_idx);
        assert_eq!(fast.pairs, oracle.pairs);
    }

    #[test]
    fn regression_duplicate_sym_idx_call_edge_caller_path_via_ineligible_symbol() {
        // sym_idx 6 is shared by an eligible symbol (path "", kind 0)
        // and a later, ineligible one (path "/r", Markdown) — same root
        // cause as the test above, minimal-shrunk to a single call edge
        // whose target happens to be the caller's own eligible node,
        // which the bug's bogus caller path turned into a false
        // self-loop (dropped) instead of the real cross-file edge.
        let symbols = vec![
            ProjectionSymbol {
                sym_idx: 0,
                path: "/r",
                line: 0,
                kind: 0,
                name: "",
                language: None,
            },
            ProjectionSymbol {
                sym_idx: 6,
                path: "",
                line: 0,
                kind: 0,
                name: "",
                language: None,
            },
            ProjectionSymbol {
                sym_idx: 6,
                path: "/r",
                line: 0,
                kind: 0,
                name: "",
                language: Some(Language::Markdown), // ineligible language
            },
        ];
        let call_edges = vec![ProjectionCallEdge {
            caller_sym_idx: 6,
            callee_name: "",
            line: 0,
        }];
        let input = ProjectionInput {
            symbol_count: 0,
            symbols: &symbols,
            call_edges: &call_edges,
            ref_edges: &[],
            ambiguous: &[],
            hierarchy_edges: &[],
            file_paths: &[],
        };
        let fast = project(&input);
        let oracle = project_reference(&input);
        assert_eq!(fast.symbol_count, oracle.symbol_count);
        assert_eq!(fast.node_sym_idx, oracle.node_sym_idx);
        assert_eq!(fast.pairs, oracle.pairs);
    }
}
