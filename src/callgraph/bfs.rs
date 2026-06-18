//! Multi-hop traversal of the persistent call graph (11.5).
//!
//! `find_paths` enumerates all caller chains from `from` to `to`, bounded
//! by `max_hops` and `max_paths`. `find_reachable` returns the transitive
//! set of symbols that can reach a target, also bounded by `max_hops`.
//!
//! Both functions are abstracted over a `callers_of(name)` closure so the
//! pure traversal logic can be unit-tested with an in-memory mock and the
//! production binary can plug in
//! [`crate::store::call_graph::find_callers_fast`].
//!
//! ## Identity model
//!
//! Traversal and the `visited` set both key on **function name only**,
//! not `(name, path)`. On a polyglot codebase with many `new` / `run` /
//! `process` definitions, all same-named functions collapse into a
//! single graph node. This is a deliberate first-cut trade-off — keeps
//! the BFS layer simple and matches the lowercased-name key used by
//! the underlying call-graph FST. The `path` field on each step still
//! carries the file from the matching call edge, so downstream output
//! disambiguates per call site. A future revision could swap the key
//! to `(name, file_id)` for stricter identity at the cost of a richer
//! adjacency model.

use std::collections::{HashSet, VecDeque};

use super::CallMatch;

/// One step in a reconstructed call chain. `line` is the call-site line
/// in `path` where this function invokes the next step in the chain
/// (the very last step in the path has `line == 0` because there is no
/// outgoing call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStep {
    pub name: String,
    pub path: String,
    pub line: usize,
}

/// One A→…→B caller chain.
#[derive(Debug, Clone)]
pub struct CallPath {
    pub steps: Vec<PathStep>,
}

/// One symbol reachable to `target`, with the BFS depth at which it was
/// discovered.
#[derive(Debug, Clone)]
pub struct ReachableMatch {
    pub name: String,
    pub path: String,
    pub line: usize,
    pub depth: usize,
}

