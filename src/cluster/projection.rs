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
#[derive(Debug, Clone)]
pub struct ProjectionSymbol {
    pub sym_idx: u32,
    pub path: String,
    pub line: u32,
    /// `crate::index::symbols::SymbolKind` discriminant.
    pub kind: u8,
    pub name: String,
    pub language: Option<Language>,
}

/// `CallEdgeBuilder`-shaped: caller `sym_idx` (exact) + callee *name*
/// (needs resolving, §3.2).
#[derive(Debug, Clone)]
pub struct ProjectionCallEdge {
    pub caller_sym_idx: u32,
    pub callee_name: String,
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
    pub symbols: &'a [ProjectionSymbol],
    pub call_edges: &'a [ProjectionCallEdge],
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

pub fn project(input: &ProjectionInput<'_>) -> ProjectedGraph {
    // --- Canonical order + eligibility (R6) ---------------------------
    let mut eligible_indices: Vec<usize> = (0..input.symbols.len())
        .filter(|&i| is_eligible(input.symbols[i].kind, input.symbols[i].language))
        .collect();
    eligible_indices.sort_unstable_by(|&a, &b| {
        let sa = &input.symbols[a];
        let sb = &input.symbols[b];
        sa.path
            .cmp(&sb.path)
            .then(sa.line.cmp(&sb.line))
            .then(sa.kind.cmp(&sb.kind))
            .then(sa.name.cmp(&sb.name))
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

    // Per-path eligible (line, sym_idx) lists for nearest-preceding ref
    // attribution (F8). Nodes are already grouped contiguously by path
    // since canonical order sorts by path first.
    let mut by_path: HashMap<&str, Vec<(u32, u32)>> = HashMap::new(); // path -> [(line, sym_idx)] ascending
    for &i in &eligible_indices {
        let s = &input.symbols[i];
        by_path
            .entry(s.path.as_str())
            .or_default()
            .push((s.line, s.sym_idx));
    }

    // Name indices for call-edge resolution (§3.2 call-name rule, R7
    // "eligible candidates only").
    let mut by_name_in_file: HashMap<(&str, &str), u32> = HashMap::new(); // (path, name) -> min eligible sym_idx
    let mut eligible_by_name: HashMap<&str, Vec<u32>> = HashMap::new();
    for &i in &eligible_indices {
        let s = &input.symbols[i];
        eligible_by_name
            .entry(s.name.as_str())
            .or_default()
            .push(s.sym_idx);
        by_name_in_file
            .entry((s.path.as_str(), s.name.as_str()))
            .and_modify(|cur| *cur = (*cur).min(s.sym_idx))
            .or_insert(s.sym_idx);
    }
    // sym_idx -> path, for call-edge callers (to resolve same-file first).
    let mut path_of_sym: HashMap<u32, &str> = HashMap::new();
    for s in input.symbols {
        path_of_sym.insert(s.sym_idx, s.path.as_str());
    }

    // --- Sites: (file key, line, to_node) -> (from_node, weight) ------
    // Call edges first (exact attribution), so they win any (file, line,
    // to) collision with an approximate ref-edge attribution (R8).
    let mut sites: HashMap<(String, u32, u32), (u32, u32)> = HashMap::new();

    for e in input.call_edges {
        let Some(&from_node) = sym_to_node.get(&e.caller_sym_idx) else {
            continue; // caller itself ineligible
        };
        let Some(&caller_path) = path_of_sym.get(&e.caller_sym_idx) else {
            continue;
        };
        let target = by_name_in_file
            .get(&(caller_path, e.callee_name.as_str()))
            .copied()
            .or_else(|| {
                eligible_by_name
                    .get(e.callee_name.as_str())
                    .filter(|cands| cands.len() == 1)
                    .map(|cands| cands[0])
            });
        let Some(target_sym) = target else { continue };
        let Some(&to_node) = sym_to_node.get(&target_sym) else {
            continue;
        };
        if to_node == from_node {
            continue; // self-loop
        }
        let key = (caller_path.to_string(), e.line, to_node);
        sites.entry(key).or_insert((from_node, CALL_WEIGHT));
    }

    for (idx, e) in input.ref_edges.iter().enumerate() {
        if input.ambiguous.get(idx).copied().unwrap_or(false) {
            continue; // R7: ambiguous resolutions are not edges
        }
        let Some(&to_node) = sym_to_node.get(&e.to_sym_idx) else {
            continue;
        };
        let Some(path) = input.file_paths.get(e.from_file_id as usize) else {
            continue;
        };
        let Some(members) = by_path.get(path.as_str()) else {
            continue; // no eligible symbol in this file at all
        };
        // Nearest-preceding: greatest (line, _) with line <= e.line.
        let pos = members.partition_point(|&(line, _)| line <= e.line);
        if pos == 0 {
            continue; // before the first eligible symbol in the file
        }
        let (_, from_sym) = members[pos - 1];
        let Some(&from_node) = sym_to_node.get(&from_sym) else {
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
        let key = (path.clone(), e.line, to_node);
        sites.entry(key).or_insert((from_node, weight));
    }

    // Sites (deduped call+ref) feed the pair-weight accumulator keyed
    // only by the undirected node pair — the `(file, line)` part of the
    // key has already done its dedup job above.
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

    // Cap and emit the canonical (sorted) pair list.
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

fn add_weight(pair_weight: &mut HashMap<(u32, u32), u32>, a: u32, b: u32, w: u32) {
    let key = if a < b { (a, b) } else { (b, a) };
    let entry = pair_weight.entry(key).or_insert(0);
    *entry = entry.saturating_add(w);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(sym_idx: u32, path: &str, line: u32, name: &str) -> ProjectionSymbol {
        ProjectionSymbol {
            sym_idx,
            path: path.to_string(),
            line,
            kind: 0,
            name: name.to_string(),
            language: Some(Language::Rust),
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
            callee_name: "helper".to_string(),
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
            callee_name: "helper".to_string(),
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
            callee_name: "target".to_string(),
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
        let mut symbols = vec![sym(0, "src/a.rs", 1, "caller")];
        let mut ref_edges = Vec::new();
        for i in 0..20u32 {
            symbols.push(sym(i + 1, "src/b.rs", i + 1, &format!("t{i}")));
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
            callee_name: "f".to_string(),
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
}
