//! C++ `#include "..."` → indexed-path resolver for v1.14.0 cross-file refs.
//!
//! Pure-function utility consumed by the Pass-2 ref resolver in
//! [`store::writer`]. Given the raw quoted include string, the file that
//! issued the directive, and the project-wide path index, returns one of the
//! indexed POSIX paths (the caller maps that back to a `file_id` via the
//! existing `file_ids: HashMap<String, u32>` table).
//!
//! ## Resolution strategy (locked in v1.14.0 task)
//!
//! 1. **Relative-to-file** — join `dir(from_path)` with the include string,
//!    collapse `.` / `..` segments without I/O, look up the result in the
//!    index. This is the path the C++ standard's "search the directory of
//!    the current file first" rule produces.
//! 2. **Project-wide basename fallback** — when (1) misses (header lives in
//!    a different subtree, project uses `-I` flags we can't see), search by
//!    file basename. Tie-break is deterministic: same-dir > shortest path
//!    from root > alphabetical. Determinism wins over "right" so two runs
//!    on the same tree produce the same ref edges.
//!
//! ## Boundary discipline
//!
//! All inputs are expected POSIX-relative. Include strings may still contain
//! backslashes when a Windows-only file forgot the portability convention
//! (`#include "sub\\bar.h"`); we normalise those here. The host-aware
//! [`crate::util::paths::normalize_to_posix`] is the wrong tool — its
//! `cfg(windows)` gate would let `\` leak through on POSIX hosts indexing a
//! Windows checkout. Resolution happens in the writer, which is OS-neutral
//! once paths are stored; so the normalisation is unconditional here.
//!
//! ## What we do NOT resolve
//!
//! - `#include <vector>` — parser already drops `system_lib_string` nodes,
//!   they never reach here.
//! - Macro includes (`#include MY_HEADER`) — same; parser skips them.
//! - Out-of-root `..` traversal — `from_path = "foo.cpp"` plus `"../x.h"`
//!   normalises to an empty stack; we fall through to the basename branch,
//!   which still has a chance if a `x.h` exists somewhere.
//! - `#include` lookup against system `-I` paths — not in scope; users
//!   relying on those see basename fallback or nothing.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

/// Basename → list of indexed POSIX paths that share it.
///
/// Borrows path strings from the `file_ids` map the caller already owns.
/// Built once per index build via [`build_basename_index`] so the basename
/// fallback is O(1) lookup + tiny sort rather than O(N) per include.
pub type BasenameIndex<'a> = HashMap<&'a str, Vec<&'a str>>;

/// Build the basename index over every indexed file path.
///
/// `file_ids` is the same `HashMap<String, u32>` the writer assembles
/// while emitting the file table. We borrow its keys so the resolver's
/// outputs can be used to probe `file_ids` directly without further
/// allocation.
pub fn build_basename_index(file_ids: &HashMap<String, u32>) -> BasenameIndex<'_> {
    let mut map: BasenameIndex<'_> = HashMap::with_capacity(file_ids.len());
    for path in file_ids.keys() {
        let path_ref = path.as_str();
        if let Some(base) = basename(path_ref) {
            map.entry(base).or_default().push(path_ref);
        }
    }
    map
}

/// Resolve `include_str` against the project index. Returns one of the
/// keys of `file_ids` (the relative POSIX path), or `None` when no
/// candidate exists.
///
/// `from_path` is the POSIX-relative path of the file containing the
/// `#include` directive — used to ground both the relative-to-file
/// branch and the same-dir tie-break in the basename branch.
pub fn resolve_include<'a>(
    include_str: &str,
    from_path: &str,
    file_ids: &'a HashMap<String, u32>,
    basename_index: &BasenameIndex<'a>,
) -> Option<&'a str> {
    // C++ syntax permits `\` inside `#include "..."` on Windows-only
    // source. Index storage is always `/`-separated (see
    // [`crate::util::paths::to_rel_posix`]), so harmonise the input
    // before any segment work.
    let raw = canonicalize_include(include_str);
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // ---- Branch 1: relative-to-file -------------------------------------
    let from_dir = dir_of(from_path);
    if let Some(joined) = join_and_normalize(from_dir, raw) {
        // `get_key_value` returns the actual `String` key — `as_str()`
        // gives a `&'a str` tied to `file_ids`, satisfying the
        // resolver's lifetime contract.
        if let Some((key, _)) = file_ids.get_key_value(joined.as_str()) {
            return Some(key.as_str());
        }
    }

    // ---- Branch 2: basename fallback -----------------------------------
    let base = basename(raw)?;
    let candidates = basename_index.get(base)?;
    pick_best(candidates, from_dir)
}