/// Enumerate caller chains from `from` to `to` over the call graph,
/// bounded by `max_hops` and `max_paths`.
///
/// Strategy: DFS backward from `to` through callers. On each frontier
/// expansion we check whether the caller is `from` — if yes, emit the
/// reversed path; if no, recurse with the caller appended to the in-flight
/// chain. `visited_in_path` prevents cycles *within* one chain; popping
/// on backtrack lets the same node participate in different chains. The
/// `max_paths` cap aborts traversal cheaply once we've collected enough.
pub fn find_paths<F>(
    callers_of: F,
    from: &str,
    to: &str,
    max_hops: usize,
    max_paths: usize,
) -> Vec<CallPath>
where
    F: Fn(&str) -> Vec<CallMatch>,
{
    if max_hops == 0 || max_paths == 0 || from == to {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut visited_in_path: HashSet<String> = HashSet::new();
    visited_in_path.insert(to.to_string());
    // Seed the chain with `to` (the destination). `line: 0` because the
    // chain terminates here — there is no outgoing call from `to`.
    let mut chain: Vec<PathStep> = vec![PathStep {
        name: to.to_string(),
        path: String::new(),
        line: 0,
    }];
    dfs_paths(
        &callers_of,
        to,
        from,
        0,
        max_hops,
        max_paths,
        &mut visited_in_path,
        &mut chain,
        &mut out,
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn dfs_paths<F>(
    callers_of: &F,
    current: &str,
    target: &str,
    depth: usize,
    max_hops: usize,
    max_paths: usize,
    visited: &mut HashSet<String>,
    chain: &mut Vec<PathStep>,
    out: &mut Vec<CallPath>,
) where
    F: Fn(&str) -> Vec<CallMatch>,
{
    if out.len() >= max_paths || depth >= max_hops {
        return;
    }
    for caller in callers_of(current) {
        if out.len() >= max_paths {
            return;
        }
        if visited.contains(&caller.name) {
            continue;
        }
        let step = PathStep {
            name: caller.name.clone(),
            path: caller.path.clone(),
            line: caller.line,
        };
        if caller.name == target {
            // Found a complete chain — reverse so the user sees A→…→B.
            chain.push(step);
            let steps: Vec<PathStep> = chain.iter().rev().cloned().collect();
            out.push(CallPath { steps });
            chain.pop();
            continue;
        }
        visited.insert(caller.name.clone());
        chain.push(step);
        dfs_paths(
            callers_of,
            &caller.name,
            target,
            depth + 1,
            max_hops,
            max_paths,
            visited,
            chain,
            out,
        );
        chain.pop();
        visited.remove(&caller.name);
    }
}

/// Compute the transitive set of symbols whose callees reach `target`.
///
/// Plain BFS backward from `target` through callers, with a `visited`
/// set to prune cycles. Depth is the BFS level at which the symbol was
/// discovered; `target` itself is *not* included in the output.
pub fn find_reachable<F>(
    callers_of: F,
    target: &str,
    max_hops: usize,
    limit: usize,
) -> Vec<ReachableMatch>
where
    F: Fn(&str) -> Vec<CallMatch>,
{
    if max_hops == 0 || limit == 0 {
        return Vec::new();
    }
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(target.to_string());
    let mut out: Vec<ReachableMatch> = Vec::new();
    let mut frontier: VecDeque<(String, usize)> = VecDeque::new();
    frontier.push_back((target.to_string(), 0));
    while let Some((node, depth)) = frontier.pop_front() {
        if depth >= max_hops {
            continue;
        }
        for caller in callers_of(&node) {
            if !visited.insert(caller.name.clone()) {
                continue;
            }
            out.push(ReachableMatch {
                name: caller.name.clone(),
                path: caller.path.clone(),
                line: caller.line,
                depth: depth + 1,
            });
            if out.len() >= limit {
                return out;
            }
            frontier.push_back((caller.name, depth + 1));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Builds a `callers_of` closure from a static adjacency map keyed
    /// by callee name. Values are `(caller_name, call_site_line)` pairs.
    fn mock_graph(
        edges: &[(&'static str, &'static str, usize)],
    ) -> impl Fn(&str) -> Vec<CallMatch> {
        let mut map: HashMap<String, Vec<CallMatch>> = HashMap::new();
        for (caller, callee, line) in edges {
            map.entry((*callee).into()).or_default().push(CallMatch {
                name: (*caller).into(),
                path: format!("src/{caller}.rs"),
                line: *line,
            });
        }
        move |name: &str| map.get(name).cloned().unwrap_or_default()
    }

    // --- find_paths ---

    #[test]
    fn direct_caller_chain() {
        let callers = mock_graph(&[("A", "B", 10)]);
        let paths = find_paths(callers, "A", "B", 6, 50);
        assert_eq!(paths.len(), 1);
        let names: Vec<&str> = paths[0].steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
    }

    #[test]
    fn two_hop_chain_reconstructed_forward_order() {
        // A calls X (line 10); X calls B (line 20). Output is A→X→B.
        let callers = mock_graph(&[("A", "X", 10), ("X", "B", 20)]);
        let paths = find_paths(callers, "A", "B", 6, 50);
        assert_eq!(paths.len(), 1);
        let names: Vec<&str> = paths[0].steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["A", "X", "B"]);
        // Call-site line on A is where A calls X = 10.
        assert_eq!(paths[0].steps[0].line, 10);
        // Call-site line on X is where X calls B = 20.
        assert_eq!(paths[0].steps[1].line, 20);
        // Terminal step (B) has no outgoing call.
        assert_eq!(paths[0].steps[2].line, 0);
    }

    #[test]
    fn multiple_paths_enumerated() {
        // A→X→B and A→Y→B — both should appear.
        let callers = mock_graph(&[("A", "X", 1), ("A", "Y", 2), ("X", "B", 3), ("Y", "B", 4)]);
        let paths = find_paths(callers, "A", "B", 6, 50);
        assert_eq!(paths.len(), 2);
        let chains: Vec<Vec<&str>> = paths
            .iter()
            .map(|p| p.steps.iter().map(|s| s.name.as_str()).collect())
            .collect();
        assert!(chains.contains(&vec!["A", "X", "B"]));
        assert!(chains.contains(&vec!["A", "Y", "B"]));
    }

    #[test]
    fn no_path_returns_empty() {
        // A reaches B only through unreachable mid-node.
        let callers = mock_graph(&[("Other", "B", 1)]);
        let paths = find_paths(callers, "A", "B", 6, 50);
        assert!(paths.is_empty());
    }

    #[test]
    fn max_hops_truncates_search() {
        // A→X→Y→Z→B requires 4 hops; with max_hops=3 we shouldn't find it.
        let callers = mock_graph(&[("A", "X", 1), ("X", "Y", 2), ("Y", "Z", 3), ("Z", "B", 4)]);
        assert!(find_paths(&callers, "A", "B", 3, 50).is_empty());
        // With max_hops=4 the chain fits.
        assert_eq!(find_paths(&callers, "A", "B", 4, 50).len(), 1);
    }

    #[test]
    fn cycle_does_not_loop_forever() {
        // A↔B mutually recursive. paths(A, A) must not infinite-loop;
        // the from==to short-circuit returns empty.
        let callers = mock_graph(&[("A", "B", 1), ("B", "A", 2)]);
        let paths = find_paths(&callers, "A", "A", 6, 50);
        assert!(paths.is_empty());
        // paths(A, B) should still find A→B even with the back-edge.
        assert_eq!(find_paths(&callers, "A", "B", 6, 50).len(), 1);
    }

    #[test]
    fn max_paths_caps_output() {
        // Five distinct paths: A→X{1..5}→B.
        let edges: Vec<(&'static str, &'static str, usize)> = vec![
            ("A", "X1", 1),
            ("A", "X2", 2),
            ("A", "X3", 3),
            ("A", "X4", 4),
            ("A", "X5", 5),
            ("X1", "B", 11),
            ("X2", "B", 12),
            ("X3", "B", 13),
            ("X4", "B", 14),
            ("X5", "B", 15),
        ];
        let callers = mock_graph(&edges);
        assert_eq!(find_paths(&callers, "A", "B", 6, 3).len(), 3);
        assert_eq!(find_paths(&callers, "A", "B", 6, 100).len(), 5);
    }

    // --- find_reachable ---

    #[test]
    fn reachable_includes_direct_callers() {
        let callers = mock_graph(&[("A", "B", 1), ("X", "B", 2)]);
        let r = find_reachable(callers, "B", 6, 100);
        let names: Vec<&str> = r.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"A"));
        assert!(names.contains(&"X"));
        // Both at depth 1.
        for m in &r {
            assert_eq!(m.depth, 1);
        }
    }

    #[test]
    fn reachable_explores_transitively() {
        // Root→A→B; expect Root and A both reachable from B.
        let callers = mock_graph(&[("Root", "A", 1), ("A", "B", 2)]);
        let r = find_reachable(callers, "B", 6, 100);
        let by_name: HashMap<&str, usize> = r.iter().map(|m| (m.name.as_str(), m.depth)).collect();
        assert_eq!(by_name.get("A"), Some(&1));
        assert_eq!(by_name.get("Root"), Some(&2));
    }

    #[test]
    fn reachable_max_hops_bounds_depth() {
        // Three-hop chain. max_hops=2 should miss the deepest node.
        let callers = mock_graph(&[("L3", "L2", 1), ("L2", "L1", 2), ("L1", "target", 3)]);
        let names: Vec<String> = find_reachable(&callers, "target", 2, 100)
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert!(names.contains(&"L1".to_string()));
        assert!(names.contains(&"L2".to_string()));
        assert!(!names.contains(&"L3".to_string()));
    }

    #[test]
    fn reachable_dedupes_visits() {
        // Diamond: D and E both call C; both C-edges land us at C once.
        let callers = mock_graph(&[("D", "C", 1), ("E", "C", 2)]);
        let r = find_reachable(callers, "C", 6, 100);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn reachable_limit_caps_output() {
        // Stack-owned `Vec<String>` avoids the `Box::leak` that earlier
        // revisions used to forge a `&'static str` from a formatted
        // name. Leak-detection sanitisers stay clean.
        let names: Vec<String> = (0..20).map(|i| format!("c{i}")).collect();
        let edges: Vec<(&str, &str, usize)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), "B", i))
            .collect();
        // mock_graph needs `&'static str` for the static-lifetime
        // signature it inherited from earlier test cases; the names
        // here are heap-owned, so we build the adjacency map inline
        // instead.
        let mut map: std::collections::HashMap<String, Vec<CallMatch>> =
            std::collections::HashMap::new();
        for (caller, callee, line) in edges {
            map.entry(callee.into()).or_default().push(CallMatch {
                name: caller.into(),
                path: format!("src/{caller}.rs"),
                line,
            });
        }
        let callers = |name: &str| map.get(name).cloned().unwrap_or_default();
        assert_eq!(find_reachable(callers, "B", 6, 5).len(), 5);
    }

    // --- Dangling edges (Phase 11.5 hardening, G3) ---

    /// A `CallEdge` may name a callee whose symbol has been deleted from
    /// the index (stale section, dynamic-dispatch, FFI). `callers_of`
    /// returns `Vec::new()` for that name. BFS must terminate that branch
    /// cleanly: no panic, no infinite frontier, zero paths through the
    /// dangling node. Pins the behaviour both flavours rely on so a
    /// future "warn on unknown callee" refactor can't silently start
    /// surfacing partial chains.
    #[test]
    fn dangling_callee_terminates_paths_branch_cleanly() {
        // A→X exists in the adjacency map; X has NO incoming edges
        // (dangling intermediate). B is the queried target; no
        // edge ever lands on B, so paths(A, B) must return empty
        // rather than hang or panic on the missing X→? frontier.
        let callers = mock_graph(&[("A", "X", 1)]);
        let paths = find_paths(&callers, "A", "B", 6, 50);
        assert!(
            paths.is_empty(),
            "dangling X→B edge must yield no path (got: {paths:?})"
        );
    }

    #[test]
    fn dangling_intermediate_does_not_break_reachable() {
        // X is the target; only A calls X. Querying reachable from X
        // walks backwards through A. A has no callers (dangling
        // upstream). BFS must surface A at depth 1 and stop without
        // re-querying A's empty frontier in an infinite loop.
        let callers = mock_graph(&[("A", "X", 1)]);
        let r = find_reachable(callers, "X", 6, 100);
        assert_eq!(r.len(), 1, "expected 1 caller of X; got {r:?}");
        assert_eq!(r[0].name, "A");
        assert_eq!(r[0].depth, 1);
    }
}
