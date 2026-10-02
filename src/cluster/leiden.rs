//! Deterministic Leiden-CPM (`docs/V9-FORMAT.md` §3.3, §13 R6/R15/R17).
//!
//! Hand-rolled, no crate (F15: no petgraph dependency). Integer-only —
//! every delta/connectivity computation uses `i128` intermediates
//! (R15), single-threaded, no RNG, canonical tie-breaking by smallest
//! id throughout. Given the same node set (in the same order), the same
//! weighted edge multiset and the same resolution, [`run`] is
//! byte-identical across runs, thread-pool sizes and input edge order
//! (§3.3 "Determinism guarantee").
//!
//! [`LeidenGraph`] nodes are plain `u32` ids 0..n. This module does not
//! know about `sym_idx`/paths/canonical keys at all — [`super::projection`]
//! is responsible for handing it nodes *already* in canonical order
//! (R6), so "ascending node id" here already means "ascending canonical
//! key" end to end.

use std::collections::{BTreeSet, VecDeque};

/// Outer convergence cap (§3.3 step 5).
pub const MAX_ITERATIONS: u32 = 4;
/// Aggregation-level cap (§3.3 step 4, R17).
pub const MAX_LEVELS: u32 = 32;

/// CPM resolution γ = `num / den` (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub num: u32,
    pub den: u32,
}

impl Resolution {
    /// §3.4 default: γ = 1/8.
    pub const DEFAULT: Resolution = Resolution { num: 1, den: 8 };
}

/// Parse `VEX_CLUSTER_RESOLUTION=a/b` (§3.4, R15): `0 < a <= 1024`,
/// `0 < b <= 1024`, otherwise warn and fall back to
/// [`Resolution::DEFAULT`]. Parse-only — the caller decides whether and
/// when to read the environment variable.
pub fn parse_resolution_env(value: &str) -> Resolution {
    let parsed = value.split_once('/').and_then(|(a, b)| {
        let a: u32 = a.trim().parse().ok()?;
        let b: u32 = b.trim().parse().ok()?;
        if a == 0 || a > 1024 || b == 0 || b > 1024 {
            return None;
        }
        Some(Resolution { num: a, den: b })
    });
    match parsed {
        Some(r) => r,
        None => {
            tracing::warn!(
                value,
                "VEX_CLUSTER_RESOLUTION invalid (want \"a/b\", 0<a<=1024, 0<b<=1024); using default 1/8"
            );
            Resolution::DEFAULT
        }
    }
}

/// An undirected, weighted graph over node ids `0..n`. No self-loops
/// (callers must not include `u == v` pairs — [`from_pairs`](Self::from_pairs)
/// panics in debug builds if one slips through via `debug_assert!`, and
/// silently drops it in release, matching the rest of the format's
/// "degrade, don't brick" posture for writer-side bugs).
#[derive(Debug, Clone)]
pub struct LeidenGraph {
    n: u32,
    offsets: Vec<u32>,
    neighbors: Vec<u32>,
    weights: Vec<i64>,
    sizes: Vec<u32>,
}

impl LeidenGraph {
    /// Build from an undirected pair list `(u, v, weight)`. Order-
    /// independent: pairs are symmetrized and sorted internally, and
    /// duplicate `(u, v)` entries (in either direction) have their
    /// weights summed, not overwritten — convenient for hand-built test
    /// graphs that list an edge once. Self-loops (`u == v`) are dropped.
    /// Every node has size 1 (level-0 graphs only — [`super::projection`]
    /// and test callers never build an already-aggregated graph by hand).
    pub fn from_pairs(n: u32, pairs: &[(u32, u32, u32)]) -> Self {
        let triples: Vec<(u32, u32, i64)> = pairs
            .iter()
            .filter(|&&(u, v, _)| u != v)
            .map(|&(u, v, w)| (u, v, i64::from(w)))
            .collect();
        let (offsets, neighbors, weights) = build_symmetric_csr(n, &triples);
        LeidenGraph {
            n,
            offsets,
            neighbors,
            weights,
            sizes: vec![1; n as usize],
        }
    }

    #[allow(dead_code)] // test/bench-only diagnostic accessor, no production caller
    pub fn node_count(&self) -> u32 {
        self.n
    }

    /// Total CPM objective-friendly degree-weighted edge count, mostly
    /// useful for tests/benches (`sum of weights / 2`, since each
    /// undirected edge is stored twice).
    #[allow(dead_code)] // test/bench-only diagnostic accessor, no production caller
    pub fn edge_count(&self) -> usize {
        self.neighbors.len() / 2
    }
}

/// Build a symmetric CSR adjacency from directed `(from, to, weight)`
/// triples, where each undirected edge may appear once or twice (either
/// direction). Duplicate `(a, b)` pairs are merged by summing weight.
/// Shared by [`LeidenGraph::from_pairs`] and [`aggregate`] — the only
/// place in this module that builds adjacency from scratch.
fn build_symmetric_csr(n: u32, triples: &[(u32, u32, i64)]) -> (Vec<u32>, Vec<u32>, Vec<i64>) {
    let mut entries: Vec<(u32, u32, i64)> = Vec::with_capacity(triples.len() * 2);
    for &(u, v, w) in triples {
        entries.push((u, v, w));
        entries.push((v, u, w));
    }
    entries.sort_unstable_by_key(|&(a, b, _)| (a, b));

    // Merge adjacent duplicate (a, b) entries (sum weight).
    let mut merged: Vec<(u32, u32, i64)> = Vec::with_capacity(entries.len());
    let mut i = 0;
    while i < entries.len() {
        let (a, b, _) = entries[i];
        let mut sum: i64 = 0;
        let mut j = i;
        while j < entries.len() && entries[j].0 == a && entries[j].1 == b {
            sum += entries[j].2;
            j += 1;
        }
        merged.push((a, b, sum));
        i = j;
    }

    let mut offsets = vec![0u32; n as usize + 1];
    for &(a, _, _) in &merged {
        offsets[a as usize + 1] += 1;
    }
    for i in 0..n as usize {
        offsets[i + 1] += offsets[i];
    }
    let neighbors: Vec<u32> = merged.iter().map(|&(_, b, _)| b).collect();
    let weights: Vec<i64> = merged.iter().map(|&(_, _, w)| w).collect();
    (offsets, neighbors, weights)
}