/// Replace every `\` in an include string with `/`. Unlike
/// [`crate::util::paths::normalize_to_posix`], this is unconditional —
/// the conversion has to happen even on POSIX hosts indexing a Windows
/// repo, where the literal source byte is `\`. Returns `Cow::Borrowed`
/// on the (overwhelmingly common) happy path so well-formed POSIX
/// include strings don't allocate per directive — at ~50 includes per
/// file across ~5k C++ files in the bug-report repo, this cuts a
/// quarter-million wasted `String`s out of `vex index`.
fn canonicalize_include(s: &str) -> Cow<'_, str> {
    if s.contains('\\') {
        Cow::Owned(s.replace('\\', "/"))
    } else {
        Cow::Borrowed(s)
    }
}

/// Directory portion of a POSIX-relative path. Returns `""` for a file
/// at the project root (no `/` separator).
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Basename portion — last `/`-separated segment. Returns `None` when
/// the path ends in `/` or is empty (defensive; should never happen
/// for real indexed paths).
fn basename(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next()?;
    if last.is_empty() {
        None
    } else {
        Some(last)
    }
}

/// Join `dir` + `include` and collapse `.` / `..` segments. Returns
/// `None` when the result escapes the project root (e.g.
/// `dir = ""`, `include = "../x.h"`).
fn join_and_normalize(dir: &str, include: &str) -> Option<String> {
    let mut segs: Vec<&str> = Vec::new();
    if !dir.is_empty() {
        for part in dir.split('/') {
            if !part.is_empty() {
                segs.push(part);
            }
        }
    }
    for part in include.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                // `pop()?` correctly bails when the include tries to
                // climb above the project root. The basename branch
                // still gets a chance afterward.
                segs.pop()?;
            }
            other => segs.push(other),
        }
    }
    if segs.is_empty() {
        return None;
    }
    Some(segs.join("/"))
}

/// Pick the deterministic winner from basename candidates: same-dir as
/// the issuing file > shortest path from root > alphabetical.
///
/// The path with the **fewest** `/` separators is "shortest from root"
/// — `a/x.h` (1 separator) wins over `a/b/x.h` (2). For ties on both
/// dimensions, lexicographic order is the final tiebreaker. Determinism
/// is the goal: two indexer runs on the same tree must agree.
fn pick_best<'a>(candidates: &[&'a str], from_dir: &str) -> Option<&'a str> {
    candidates.iter().copied().min_by(|a, b| {
        let a_same = dir_of(a) == from_dir;
        let b_same = dir_of(b) == from_dir;
        match (a_same, b_same) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => {
                let a_depth = a.matches('/').count();
                let b_depth = b.matches('/').count();
                a_depth.cmp(&b_depth).then_with(|| a.cmp(b))
            }
        }
    })
}

/// Build the file → directly-included file_id map for every C++ source/header
/// in the index. Caller is responsible for filtering `cpp_files` to actual
/// C++ paths (cheapest signal: file extension via
/// [`crate::parse::language::Language::from_extension`]) — `include_resolver`
/// stays decoupled from the language enum.
///
/// Each entry holds the deduplicated, file_id-sorted list of targets the
/// directive resolved to. **Self-includes are dropped** — a file that
/// `#include`s itself (via an alias path) shouldn't loop the BFS through
/// the source node. Unresolved includes (path not in the index, system
/// headers that leaked past the parser filter) are simply skipped: they
/// don't make the graph wrong, they just don't extend it.
///
/// Every C++ file passed in lands in the map, even when its include list is
/// empty. That lets the BFS use `include_graph.contains_key(&from)` as the
/// "is this file a C++ source?" gate without re-running language detection.
pub fn build_include_graph<'a, I>(
    cpp_files: I,
    file_ids: &HashMap<String, u32>,
    basename_index: &BasenameIndex<'a>,
) -> HashMap<u32, Vec<u32>>
where
    I: IntoIterator<Item = (&'a str, &'a [String])>,
{
    let mut graph: HashMap<u32, Vec<u32>> = HashMap::new();
    for (path, includes) in cpp_files {
        let Some(&from_fid) = file_ids.get(path) else {
            // File not in the writer's path table — shouldn't happen for
            // a well-formed `parsed` slice but the lookup keeps the
            // function total instead of panicking on a regression.
            continue;
        };
        let mut targets: Vec<u32> = Vec::with_capacity(includes.len());
        for inc in includes {
            if let Some(resolved) = resolve_include(inc, path, file_ids, basename_index) {
                if let Some(&target_fid) = file_ids.get(resolved) {
                    if target_fid != from_fid {
                        targets.push(target_fid);
                    }
                }
            }
        }
        // Sort+dedup so BFS visits neighbours in deterministic file_id
        // order and a header included twice (rare but legal in C++ before
        // `#pragma once`) only enters the queue once.
        targets.sort_unstable();
        targets.dedup();
        graph.insert(from_fid, targets);
    }
    graph
}