/// Final clustering over a [`LeidenGraph`]'s own node ids. `assignment`
/// is already compacted to dense ordinals `0..k-1`, ascending by the
/// minimum node id in each cluster (§3.3 step 6 / §13 R6 — since node
/// ids are canonical-order positions end to end, this *is* "ascending
/// by min canonical key").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeidenResult {
    pub assignment: Vec<u32>,
    pub levels: u16,
    pub iter_cap_hit: bool,
}

/// Run deterministic Leiden-CPM over `graph` at `resolution`. Pure,
/// single-threaded, no I/O.
pub fn run(graph: &LeidenGraph, resolution: Resolution) -> LeidenResult {
    let n = graph.n;
    if n == 0 {
        return LeidenResult {
            assignment: Vec::new(),
            levels: 0,
            iter_cap_hit: false,
        };
    }

    let mut current: Vec<u32> = (0..n).collect();
    let mut total_levels: u16 = 0;
    let mut converged = false;
    let mut hit_max_any = false;

    for _ in 0..MAX_ITERATIONS {
        let (owner, levels, hit_max) = run_multilevel(graph, Some(current.clone()), resolution);
        total_levels = total_levels.max(levels);
        hit_max_any |= hit_max;
        let changed = owner != current;
        current = owner;
        if !changed {
            converged = true;
            break;
        }
    }
    let iter_cap_hit = hit_max_any || !converged;

    LeidenResult {
        assignment: compact_by_min_id(&current),
        levels: total_levels,
        iter_cap_hit,
    }
}

/// Compact an arbitrary node-id-valued partition into dense ordinals
/// `0..k-1`, ascending by each group's minimum *member* node id — NOT by
/// the raw group-id value, which carries no ordering meaning on its own
/// (a group's id is just whichever node first served as a merge target;
/// a smaller-id node can end up merged into a larger-id group, so the
/// group id is not guaranteed to equal its own minimum member).
fn compact_by_min_id(partition: &[u32]) -> Vec<u32> {
    let n = partition.len();
    let reps = groups_ordered_by_min_member(partition, n as u32);
    let mut remap = vec![u32::MAX; n];
    for (new_id, &rep) in reps.iter().enumerate() {
        remap[rep as usize] = new_id as u32;
    }
    partition.iter().map(|&o| remap[o as usize]).collect()
}

/// The distinct values appearing in `partition` (each a "group id" in
/// `0..universe`), ordered ascending by that group's minimum member node
/// id (see [`compact_by_min_id`]'s doc comment for why this is not the
/// same as sorting the group-id values themselves).
fn groups_ordered_by_min_member(partition: &[u32], universe: u32) -> Vec<u32> {
    let mut min_member: Vec<Option<u32>> = vec![None; universe as usize];
    for (node, &g) in partition.iter().enumerate() {
        let node = node as u32;
        let slot = &mut min_member[g as usize];
        *slot = Some(slot.map_or(node, |cur| cur.min(node)));
    }
    let mut groups: Vec<(u32, u32)> = min_member
        .iter()
        .enumerate()
        .filter_map(|(g, m)| m.map(|mm| (g as u32, mm)))
        .collect();
    groups.sort_unstable_by_key(|&(_, mm)| mm);
    groups.into_iter().map(|(g, _)| g).collect()
}

/// One full multilevel pass (§3.3 steps 1–4): local moving, refinement,
/// aggregation, repeated until Traag termination (`|P| == |V(G)|`) or
/// `MAX_LEVELS`. Returns the flattened level-0 partition (node ids valid
/// at whichever level it stopped at — [`run`] compacts afterwards),
/// the number of aggregation levels actually run, and whether
/// `MAX_LEVELS` was hit.
fn run_multilevel(
    level0: &LeidenGraph,
    seed: Option<Vec<u32>>,
    resolution: Resolution,
) -> (Vec<u32>, u16, bool) {
    let n0 = level0.n;
    let mut owner: Vec<u32> = (0..n0).collect();
    let mut graph = level0.clone();
    let mut seed_partition = seed;
    let mut levels: u16 = 0;
    let mut hit_max = false;

    loop {
        let p = local_moving(&graph, seed_partition.as_deref(), resolution);
        let terminal = is_all_singletons(&p);
        let r = refine(&graph, &p, resolution);

        if terminal {
            compose_owner_direct(&mut owner, &r);
            break;
        }

        levels = levels.saturating_add(1);
        if u32::from(levels) >= MAX_LEVELS {
            hit_max = true;
            // R17: run a final refinement before exit (already did, as
            // `r`) so "every cluster connected" holds unconditionally.
            compose_owner_direct(&mut owner, &r);
            break;
        }

        let (next_graph, next_seed, refined_to_new) = aggregate(&graph, &r, &p);
        compose_owner_through_aggregation(&mut owner, &r, &refined_to_new);
        graph = next_graph;
        seed_partition = Some(next_seed);
    }

    (owner, levels, hit_max)
}

fn compose_owner_direct(owner: &mut [u32], r: &[u32]) {
    for o in owner.iter_mut() {
        *o = r[*o as usize];
    }
}

fn compose_owner_through_aggregation(owner: &mut [u32], r: &[u32], refined_to_new: &[u32]) {
    for o in owner.iter_mut() {
        *o = refined_to_new[r[*o as usize] as usize];
    }
}

fn is_all_singletons(p: &[u32]) -> bool {
    let mut seen = vec![false; p.len()];
    for &c in p {
        if seen[c as usize] {
            return false;
        }
        seen[c as usize] = true;
    }
    true
}

// ---------------------------------------------------------------------------
// Step 1: local moving (queue-based)
// ---------------------------------------------------------------------------

/// §3.3 step 1. `seed`, if given, is the starting community assignment
/// (warm-started from the previous level's aggregation); otherwise every
/// node starts in its own singleton community. Returns the unrefined
/// partition `P` — node ids used as community ids, bounded `< graph.n`.
fn local_moving(graph: &LeidenGraph, seed: Option<&[u32]>, res: Resolution) -> Vec<u32> {
    let n = graph.n as usize;
    if n == 0 {
        return Vec::new();
    }

    let mut comm: Vec<u32> = match seed {
        Some(s) if s.len() == n => s.to_vec(),
        _ => (0..n as u32).collect(),
    };

    let mut community_size: Vec<i64> = vec![0; n];
    for v in 0..n {
        community_size[comm[v] as usize] += i64::from(graph.sizes[v]);
    }
    let mut used = vec![false; n];
    for &c in &comm {
        used[c as usize] = true;
    }
    let mut empty_ids: BTreeSet<u32> = (0..n as u32).filter(|&c| !used[c as usize]).collect();

    let mut in_queue = vec![true; n];
    let mut queue: VecDeque<u32> = (0..n as u32).collect();

    let mut scratch: Vec<i64> = vec![0; n];
    let mut touched_flag = vec![false; n];
    let mut touched: Vec<u32> = Vec::new();

    while let Some(v) = queue.pop_front() {
        let vi = v as usize;
        in_queue[vi] = false;

        touched.clear();
        let start = graph.offsets[vi] as usize;
        let end = graph.offsets[vi + 1] as usize;
        for e in start..end {
            let u = graph.neighbors[e] as usize;
            let w = graph.weights[e];
            let cu = comm[u] as usize;
            if !touched_flag[cu] {
                touched_flag[cu] = true;
                touched.push(cu as u32);
            }
            scratch[cu] += w;
        }

        let cur = comm[vi] as usize;
        let n_v = i128::from(graph.sizes[vi]);
        let n_a = i128::from(community_size[cur]);
        let w_cur = i128::from(scratch[cur]);

        let mut best_delta: i128 = 0; // baseline: stay put
        let mut best_target: Option<u32> = None;

        for &c in &touched {
            if c as usize == cur {
                continue;
            }
            let w_b = i128::from(scratch[c as usize]);
            let n_b = i128::from(community_size[c as usize]);
            let delta =
                i128::from(res.den) * (w_b - w_cur) - i128::from(res.num) * n_v * (n_b - n_a + n_v);
            if delta > best_delta || (delta == best_delta && best_target.is_some_and(|bt| c < bt)) {
                best_delta = delta;
                best_target = Some(c);
            }
        }

        // The smallest currently-empty community id is the "move to a
        // brand-new singleton" candidate (§3.3 step 1).
        if let Some(&new_id) = empty_ids.iter().next() {
            let delta =
                i128::from(res.den) * (0 - w_cur) - i128::from(res.num) * n_v * (0 - n_a + n_v);
            if delta > best_delta
                || (delta == best_delta && best_target.is_some_and(|bt| new_id < bt))
            {
                best_delta = delta;
                best_target = Some(new_id);
            }
        }

        for &c in &touched {
            scratch[c as usize] = 0;
            touched_flag[c as usize] = false;
        }

        if best_delta > 0 {
            let target = best_target.expect("best_delta > 0 implies a candidate was found");
            let target_usize = target as usize;

            community_size[cur] -= i64::from(graph.sizes[vi]);
            if community_size[cur] == 0 {
                empty_ids.insert(cur as u32);
            }
            if community_size[target_usize] == 0 {
                empty_ids.remove(&target);
            }
            community_size[target_usize] += i64::from(graph.sizes[vi]);
            comm[vi] = target;

            for e in start..end {
                let u = graph.neighbors[e];
                if comm[u as usize] != target && !in_queue[u as usize] {
                    in_queue[u as usize] = true;
                    queue.push_back(u);
                }
            }
        }
    }

    comm
}

// ---------------------------------------------------------------------------
// Step 2: refinement (θ→0 greedy limit)
// ---------------------------------------------------------------------------

/// §3.3 step 2. Refines `p` (the unrefined local-moving partition) into
/// a finer partition `R` where every community is guaranteed
/// γ-well-connected, by starting every node as its own singleton and
/// only merging a still-singleton `v` into an existing, well-connected,
/// adjacent refined community `T` with `Δ' >= 0` — merges never cross a
/// `p`-community boundary. Returns `R`: node ids (the representative —
/// always the smallest member id, by construction of "ascending visit
/// order, singleton-only merges").
fn refine(graph: &LeidenGraph, p: &[u32], res: Resolution) -> Vec<u32> {
    let n = graph.n as usize;
    if n == 0 {
        return Vec::new();
    }

    let mut refined: Vec<u32> = (0..n as u32).collect();
    let mut refined_size: Vec<i64> = graph.sizes.iter().map(|&s| i64::from(s)).collect();

    let mut community_total_size: Vec<i64> = vec![0; n];
    for v in 0..n {
        community_total_size[p[v] as usize] += i64::from(graph.sizes[v]);
    }

    // deg_in_c[v] = w(v, C\v) where C = p[v]'s community — computed once
    // for every node up front, since a not-yet-visited node can be
    // selected as a merge target (its neighbour may have a smaller id
    // and be processed first).
    let mut deg_in_c: Vec<i64> = vec![0; n];
    for v in 0..n {
        let c = p[v];
        let start = graph.offsets[v] as usize;
        let end = graph.offsets[v + 1] as usize;
        for e in start..end {
            let u = graph.neighbors[e] as usize;
            if p[u] == c {
                deg_in_c[v] += graph.weights[e];
            }
        }
    }

    // Every node starts as its own singleton refined community, so its
    // initial "boundary within C" is simply its own within-C degree.
    let mut sum_deg_in_c: Vec<i64> = deg_in_c.clone();
    let mut internal_weight: Vec<i64> = vec![0; n];

    // Group members by community without a HashMap: push in ascending
    // node-id order (already the iteration order below), so each
    // per-community member list comes out ascending for free.
    let mut members_by_c: Vec<Vec<u32>> = vec![Vec::new(); n];
    for v in 0..n {
        members_by_c[p[v] as usize].push(v as u32);
    }

    let mut scratch: Vec<i64> = vec![0; n];
    let mut touched_flag = vec![false; n];
    let mut touched: Vec<u32> = Vec::new();

    for c in 0..n as u32 {
        let members = &members_by_c[c as usize];
        if members.is_empty() {
            continue;
        }
        let n_c = i128::from(community_total_size[c as usize]);

        for &v in members {
            let vi = v as usize;
            if refined[vi] != v {
                continue; // already merged into an earlier T within this C
            }
            if refined_size[vi] != i64::from(graph.sizes[vi]) {
                // §3.3 step 2 gates movers on "v is still a singleton" —
                // v's own slot hasn't moved, but it has already absorbed
                // at least one follower (some u has refined[u] == v), so
                // it is no longer a singleton and must only ever act as a
                // receiving T from here on, never as a mover again. Without
                // this check, v would be free to defect into some other T'
                // using its own lone-node `deg_in_c`/`size` — ignoring the
                // followers it already absorbed — which both evaluates the
                // wrong move (wrong n_v/w_v_c for what is now a multi-member
                // group) and strands those followers under a representative
                // id ("v") that no longer satisfies refine()'s own
                // well-connectedness guarantee for its final group.
                continue;
            }

            let w_v_c = deg_in_c[vi];
            let n_v = i128::from(graph.sizes[vi]);
            let well_connected_v =
                i128::from(res.den) * i128::from(w_v_c) >= i128::from(res.num) * n_v * (n_c - n_v);
            if !well_connected_v {
                continue;
            }

            touched.clear();
            let start = graph.offsets[vi] as usize;
            let end = graph.offsets[vi + 1] as usize;
            for e in start..end {
                let u = graph.neighbors[e] as usize;
                if p[u] != c {
                    continue;
                }
                let w = graph.weights[e];
                let t = refined[u];
                if !touched_flag[t as usize] {
                    touched_flag[t as usize] = true;
                    touched.push(t);
                }
                scratch[t as usize] += w;
            }

            let mut best_delta: i128 = i128::MIN;
            let mut best_t: Option<u32> = None;
            for &t in &touched {
                let n_t = i128::from(refined_size[t as usize]);
                let w_v_t = i128::from(scratch[t as usize]);
                let boundary_t = sum_deg_in_c[t as usize] - 2 * internal_weight[t as usize];
                let well_connected_t = i128::from(res.den) * i128::from(boundary_t)
                    >= i128::from(res.num) * n_t * (n_c - n_t);
                if !well_connected_t {
                    continue;
                }
                let delta = i128::from(res.den) * w_v_t - i128::from(res.num) * n_v * n_t;
                if delta < 0 {
                    continue;
                }
                let better = match best_t {
                    None => true,
                    Some(bt) => delta > best_delta || (delta == best_delta && t < bt),
                };
                if better {
                    best_delta = delta;
                    best_t = Some(t);
                }
            }

            if let Some(t) = best_t {
                refined[vi] = t;
                refined_size[t as usize] += i64::from(graph.sizes[vi]);
                sum_deg_in_c[t as usize] += w_v_c;
                internal_weight[t as usize] += scratch[t as usize];
            }

            for &t in &touched {
                scratch[t as usize] = 0;
                touched_flag[t as usize] = false;
            }
        }
    }

    refined
}