/// BFS the include graph from `from_file_id` looking for any sym_idx whose
/// defining file_id matches a candidate of `name`. Returns the first match
/// in breadth-first order — closer in the include graph wins over a
/// distant duplicate definition.
///
/// `sym_to_file_id` is **parallel to** the writer's `sym_entries` Vec (post
/// Module-filter), i.e. `sym_to_file_id[i]` is the defining file_id of the
/// global symbol whose name is in `name_to_global[name][_] == i`. Same
/// indexing convention as `name_to_global`, so the returned u32 can be
/// stored directly into `RefEdgeBuilder::to_sym_idx` without translation.
///
/// `from_file_id` itself is visited first — defensive fallback for a binder
/// gap that produced `Unresolved` for a symbol actually defined in the
/// source file. Normal C++ refs to in-file symbols go through
/// `BindTarget::ModuleSymbol` and never reach here.
///
/// Returns `None` when:
/// - `name` has no entries in `name_to_global` (unknown identifier);
/// - no candidate's defining file is reachable through `include_graph`
///   from `from_file_id`;
/// - `from_file_id` isn't a C++ file (its key isn't in `include_graph`).
///
/// Cycle safety: `visited: HashSet<file_id>` — `#pragma once` cycles and
/// guard-macro cycles are real, depth caps would either time out or miss
/// legitimate deep includes.
pub fn resolve_via_include_bfs(
    name: &str,
    from_file_id: u32,
    name_to_global: &HashMap<&str, Vec<u32>>,
    sym_to_file_id: &[u32],
    include_graph: &HashMap<u32, Vec<u32>>,
) -> Option<u32> {
    // Non-C++ files (whose paths were never passed to `build_include_graph`)
    // bail here — saves the candidate-by-file allocation below.
    include_graph.get(&from_file_id)?;

    let candidates = name_to_global.get(name)?;
    if candidates.is_empty() {
        return None;
    }

    // Group candidates by defining file_id. First sym_idx written per file
    // wins; the BFS then picks the file_id that's reachable in fewest hops.
    let mut candidate_by_file: HashMap<u32, u32> = HashMap::with_capacity(candidates.len());
    for &sym in candidates {
        if let Some(&fid) = sym_to_file_id.get(sym as usize) {
            candidate_by_file.entry(fid).or_insert(sym);
        }
    }
    if candidate_by_file.is_empty() {
        return None;
    }

    let mut visited: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    visited.insert(from_file_id);
    queue.push_back(from_file_id);

    while let Some(fid) = queue.pop_front() {
        if let Some(&sym) = candidate_by_file.get(&fid) {
            return Some(sym);
        }
        if let Some(neighbours) = include_graph.get(&fid) {
            for &next in neighbours {
                if visited.insert(next) {
                    queue.push_back(next);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_into_ids(paths: &[&str]) -> HashMap<String, u32> {
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| ((*p).to_string(), i as u32))
            .collect()
    }

    #[test]
    fn sibling_header_resolves_relative() {
        let ids = paths_into_ids(&["a/b/foo.cpp", "a/b/bar.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("bar.h", "a/b/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("a/b/bar.h"));
    }

    #[test]
    fn subdir_relative_include_resolves() {
        let ids = paths_into_ids(&["a/b/foo.cpp", "a/b/sub/bar.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("sub/bar.h", "a/b/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("a/b/sub/bar.h"));
    }

    #[test]
    fn parent_dir_include_resolves() {
        // `#include "../shared/x.h"` from `a/b/foo.cpp` lands on `a/shared/x.h`.
        let ids = paths_into_ids(&["a/b/foo.cpp", "a/shared/x.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("../shared/x.h", "a/b/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("a/shared/x.h"));
    }

    #[test]
    fn dot_segments_collapse() {
        let ids = paths_into_ids(&["a/b/foo.cpp", "a/b/bar.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("./bar.h", "a/b/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("a/b/bar.h"));
    }

    #[test]
    fn relative_miss_falls_back_to_unique_basename() {
        // No sibling `weird.h`; only `c/d/weird.h` exists project-wide.
        let ids = paths_into_ids(&["a/foo.cpp", "c/d/weird.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("weird.h", "a/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("c/d/weird.h"));
    }

    #[test]
    fn basename_tiebreak_prefers_same_dir() {
        // `a/b/bar.h` shares a directory with the issuer, `x/y/bar.h` does
        // not — same-dir wins regardless of depth.
        let ids = paths_into_ids(&["a/b/foo.cpp", "x/y/bar.h", "a/b/bar.h"]);
        let bi = build_basename_index(&ids);
        // Note: include is `"qq/bar.h"` so relative-to-file misses; we
        // exercise the basename fallback explicitly.
        let got = resolve_include("qq/bar.h", "a/b/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("a/b/bar.h"));
    }

    #[test]
    fn basename_tiebreak_prefers_shorter_path_when_no_same_dir() {
        let ids = paths_into_ids(&["root/foo.cpp", "deep/a/b/bar.h", "shallow/bar.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("qq/bar.h", "root/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("shallow/bar.h"));
    }

    #[test]
    fn basename_tiebreak_alphabetical_when_depth_equal() {
        // Two candidates, same depth, neither same-dir → alphabetical.
        let ids = paths_into_ids(&["root/foo.cpp", "z/bar.h", "a/bar.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("qq/bar.h", "root/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("a/bar.h"));
    }

    #[test]
    fn no_match_returns_none() {
        let ids = paths_into_ids(&["a/foo.cpp"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("does_not_exist.h", "a/foo.cpp", &ids, &bi);
        assert_eq!(got, None);
    }

    #[test]
    fn file_at_root_with_sibling_include() {
        let ids = paths_into_ids(&["foo.cpp", "bar.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("bar.h", "foo.cpp", &ids, &bi);
        assert_eq!(got, Some("bar.h"));
    }

    #[test]
    fn out_of_root_falls_through_to_basename() {
        // `from_path = "foo.cpp"` (root), include = `"../x.h"` → relative
        // branch pops past empty stack and yields None; basename branch
        // still finds the file.
        let ids = paths_into_ids(&["foo.cpp", "x.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("../x.h", "foo.cpp", &ids, &bi);
        assert_eq!(got, Some("x.h"));
    }

    #[test]
    fn windows_backslashes_in_include_normalize() {
        // A Windows-only file with `#include "sub\\bar.h"` — the index
        // stores POSIX paths; resolver normalises the input string.
        let ids = paths_into_ids(&["src/foo.cpp", "src/sub/bar.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("sub\\bar.h", "src/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("src/sub/bar.h"));
    }

    #[test]
    fn empty_include_is_none() {
        let ids = paths_into_ids(&["a/foo.cpp", "a/bar.h"]);
        let bi = build_basename_index(&ids);
        assert_eq!(resolve_include("", "a/foo.cpp", &ids, &bi), None);
        // Whitespace-only also rejected; the parser already strips
        // surrounding quotes, so this is purely defensive.
        assert_eq!(resolve_include("   ", "a/foo.cpp", &ids, &bi), None);
    }

    #[test]
    fn relative_wins_over_basename_when_both_exist() {
        // `bar.h` exists both as a sibling and elsewhere — relative-to-file
        // is the first branch, so it must win. If the implementation ever
        // probes basename first, the elsewhere copy would silently steal
        // the resolution; this test pins the priority.
        let ids = paths_into_ids(&["a/b/foo.cpp", "a/b/bar.h", "x/bar.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("bar.h", "a/b/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("a/b/bar.h"));
    }

    #[test]
    fn basename_index_groups_by_filename() {
        let ids = paths_into_ids(&["a/x.h", "b/x.h", "c/y.h"]);
        let bi = build_basename_index(&ids);
        let mut xs: Vec<&str> = bi.get("x.h").cloned().unwrap_or_default();
        xs.sort();
        assert_eq!(xs, vec!["a/x.h", "b/x.h"]);
        let ys: Vec<&str> = bi.get("y.h").cloned().unwrap_or_default();
        assert_eq!(ys, vec!["c/y.h"]);
    }

    #[test]
    fn deep_relative_path_with_mixed_dots() {
        // `a/b/c/foo.cpp` + `"./../sub/./x.h"` → `a/b/sub/x.h`.
        let ids = paths_into_ids(&["a/b/c/foo.cpp", "a/b/sub/x.h"]);
        let bi = build_basename_index(&ids);
        let got = resolve_include("./../sub/./x.h", "a/b/c/foo.cpp", &ids, &bi);
        assert_eq!(got, Some("a/b/sub/x.h"));
    }

    // ---------------- build_include_graph ----------------

    /// Build (path, includes) tuples for [`build_include_graph`] from a
    /// `&[(&str, &[&str])]` literal. The owned `Vec<String>` rounds the
    /// inputs to the IntoIterator shape the API takes.
    fn graph_input(spec: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        spec.iter()
            .map(|(p, incs)| {
                (
                    (*p).to_string(),
                    incs.iter().map(|s| (*s).to_string()).collect(),
                )
            })
            .collect()
    }

    fn build_graph(
        ids: &HashMap<String, u32>,
        bi: &BasenameIndex<'_>,
        owned: &[(String, Vec<String>)],
    ) -> HashMap<u32, Vec<u32>> {
        build_include_graph(
            owned.iter().map(|(p, v)| (p.as_str(), v.as_slice())),
            ids,
            bi,
        )
    }

    #[test]
    fn graph_resolves_single_include() {
        let ids = paths_into_ids(&["src/foo.cpp", "src/bar.h"]);
        let bi = build_basename_index(&ids);
        let owned = graph_input(&[("src/foo.cpp", &["bar.h"]), ("src/bar.h", &[])]);
        let g = build_graph(&ids, &bi, &owned);
        let foo_id = ids["src/foo.cpp"];
        let bar_id = ids["src/bar.h"];
        assert_eq!(g.get(&foo_id), Some(&vec![bar_id]));
        // Headers without includes still appear with an empty Vec — the BFS
        // uses graph membership as the "is this file C++?" gate.
        assert_eq!(g.get(&bar_id), Some(&vec![]));
    }

    #[test]
    fn graph_drops_self_include() {
        // Pathological `#include "foo.h"` inside `foo.h` (rare but legal
        // before guard macros take effect). The graph mustn't loop the BFS
        // through its starting node.
        let ids = paths_into_ids(&["foo.h"]);
        let bi = build_basename_index(&ids);
        let owned = graph_input(&[("foo.h", &["foo.h"])]);
        let g = build_graph(&ids, &bi, &owned);
        assert_eq!(g.get(&ids["foo.h"]), Some(&vec![]));
    }

    #[test]
    fn graph_skips_unresolvable_include() {
        // `#include "missing.h"` — neither relative nor basename match.
        // Build silently drops the edge; existing edges still land.
        let ids = paths_into_ids(&["src/foo.cpp", "src/bar.h"]);
        let bi = build_basename_index(&ids);
        let owned = graph_input(&[("src/foo.cpp", &["bar.h", "missing.h"])]);
        let g = build_graph(&ids, &bi, &owned);
        assert_eq!(g.get(&ids["src/foo.cpp"]), Some(&vec![ids["src/bar.h"]]));
    }

    #[test]
    fn graph_dedupes_repeated_includes() {
        let ids = paths_into_ids(&["foo.cpp", "bar.h"]);
        let bi = build_basename_index(&ids);
        let owned = graph_input(&[("foo.cpp", &["bar.h", "bar.h", "bar.h"])]);
        let g = build_graph(&ids, &bi, &owned);
        assert_eq!(g.get(&ids["foo.cpp"]), Some(&vec![ids["bar.h"]]));
    }

    // ---------------- resolve_via_include_bfs ----------------

    /// Construct the test scaffolding the BFS needs from a literal list of
    /// per-file symbol definitions. Order matters: the sym_idx assigned to
    /// `name` in file `i` is its position in the flattened iteration.
    #[allow(clippy::type_complexity)] // test helper; tuple shape mirrors writer's locals
    fn build_resolver_inputs<'a>(
        files: &'a [&'a str],
        defs: &'a [(&'a str, &'a [&'a str])],
    ) -> (HashMap<String, u32>, HashMap<&'a str, Vec<u32>>, Vec<u32>) {
        let ids = paths_into_ids(files);
        // Walk `defs` in file order so the global sym_idx is the entries
        // position (matches the writer's `name_to_global` convention).
        let mut name_to_global: HashMap<&str, Vec<u32>> = HashMap::new();
        let mut sym_to_file_id: Vec<u32> = Vec::new();
        let mut idx: u32 = 0;
        for &file in files {
            // `defs` may omit a file or list a file zero times — the empty
            // case is fine, just no symbols are recorded for it.
            for (def_file, names) in defs {
                if *def_file == file {
                    for &n in *names {
                        name_to_global.entry(n).or_default().push(idx);
                        sym_to_file_id.push(ids[file]);
                        idx += 1;
                    }
                }
            }
        }
        (ids, name_to_global, sym_to_file_id)
    }

    #[test]
    fn bfs_direct_include() {
        // `foo.cpp` includes `bar.h`, `bar.h` defines `Bar`.
        let files = &["foo.cpp", "bar.h"];
        let defs: &[(&str, &[&str])] = &[("bar.h", &["Bar"])];
        let (ids, ntg, stf) = build_resolver_inputs(files, defs);
        let graph: HashMap<u32, Vec<u32>> =
            HashMap::from([(ids["foo.cpp"], vec![ids["bar.h"]]), (ids["bar.h"], vec![])]);
        let got = resolve_via_include_bfs("Bar", ids["foo.cpp"], &ntg, &stf, &graph);
        // `Bar` is the first (and only) sym pushed → sym_idx 0.
        assert_eq!(got, Some(0));
    }

    #[test]
    fn bfs_transitive_through_chain() {
        // `a.cpp` → `b.h` → `c.h` defines `Baz`. Depth-2 must resolve.
        let files = &["a.cpp", "b.h", "c.h"];
        let defs: &[(&str, &[&str])] = &[("c.h", &["Baz"])];
        let (ids, ntg, stf) = build_resolver_inputs(files, defs);
        let graph: HashMap<u32, Vec<u32>> = HashMap::from([
            (ids["a.cpp"], vec![ids["b.h"]]),
            (ids["b.h"], vec![ids["c.h"]]),
            (ids["c.h"], vec![]),
        ]);
        let got = resolve_via_include_bfs("Baz", ids["a.cpp"], &ntg, &stf, &graph);
        assert_eq!(got, Some(0));
    }

    #[test]
    fn bfs_cycle_terminates() {
        // `a.h` ⇄ `b.h` mutual include (real before `#pragma once` is
        // applied). BFS must not loop; `Bar` lives in `b.h`.
        let files = &["a.h", "b.h"];
        let defs: &[(&str, &[&str])] = &[("b.h", &["Bar"])];
        let (ids, ntg, stf) = build_resolver_inputs(files, defs);
        let graph: HashMap<u32, Vec<u32>> = HashMap::from([
            (ids["a.h"], vec![ids["b.h"]]),
            (ids["b.h"], vec![ids["a.h"]]),
        ]);
        let got = resolve_via_include_bfs("Bar", ids["a.h"], &ntg, &stf, &graph);
        assert_eq!(got, Some(0));
    }

    #[test]
    fn bfs_returns_none_for_non_cpp_file() {
        // `foo.py` was never passed to `build_include_graph`, so its
        // file_id has no key in the map. BFS must early-out instead of
        // running BFS from an isolated node.
        let files = &["foo.py", "bar.h"];
        let defs: &[(&str, &[&str])] = &[("bar.h", &["Bar"])];
        let (ids, ntg, stf) = build_resolver_inputs(files, defs);
        let graph: HashMap<u32, Vec<u32>> = HashMap::from([(ids["bar.h"], vec![])]);
        let got = resolve_via_include_bfs("Bar", ids["foo.py"], &ntg, &stf, &graph);
        assert_eq!(got, None);
    }

    #[test]
    fn bfs_returns_none_for_unknown_name() {
        let files = &["foo.cpp"];
        let (ids, ntg, stf) = build_resolver_inputs(files, &[]);
        let graph: HashMap<u32, Vec<u32>> = HashMap::from([(ids["foo.cpp"], vec![])]);
        let got = resolve_via_include_bfs("Nope", ids["foo.cpp"], &ntg, &stf, &graph);
        assert_eq!(got, None);
    }

    #[test]
    fn bfs_returns_none_when_candidate_unreachable() {
        // `Bar` is defined in `unrelated.h`, which `foo.cpp` never reaches.
        let files = &["foo.cpp", "bar.h", "unrelated.h"];
        let defs: &[(&str, &[&str])] = &[("unrelated.h", &["Bar"])];
        let (ids, ntg, stf) = build_resolver_inputs(files, defs);
        let graph: HashMap<u32, Vec<u32>> = HashMap::from([
            (ids["foo.cpp"], vec![ids["bar.h"]]),
            (ids["bar.h"], vec![]),
            (ids["unrelated.h"], vec![]),
        ]);
        let got = resolve_via_include_bfs("Bar", ids["foo.cpp"], &ntg, &stf, &graph);
        assert_eq!(got, None);
    }

    #[test]
    fn bfs_prefers_closer_definition() {
        // `Foo` is defined in BOTH `near.h` (depth 1) and `deep.h` (depth 2
        // through `near.h`). The closer copy must win — that's the whole
        // point of BFS over DFS for this resolver.
        let files = &["src.cpp", "near.h", "deep.h"];
        let defs: &[(&str, &[&str])] = &[("near.h", &["Foo"]), ("deep.h", &["Foo"])];
        let (ids, ntg, stf) = build_resolver_inputs(files, defs);
        let graph: HashMap<u32, Vec<u32>> = HashMap::from([
            (ids["src.cpp"], vec![ids["near.h"]]),
            (ids["near.h"], vec![ids["deep.h"]]),
            (ids["deep.h"], vec![]),
        ]);
        let got = resolve_via_include_bfs("Foo", ids["src.cpp"], &ntg, &stf, &graph);
        // `Foo` in near.h was pushed first (sym_idx 0); deep.h's was 1.
        // BFS reaches near.h before deep.h, so we expect 0.
        assert_eq!(got, Some(0));
    }

    #[test]
    fn bfs_finds_definition_in_from_file_defensively() {
        // Belt-and-suspenders: if the binder produces `Unresolved` for a
        // symbol actually defined in the source file (binder gap), BFS
        // checks `from_file_id` first and resolves cleanly. Real C++ refs
        // to in-file defs go through ModuleSymbol and never reach here.
        let files = &["foo.cpp"];
        let defs: &[(&str, &[&str])] = &[("foo.cpp", &["LocalThing"])];
        let (ids, ntg, stf) = build_resolver_inputs(files, defs);
        let graph: HashMap<u32, Vec<u32>> = HashMap::from([(ids["foo.cpp"], vec![])]);
        let got = resolve_via_include_bfs("LocalThing", ids["foo.cpp"], &ntg, &stf, &graph);
        assert_eq!(got, Some(0));
    }

    #[test]
    fn bfs_with_empty_include_list_still_self_checks() {
        // C++ file with zero includes is in the graph (empty Vec). BFS
        // visits only itself. If the candidate isn't there, returns None
        // without false-positive matches against unrelated files.
        let files = &["lone.cpp", "elsewhere.h"];
        let defs: &[(&str, &[&str])] = &[("elsewhere.h", &["X"])];
        let (ids, ntg, stf) = build_resolver_inputs(files, defs);
        let graph: HashMap<u32, Vec<u32>> = HashMap::from([(ids["lone.cpp"], vec![])]);
        let got = resolve_via_include_bfs("X", ids["lone.cpp"], &ntg, &stf, &graph);
        assert_eq!(got, None);
    }

    #[test]
    fn bfs_indexes_through_sym_to_file_id_offsets() {
        // The writer's `name_to_global` and `sym_to_file_id` are
        // post-Module-filter parallel arrays. If a file with Module rows
        // sits before the target file, the target's sym_idx is offset
        // by the count of preceding non-Module symbols. This test pins
        // the indexing convention: building `name_to_global` with the
        // exact sym_idx the BFS should return, and verifying the BFS
        // returns it verbatim regardless of how Module-skipped offsets
        // shift entries. Guards against a future refactor that flips
        // sym_entries-position vs SymbolRecord-position semantics.
        let files = &["a.cpp", "b.h"];
        let (ids, _, _) = build_resolver_inputs(files, &[]);
        // Manually craft `name_to_global` and `sym_to_file_id` to
        // simulate b.h's `Bar` sitting at sym_idx 5 (e.g. after 5 syms
        // in a.cpp + Module rows skipped).
        let mut ntg: HashMap<&str, Vec<u32>> = HashMap::new();
        ntg.insert("Bar", vec![5]);
        let mut stf = vec![ids["a.cpp"]; 5];
        stf.push(ids["b.h"]); // sym_idx 5 lives in b.h
        let graph: HashMap<u32, Vec<u32>> =
            HashMap::from([(ids["a.cpp"], vec![ids["b.h"]]), (ids["b.h"], vec![])]);
        let got = resolve_via_include_bfs("Bar", ids["a.cpp"], &ntg, &stf, &graph);
        assert_eq!(
            got,
            Some(5),
            "BFS must return the exact sym_idx from name_to_global, not a remapped index"
        );
    }

    #[test]
    fn bfs_traverses_multiple_neighbours() {
        // `foo.cpp` includes `a.h` and `b.h` at the same depth; `Target`
        // is in `b.h`. BFS finds it regardless of neighbour order.
        let files = &["foo.cpp", "a.h", "b.h"];
        let defs: &[(&str, &[&str])] = &[("b.h", &["Target"])];
        let (ids, ntg, stf) = build_resolver_inputs(files, defs);
        let graph: HashMap<u32, Vec<u32>> = HashMap::from([
            (ids["foo.cpp"], vec![ids["a.h"], ids["b.h"]]),
            (ids["a.h"], vec![]),
            (ids["b.h"], vec![]),
        ]);
        let got = resolve_via_include_bfs("Target", ids["foo.cpp"], &ntg, &stf, &graph);
        assert_eq!(got, Some(0));
    }
}