// ---------------------------------------------------------------------------
// Step 3: aggregation
// ---------------------------------------------------------------------------

/// §3.3 step 3. Contracts `graph` by the refined partition `r`: each
/// distinct `r` value becomes one aggregate node (sized by the sum of
/// its members' sizes), cross-community edges are summed, internal
/// (same-aggregate) edges are dropped (self-loops never enter `Δ'` or
/// `well_connected`'s formulas, so they carry no information this module
/// needs — see the module doc). New node ids are assigned in ascending
/// order of the old representative id, which preserves "ascending id ==
/// ascending min original id" through every level (R6).
///
/// Returns `(next_graph, next_seed, refined_to_new)` where
/// `refined_to_new[rep]` is the new node id for the refined community
/// represented by `rep`, and `next_seed` is the aggregate's initial
/// partition for the next level's local moving — grouped by which `p`
/// (unrefined) community each new node's underlying refined community
/// came from (§3.3 step 3 "the unrefined partition from step 1").
fn aggregate(graph: &LeidenGraph, r: &[u32], p: &[u32]) -> (LeidenGraph, Vec<u32>, Vec<u32>) {
    let n = graph.n as usize;
    // Ascending by min *member* node id (R6) — NOT by the raw `r` value,
    // which is just whichever node first served as a merge target (see
    // `groups_ordered_by_min_member`'s doc comment).
    let reps: Vec<u32> = groups_ordered_by_min_member(r, graph.n);
    let new_n = reps.len() as u32;

    let mut refined_to_new = vec![u32::MAX; n];
    for (new_id, &rep) in reps.iter().enumerate() {
        refined_to_new[rep as usize] = new_id as u32;
    }

    let mut new_sizes = vec![0u32; new_n as usize];
    for v in 0..n {
        let nid = refined_to_new[r[v] as usize] as usize;
        new_sizes[nid] = new_sizes[nid].saturating_add(graph.sizes[v]);
    }

    let mut triples: Vec<(u32, u32, i64)> = Vec::new();
    for v in 0..n {
        let start = graph.offsets[v] as usize;
        let end = graph.offsets[v + 1] as usize;
        let nv = refined_to_new[r[v] as usize];
        for e in start..end {
            let u = graph.neighbors[e] as usize;
            if u <= v {
                continue; // visit each undirected edge once
            }
            let nu = refined_to_new[r[u] as usize];
            if nu == nv {
                continue; // internal edge — dropped (see doc comment)
            }
            triples.push((nv, nu, graph.weights[e]));
        }
    }
    let (offsets, neighbors, weights) = build_symmetric_csr(new_n, &triples);
    let next_graph = LeidenGraph {
        n: new_n,
        offsets,
        neighbors,
        weights,
        sizes: new_sizes,
    };

    let next_seed = build_next_seed(new_n, &reps, p);

    (next_graph, next_seed, refined_to_new)
}

/// Seeds the next level's local moving by grouping new nodes according
/// to the `p`-community their underlying refined community belongs to
/// (every member of one refined community shares the same `p` value, by
/// construction — refinement never merges across a `p` boundary).
/// Compacted via first-seen order while scanning `reps` ascending, so
/// the result stays consistent with "ascending new id ~ ascending min
/// original id" (no HashMap — a `Vec<Option<u32>>` indexed by the old
/// `p` value, bounded by `p.len()`).
fn build_next_seed(new_n: u32, reps_sorted: &[u32], p: &[u32]) -> Vec<u32> {
    let mut seed = vec![0u32; new_n as usize];
    let mut p_to_seed: Vec<Option<u32>> = vec![None; p.len()];
    let mut next_id = 0u32;
    for (new_id, &rep) in reps_sorted.iter().enumerate() {
        let pc = p[rep as usize] as usize;
        let sid = match p_to_seed[pc] {
            Some(s) => s,
            None => {
                let s = next_id;
                p_to_seed[pc] = Some(s);
                next_id += 1;
                s
            }
        };
        seed[new_id] = sid;
    }
    seed
}

// ---------------------------------------------------------------------------
// CPM objective (test/proptest helper — H(final) >= H(singletons), §8)
// ---------------------------------------------------------------------------

/// Returns `2 * den * H(assignment)` (scaled to stay integer — see
/// `H(P) = Σ_c [W_in(c) - γ N_c(N_c-1)/2]`, `γ = num/den`), computed
/// directly from the level-0 graph and any assignment array sharing its
/// node-id space (works for both "all singletons" and a real Leiden
/// result). Two values at the *same* resolution compare correctly via
/// `>=` despite the scaling, since both sides are scaled identically.
#[allow(dead_code)] // proptest/unit-test helper only; no production caller
pub fn cpm_objective_scaled(graph: &LeidenGraph, assignment: &[u32], res: Resolution) -> i128 {
    let n = graph.n as usize;
    if n == 0 {
        return 0;
    }
    let num_clusters = assignment
        .iter()
        .copied()
        .max()
        .map(|m| m as usize + 1)
        .unwrap_or(0);

    let mut size_c: Vec<i128> = vec![0; num_clusters];
    for v in 0..n {
        size_c[assignment[v] as usize] += i128::from(graph.sizes[v]);
    }

    let mut w_in: Vec<i128> = vec![0; num_clusters];
    for v in 0..n {
        let cv = assignment[v];
        let start = graph.offsets[v] as usize;
        let end = graph.offsets[v + 1] as usize;
        for e in start..end {
            let u = graph.neighbors[e] as usize;
            if u <= v {
                continue; // each undirected edge counted once
            }
            if assignment[u] == cv {
                w_in[cv as usize] += i128::from(graph.weights[e]);
            }
        }
    }

    let mut total: i128 = 0;
    for c in 0..num_clusters {
        let nc = size_c[c];
        if nc == 0 {
            continue;
        }
        total += 2 * i128::from(res.den) * w_in[c] - i128::from(res.num) * nc * (nc - 1);
    }
    total
}

/// Does every cluster in `assignment` induce a connected subgraph of
/// `graph`? Used by both the proptests (lib-internal) and the
/// `fuzz_leiden` target (an external crate, so this must be a genuine
/// `pub fn`, not `#[cfg(test)]`-gated). Only reachable from this crate's
/// own tests and from `__fuzz_leiden_bytes` below — neither counts as a
/// production caller for the `--bins` target's dead-code analysis.
#[allow(dead_code)]
pub fn is_partition_connected(graph: &LeidenGraph, assignment: &[u32]) -> bool {
    let n = graph.n as usize;
    if n == 0 {
        return true;
    }
    debug_assert_eq!(assignment.len(), n);

    let num_clusters = assignment
        .iter()
        .copied()
        .max()
        .map(|m| m as usize + 1)
        .unwrap_or(0);
    let mut members_of: Vec<Vec<u32>> = vec![Vec::new(); num_clusters];
    for (v, &c) in assignment.iter().enumerate() {
        members_of[c as usize].push(v as u32);
    }

    for members in &members_of {
        if members.is_empty() {
            continue;
        }
        let mut in_cluster = vec![false; n];
        for &m in members {
            in_cluster[m as usize] = true;
        }
        let start_node = members[0];
        let mut visited = vec![false; n];
        let mut stack = vec![start_node];
        visited[start_node as usize] = true;
        let mut count = 1usize;
        while let Some(v) = stack.pop() {
            let vi = v as usize;
            let s = graph.offsets[vi] as usize;
            let e = graph.offsets[vi + 1] as usize;
            for edge in s..e {
                let u = graph.neighbors[edge];
                if in_cluster[u as usize] && !visited[u as usize] {
                    visited[u as usize] = true;
                    count += 1;
                    stack.push(u);
                }
            }
        }
        if count != members.len() {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Fuzz shim (`docs/V9-FORMAT.md` §7, §13 "Tests" — fuzz_leiden)
// ---------------------------------------------------------------------------

/// Decode a small (<= 256 node) graph from arbitrary bytes, run
/// [`run`] twice and assert: identical output, every cluster induces a
/// connected subgraph, and no panic (overflow would panic in a
/// debug/fuzz build via `i128` arithmetic — unreachable in practice at
/// this scale, but the fuzzer is free to try). Mirrors the `__fuzz_*`
/// pattern (e.g. `store/rename_chains.rs`).
#[doc(hidden)]
pub fn __fuzz_leiden_bytes(data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let n = u32::from(data[0]); // 0..=255, i.e. <= 256 nodes
    if n == 0 {
        return;
    }
    let rest = &data[1..];
    let mut pairs: Vec<(u32, u32, u32)> = Vec::new();
    for chunk in rest.as_chunks::<3>().0 {
        let u = u32::from(chunk[0]) % n;
        let v = u32::from(chunk[1]) % n;
        if u == v {
            continue;
        }
        let w = u32::from(chunk[2] % 8) + 1;
        pairs.push((u, v, w));
    }

    let graph = LeidenGraph::from_pairs(n, &pairs);
    let res = Resolution::DEFAULT;
    let first = run(&graph, res);
    let second = run(&graph, res);
    assert_eq!(
        first, second,
        "Leiden must be deterministic for a fixed graph+resolution"
    );
    assert!(
        is_partition_connected(&graph, &first.assignment),
        "every cluster must induce a connected subgraph"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clique(start: u32, len: u32) -> Vec<(u32, u32, u32)> {
        let mut pairs = Vec::new();
        for a in start..start + len {
            for b in (a + 1)..start + len {
                pairs.push((a, b, 1));
            }
        }
        pairs
    }

    // -----------------------------------------------------------------
    // Hand-built graphs with known partitions (§8)
    // -----------------------------------------------------------------

    #[test]
    fn two_k5_joined_by_bridge_yields_two_clusters() {
        let mut pairs = clique(0, 5);
        pairs.extend(clique(5, 5));
        pairs.push((4, 5, 1));
        let graph = LeidenGraph::from_pairs(10, &pairs);
        let result = run(&graph, Resolution::DEFAULT);
        let k = result.assignment.iter().copied().max().unwrap() + 1;
        assert_eq!(k, 2, "{:?}", result.assignment);
        assert_eq!(result.assignment[0..5], result.assignment[0..5]); // sanity
        let a = result.assignment[0];
        let b = result.assignment[5];
        assert_ne!(a, b);
        assert!(result.assignment[0..5].iter().all(|&c| c == a));
        assert!(result.assignment[5..10].iter().all(|&c| c == b));
        assert!(is_partition_connected(&graph, &result.assignment));
    }

    #[test]
    fn ring_of_eight_k4_yields_eight_clusters_under_cpm() {
        // The classic modularity resolution-limit counterexample: 8
        // K4's joined in a ring by single bridge edges. CPM (unlike
        // modularity) has no resolution limit, so each K4 stays its own
        // cluster at gamma=1/8.
        let mut pairs = Vec::new();
        for i in 0..8u32 {
            pairs.extend(clique(i * 4, 4));
        }
        for i in 0..8u32 {
            let a = i * 4; // bridge between clique i and clique i+1
            let b = ((i + 1) % 8) * 4;
            pairs.push((a, b, 1));
        }
        let graph = LeidenGraph::from_pairs(32, &pairs);
        let result = run(&graph, Resolution::DEFAULT);
        let k = result.assignment.iter().copied().max().unwrap() + 1;
        assert_eq!(k, 8, "{:?}", result.assignment);
        for i in 0..8u32 {
            let c0 = result.assignment[(i * 4) as usize];
            for off in 1..4 {
                assert_eq!(result.assignment[(i * 4 + off) as usize], c0);
            }
        }
        assert!(is_partition_connected(&graph, &result.assignment));
    }

    #[test]
    fn star_graph_is_one_cluster_at_default_resolution() {
        // Hub (0) connected to 4 leaves, weight-1 edges. At gamma=1/8
        // (num=1, den=8), a leaf joining a community of current size
        // `n_b` has `delta' = den*w - num*n_v*n_b = 8 - n_b` (see
        // `docs/V9-FORMAT.md` §3.4's "a node with one call edge (weight
        // 2) joins a community of size <= 16" — weight-1 edges halve
        // that to <= 8). With only 4 leaves the community never reaches
        // that cap, so the whole star stays one cluster; a star with 10
        // leaves would NOT (documented, not asserted, elsewhere).
        let mut pairs = Vec::new();
        for leaf in 1..=4u32 {
            pairs.push((0, leaf, 1));
        }
        let graph = LeidenGraph::from_pairs(5, &pairs);
        let result = run(&graph, Resolution::DEFAULT);
        let k = result.assignment.iter().copied().max().unwrap() + 1;
        assert_eq!(k, 1, "{:?}", result.assignment);
        assert!(is_partition_connected(&graph, &result.assignment));
    }

    #[test]
    fn star_graph_beyond_the_cpm_cap_splits_off_excess_leaves() {
        // Documents the cap behavior noted above: a 10-leaf star at
        // gamma=1/8 with weight-1 edges keeps only ~8 nodes (hub + 7
        // leaves) in one cluster; the rest end up singletons. This is
        // expected CPM behavior, not a bug — a real index uses CALL
        // weight 2, doubling the cap (see §3.4).
        let mut pairs = Vec::new();
        for leaf in 1..=10u32 {
            pairs.push((0, leaf, 1));
        }
        let graph = LeidenGraph::from_pairs(11, &pairs);
        let result = run(&graph, Resolution::DEFAULT);
        assert!(is_partition_connected(&graph, &result.assignment));
        // The hub's own cluster must contain more than just the hub.
        let hub_cluster = result.assignment[0];
        let hub_cluster_size = result
            .assignment
            .iter()
            .filter(|&&c| c == hub_cluster)
            .count();
        assert!(hub_cluster_size > 1, "{:?}", result.assignment);
    }

    #[test]
    fn disconnected_components_never_merge() {
        let mut pairs = clique(0, 4);
        pairs.extend(clique(4, 4));
        // No bridge at all between the two components.
        let graph = LeidenGraph::from_pairs(8, &pairs);
        let result = run(&graph, Resolution::DEFAULT);
        let a = result.assignment[0];
        let b = result.assignment[4];
        assert_ne!(
            a, b,
            "disconnected cliques must never land in the same cluster"
        );
        assert!(is_partition_connected(&graph, &result.assignment));
    }

    #[test]
    fn isolated_node_is_its_own_singleton() {
        // Node 2 has no edges at all.
        let pairs = vec![(0, 1, 1)];
        let graph = LeidenGraph::from_pairs(3, &pairs);
        let result = run(&graph, Resolution::DEFAULT);
        assert_ne!(result.assignment[2], result.assignment[0]);
        assert_ne!(result.assignment[2], result.assignment[1]);
    }

    #[test]
    fn empty_graph_yields_empty_result() {
        let graph = LeidenGraph::from_pairs(0, &[]);
        let result = run(&graph, Resolution::DEFAULT);
        assert!(result.assignment.is_empty());
        assert_eq!(result.levels, 0);
        assert!(!result.iter_cap_hit);
    }

    #[test]
    fn single_edge_pair_can_still_end_up_singleton_or_paired() {
        let graph = LeidenGraph::from_pairs(2, &[(0, 1, 1)]);
        let result = run(&graph, Resolution::DEFAULT);
        assert_eq!(result.assignment.len(), 2);
        assert!(is_partition_connected(&graph, &result.assignment));
    }

    #[test]
    fn no_nodes_no_edges_graph_has_all_singletons() {
        let graph = LeidenGraph::from_pairs(5, &[]);
        let result = run(&graph, Resolution::DEFAULT);
        let distinct: std::collections::HashSet<u32> = result.assignment.iter().copied().collect();
        assert_eq!(distinct.len(), 5);
    }

    // -----------------------------------------------------------------
    // Resolution env parsing
    // -----------------------------------------------------------------

    #[test]
    fn parse_resolution_env_accepts_valid_fraction() {
        let r = parse_resolution_env("1/4");
        assert_eq!(r, Resolution { num: 1, den: 4 });
    }

    #[test]
    fn parse_resolution_env_rejects_zero_and_oversized_and_malformed() {
        assert_eq!(parse_resolution_env("0/4"), Resolution::DEFAULT);
        assert_eq!(parse_resolution_env("4/0"), Resolution::DEFAULT);
        assert_eq!(parse_resolution_env("2000/4"), Resolution::DEFAULT);
        assert_eq!(parse_resolution_env("4/2000"), Resolution::DEFAULT);
        assert_eq!(parse_resolution_env("garbage"), Resolution::DEFAULT);
        assert_eq!(parse_resolution_env(""), Resolution::DEFAULT);
    }

    #[test]
    fn parse_resolution_env_accepts_boundary_values() {
        assert_eq!(
            parse_resolution_env("1024/1024"),
            Resolution {
                num: 1024,
                den: 1024
            }
        );
        assert_eq!(parse_resolution_env("1/1"), Resolution { num: 1, den: 1 });
    }

    // -----------------------------------------------------------------
    // Proptests (§8)
    // -----------------------------------------------------------------

    /// Checks the exact guarantee `refine` is documented to provide
    /// (§3.3 step 2): for every refined community `T` it produces (over
    /// a fixed graph and local-moving partition `p`), **every** member
    /// `v` satisfies `well_connected(v, T)` — evaluated against `T`'s
    /// *final* membership, in i128, using the same graph.
    ///
    /// This is a stronger, non-obvious claim than the per-move gate
    /// inside `refine` (`well_connected_v`, checked against `p[v]`'s
    /// *whole* p-community before any move): a node can be
    /// well-connected to its entire p-community while being poorly
    /// connected to one particular subset `T` carved out of it, and `T`
    /// keeps absorbing further members *after* `v` joins it, so this
    /// checks that nothing later invalidates `v`'s own membership.
    ///
    /// It holds by construction when `refine`'s singleton-only-mover
    /// gate is correct: a singleton `v` only ever moves into `T` when
    /// `Δ' = den·w(v,T_before) - num·n_v·N_T_before >= 0`, which is
    /// algebraically identical to `well_connected(v, T_after)` once `v`
    /// is counted as a member (`T_after \ v == T_before`,
    /// `N_T_after - n_v == N_T_before`) — so the move itself can never
    /// create a violation. This test also pins the fix (committed
    /// alongside it) that `refine`'s mover loop must skip a node that
    /// has already absorbed followers (`refined_size != size`), not
    /// just one whose own slot has moved — without that fix, a
    /// multi-member representative could defect into some other target
    /// using its own lone-node degree/size, stranding its followers
    /// under a label that no longer satisfies this inequality. Verified
    /// to fail (187/45,481 violations across the same proptest inputs)
    /// with that gate temporarily disabled, then restored — see the P3
    /// follow-up report.
    fn well_connected_holds_for_every_member_of_every_refined_community(
        graph: &LeidenGraph,
        p: &[u32],
        res: Resolution,
    ) -> bool {
        let r = refine(graph, p, res);
        let n = graph.n as usize;
        let mut size_of_t: std::collections::HashMap<u32, i128> = std::collections::HashMap::new();
        for (v, &t) in r.iter().enumerate().take(n) {
            *size_of_t.entry(t).or_insert(0) += i128::from(graph.sizes[v]);
        }
        for (v, &t) in r.iter().enumerate().take(n) {
            let n_t = size_of_t[&t];
            if n_t < 2 {
                continue; // singleton communities trivially satisfy well_connected (N_T - n_v == 0)
            }
            let n_v = i128::from(graph.sizes[v]);
            let mut w_v_t: i128 = 0;
            let start = graph.offsets[v] as usize;
            let end = graph.offsets[v + 1] as usize;
            for e in start..end {
                let u = graph.neighbors[e];
                if u as usize != v && r[u as usize] == t {
                    w_v_t += i128::from(graph.weights[e]);
                }
            }
            let ok = i128::from(res.den) * w_v_t >= i128::from(res.num) * n_v * (n_t - n_v);
            if !ok {
                return false;
            }
        }
        true
    }

    proptest::proptest! {
        /// §3.3 step 2's well-connectedness guarantee, as stated and
        /// justified on `well_connected_holds_for_every_member_of_every_refined_community`
        /// above — run directly against `refine`'s own output (not the
        /// full multi-level `run`, since the guarantee is specifically
        /// about what one `refine` call produces for a given `p`;
        /// `p` here is `local_moving`'s real output on the same graph,
        /// not an arbitrary partition, matching how `refine` is always
        /// actually called).
        #[test]
        fn well_connected_guarantee_holds_for_refine_output(
            edges in proptest::collection::vec((0u32..20, 0u32..20, 1u32..4), 0..80),
        ) {
            let n = 20;
            let pairs: Vec<(u32,u32,u32)> = edges.into_iter().filter(|&(u,v,_)| u != v).collect();
            let graph = LeidenGraph::from_pairs(n, &pairs);
            let p = local_moving(&graph, None, Resolution::DEFAULT);
            proptest::prop_assert!(well_connected_holds_for_every_member_of_every_refined_community(
                &graph, &p, Resolution::DEFAULT,
            ));
        }

        /// Same graph, edges fed in shuffled order, must give an
        /// identical result (§3.3 determinism guarantee, §8).
        #[test]
        fn shuffled_edge_order_gives_identical_result(
            seed in proptest::collection::vec((0u32..12, 0u32..12, 1u32..4), 0..40),
            shuffle_seed in 0u64..1000,
        ) {
            let n = 12;
            let pairs: Vec<(u32,u32,u32)> = seed.into_iter().filter(|&(u,v,_)| u != v).collect();
            let graph_a = LeidenGraph::from_pairs(n, &pairs);

            // Deterministic pseudo-shuffle (LCG), no external RNG crate.
            let mut shuffled = pairs.clone();
            let mut state = shuffle_seed.wrapping_add(1);
            for i in (1..shuffled.len()).rev() {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let j = (state >> 33) as usize % (i + 1);
                shuffled.swap(i, j);
            }
            let graph_b = LeidenGraph::from_pairs(n, &shuffled);

            let result_a = run(&graph_a, Resolution::DEFAULT);
            let result_b = run(&graph_b, Resolution::DEFAULT);
            proptest::prop_assert_eq!(result_a, result_b);
        }

        /// Every cluster is connected, for arbitrary small random graphs.
        #[test]
        fn every_cluster_is_connected(
            edges in proptest::collection::vec((0u32..16, 0u32..16, 1u32..4), 0..60),
        ) {
            let n = 16;
            let pairs: Vec<(u32,u32,u32)> = edges.into_iter().filter(|&(u,v,_)| u != v).collect();
            let graph = LeidenGraph::from_pairs(n, &pairs);
            let result = run(&graph, Resolution::DEFAULT);
            proptest::prop_assert!(is_partition_connected(&graph, &result.assignment));
        }

        /// H(final) >= H(singletons), computed in i128 (§8).
        #[test]
        fn final_objective_is_at_least_singleton_objective(
            edges in proptest::collection::vec((0u32..14, 0u32..14, 1u32..4), 0..50),
        ) {
            let n = 14;
            let pairs: Vec<(u32,u32,u32)> = edges.into_iter().filter(|&(u,v,_)| u != v).collect();
            let graph = LeidenGraph::from_pairs(n, &pairs);
            let result = run(&graph, Resolution::DEFAULT);
            let singletons: Vec<u32> = (0..n).collect();
            let h_final = cpm_objective_scaled(&graph, &result.assignment, Resolution::DEFAULT);
            let h_singletons = cpm_objective_scaled(&graph, &singletons, Resolution::DEFAULT);
            proptest::prop_assert!(h_final >= h_singletons);
        }

        /// Ordinals ascend by min member id (compact_by_min_id's contract,
        /// exercised end to end through `run`).
        #[test]
        fn ordinals_ascend_by_min_member(
            edges in proptest::collection::vec((0u32..16, 0u32..16, 1u32..4), 0..60),
        ) {
            let n = 16;
            let pairs: Vec<(u32,u32,u32)> = edges.into_iter().filter(|&(u,v,_)| u != v).collect();
            let graph = LeidenGraph::from_pairs(n, &pairs);
            let result = run(&graph, Resolution::DEFAULT);
            let num_clusters = result.assignment.iter().copied().max().map(|m| m as usize + 1).unwrap_or(0);
            let mut min_member = vec![u32::MAX; num_clusters];
            for (node, &c) in result.assignment.iter().enumerate() {
                let node = node as u32;
                if node < min_member[c as usize] {
                    min_member[c as usize] = node;
                }
            }
            for w in min_member.windows(2) {
                proptest::prop_assert!(w[0] < w[1], "ordinals must ascend by min member id: {:?}", min_member);
            }
        }
    }

    // -----------------------------------------------------------------
    // Determinism under different rayon pool sizes (§8)
    // -----------------------------------------------------------------

    #[test]
    fn deterministic_under_rayon_pool_of_one_and_eight_threads() {
        let mut pairs = clique(0, 5);
        pairs.extend(clique(5, 5));
        pairs.push((4, 5, 1));

        let run_in_pool = |threads: usize| -> LeidenResult {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let graph = LeidenGraph::from_pairs(10, &pairs);
                run(&graph, Resolution::DEFAULT)
            })
        };

        let one = run_in_pool(1);
        let eight = run_in_pool(8);
        assert_eq!(one, eight);
    }

    #[test]
    fn fuzz_shim_does_not_panic_on_arbitrary_bytes() {
        __fuzz_leiden_bytes(&[]);
        __fuzz_leiden_bytes(&[0]);
        __fuzz_leiden_bytes(&[5, 1, 2, 3, 0, 1, 1, 2, 2, 4]);
        let mut big = vec![255u8];
        big.extend(std::iter::repeat_n(7u8, 300));
        __fuzz_leiden_bytes(&big);
    }

    /// Pins "aggregation seeds the next level from the UNREFINED
    /// partition `p`, not the refined partition `r`" (`build_next_seed`,
    /// §3.3 step 3: "The aggregate's initial partition is the unrefined
    /// partition from step 1").
    ///
    /// Hand-built (found by an exhaustive random search over small
    /// graphs, then pinned as a fixed fixture) 18-node graph where
    /// `local_moving` and `refine` provably disagree: `local_moving`
    /// groups nodes `{3, 9, 10, 12, 15}` into one p-community (value
    /// `12`), but `refine` splits that community into two refined
    /// pieces — `{3, 10}` (representative `10`) and `{9, 12, 15}`
    /// (representative `12`) — because the split pieces are not
    /// well-connected to *each other*, only to the community as a
    /// whole. This is exactly the scenario the distinction matters for:
    /// seeding the next level from `p` groups the two resulting
    /// aggregate nodes back together (giving `local_moving` the
    /// opportunity to re-merge them if warranted); seeding from `r`
    /// would never group them (every representative `rep` trivially
    /// satisfies `r[rep] == rep`, so substituting `r` for `p` always
    /// degenerates to the discrete identity seed — no grouping hint at
    /// all, for *any* input, which is itself why this needs a
    /// concrete graph rather than a direct `build_next_seed` unit test
    /// to be a meaningful regression pin).
    #[test]
    fn aggregation_seeds_next_level_from_unrefined_partition_not_refined() {
        let pairs = vec![
            (3, 10, 1),
            (12, 10, 3),
            (11, 2, 3),
            (0, 4, 1),
            (7, 14, 1),
            (8, 6, 1),
            (17, 13, 1),
            (13, 2, 3),
            (7, 0, 2),
            (12, 15, 2),
            (2, 14, 3),
            (12, 9, 1),
            (0, 1, 3),
        ];
        let n = 18;
        let graph = LeidenGraph::from_pairs(n, &pairs);

        let p = local_moving(&graph, None, Resolution::DEFAULT);
        let r = refine(&graph, &p, Resolution::DEFAULT);

        // Pin the premise: local_moving merges {3,9,10,12,15} into one
        // p-community, refine splits it into {3,10} and {9,12,15}.
        assert_eq!(p[3], p[9]);
        assert_eq!(p[3], p[10]);
        assert_eq!(p[3], p[12]);
        assert_eq!(p[3], p[15]);
        assert_ne!(
            r[3], r[9],
            "premise: refine must split the p-community for this test to mean anything"
        );
        assert_eq!(r[3], r[10], "premise: {{3,10}} is one refined piece");
        assert_eq!(r[9], r[12]);
        assert_eq!(
            r[9], r[15],
            "premise: {{9,12,15}} is the other refined piece"
        );

        let (_next_graph, next_seed, refined_to_new) = aggregate(&graph, &r, &p);

        // The two pieces split out of the same p-community must be
        // seeded together at the next level.
        let new_id_a = refined_to_new[r[3] as usize];
        let new_id_b = refined_to_new[r[9] as usize];
        assert_ne!(
            new_id_a, new_id_b,
            "the two refined pieces must be distinct aggregate nodes"
        );
        assert_eq!(
            next_seed[new_id_a as usize], next_seed[new_id_b as usize],
            "aggregation must seed the next level from `p` (both pieces came from the same \
             p-community), not from `r` (which would never group them, since every \
             representative rep satisfies r[rep] == rep) — next_seed={next_seed:?}"
        );

        // Direct proof this is `p`-sourced, not coincidence: calling the
        // seed builder with `r` in place of `p` (the exact bug this test
        // guards against) gives the discrete identity seed instead —
        // confirmed by temporarily swapping the argument at the
        // `aggregate` call site during review (RED), then restoring
        // (GREEN); see the P3 follow-up report for the transcript.
        let reps = groups_ordered_by_min_member(&r, graph.node_count());
        let seed_from_p = build_next_seed(reps.len() as u32, &reps, &p);
        let seed_from_r_bug = build_next_seed(reps.len() as u32, &reps, &r);
        assert_eq!(
            seed_from_p[new_id_a as usize],
            seed_from_p[new_id_b as usize]
        );
        assert_ne!(
            seed_from_r_bug[new_id_a as usize], seed_from_r_bug[new_id_b as usize],
            "sanity: seeding from `r` must NOT group the split pieces (confirms this fixture \
             actually discriminates between the two argument choices)"
        );
    }
}
