//! `vex bundle --mode project` assembler — top-N callees by reverse
//! indegree (count(distinct callers) per callee). NOT PageRank;
//! `mode_hints.scoring = "reverse_indegree"` is the authoritative label.
//!
//! Also hosts the FU-6 directory-symbol-density tree
//! ([`DirectoryTreeEntry`]) emitted via `mode_hints.directory_tree` —
//! the "architecture orientation" use case from the v1.10.1 audit.
//!
//! Isolated from `mod.rs` so the per-mode assembler doesn't share
//! screen space with the public types + dispatch.

use std::collections::BTreeSet;

use anyhow::Result;
use serde::Serialize;

use crate::protocol::{LexicalSignals, PostSignals, SemanticSignals, Signals, StructuralSignals};
use crate::store::reader::IndexReader;

use super::{
    global_rank_percentile, BundleArgs, BundleCoreItem, BundleCtx, BundleItem, BundleResponse,
    ModeSpecificMeta,
};

/// Project-mode assembler. Public for bench/test access — see
/// `symbol::assemble_symbol` for the rationale.
pub fn assemble_project(
    args: &BundleArgs<'_>,
    ctx: &BundleCtx<'_>,
) -> Result<(BundleResponse, ModeSpecificMeta)> {
    let has_call_graph = ctx.reader.has_call_graph();

    // FU-6 `--directory-tree-only`: skip the indegree walk (which
    // requires the call graph) and emit just the directory-symbol
    // density tree. Honour the path-glob filter so users can narrow
    // the tree to e.g. `src/**` without pulling the whole repo.
    if args.directory_tree_only {
        let directory_tree =
            directory_symbol_tree(ctx.reader, args.path_glob, args.directory_tree_top)?;
        // Distinguish "no indexed files at all" from "caller suppressed
        // the tree via --directory-tree-top 0" so the empty_reason stays
        // honest — `directory_tree: []` means different things in those
        // two scenarios.
        let empty_reason = if directory_tree.is_empty() {
            if args.directory_tree_top == 0 {
                Some("directory_tree_top_zero")
            } else if ctx.reader.symbol_count() == 0 {
                Some("no_indexed_files")
            } else {
                // Path-glob filtered every entry away.
                Some("path_glob_filtered_all")
            }
        } else {
            None
        };
        return Ok((
            BundleResponse {
                mode: args.mode.as_str(),
                items: Vec::new(),
                mode_hints: Some(serde_json::json!({
                    "scoring": "directory_tree_only",
                    "directory_tree_only": true,
                    "directory_tree_top": args.directory_tree_top,
                    "directory_tree": directory_tree,
                    "has_call_graph": has_call_graph,
                    "path_glob": args.path_glob,
                    "empty_reason": empty_reason,
                })),
            },
            ModeSpecificMeta::default(),
        ));
    }

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
            signals: Signals::from_parts(
                StructuralSignals { fst_hit: true },
                LexicalSignals::default(),
                SemanticSignals::default(),
                PostSignals {
                    indegree: Some(row.indegree),
                    ..Default::default()
                },
            ),
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

    let directory_tree =
        directory_symbol_tree(ctx.reader, args.path_glob, args.directory_tree_top)?;

    let mode_hints = serde_json::json!({
        "scoring": "reverse_indegree",
        "top_n": args.top_n,
        "path_glob": args.path_glob,
        "total_ranked_symbols": report.total_ranked,
        "has_call_graph": true,
        "directory_tree_top": args.directory_tree_top,
        "directory_tree": directory_tree,
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

// ---------------------------------------------------------------------------
// FU-6 — directory-symbol-density tree (extension of `--mode project`).
// ---------------------------------------------------------------------------
//
// "Architecture orientation" use case from the v1.10.1 follow-up audit:
// a directory listing annotated with how many symbols live under each
// directory (recursive vs immediate). Reuses the indexed file table +
// symbol records — no new on-disk format, no walk_builder call.

/// One row of the directory tree returned via `mode_hints.directory_tree`.
/// Sorted descending by `recursive_symbol_count` at emit time.
///
/// `pub(crate)` so the serialised field names stay an internal contract
/// (locked by the integration tests in `tests/cli_bundle_test.rs`)
/// instead of a public-API guarantee.
#[derive(Serialize, Clone, Debug)]
pub(crate) struct DirectoryTreeEntry {
    /// POSIX-relative directory path (root reported as `"."`).
    pub path: String,
    /// Files indexed directly inside this directory (non-recursive).
    pub file_count: usize,
    /// Symbols indexed directly inside this directory (non-recursive).
    pub symbol_count: usize,
    /// Symbols indexed in this directory plus every transitive child.
    pub recursive_symbol_count: usize,
}

/// Build the directory-symbol-density tree from the indexed file
/// table + symbol records.
///
/// `path_glob` is honoured as an include-filter so the tree can be
/// narrowed (e.g. `--path-glob 'src/**'`). `top_n` caps the returned
/// entries.
fn directory_symbol_tree(
    reader: &IndexReader,
    path_glob: Option<&str>,
    top_n: usize,
) -> Result<Vec<DirectoryTreeEntry>> {
    // Caller asked for zero entries — skip the O(N + D) symbol scan
    // entirely. Saves the per-symbol HashMap fill on the legitimate
    // "suppress the tree" use case.
    if top_n == 0 {
        return Ok(Vec::new());
    }

    use std::collections::HashMap;

    // Build a (dir → (file_count, symbol_count)) map of the *immediate*
    // contributions, then roll that up the parent chain to produce the
    // recursive counts. Two-pass keeps the algorithm O(N + D) where D
    // is the average path depth (small).
    let path_scope = if let Some(glob) = path_glob {
        let includes = vec![glob.to_string()];
        Some(crate::cli::scope::PathScope::from_args(&includes, &[])?)
    } else {
        None
    };

    let file_paths = reader.file_paths();
    let mut per_dir_files: HashMap<String, usize> = HashMap::new();
    let mut per_dir_syms: HashMap<String, usize> = HashMap::new();

    // Per-file symbol counts derived from SymbolRecord::file_offset.
    // Symbol records carry a `file_offset` pointing into the strings
    // section — reading that yields the same path string as
    // `file_paths()` produces, so dir-keying matches up.
    let mut per_path_syms: HashMap<String, usize> = HashMap::new();
    for i in 0..reader.symbol_count() {
        let Some(rec) = reader.symbol(i) else {
            continue;
        };
        let path = reader.read_string(rec.file_offset);
        if path.is_empty() {
            continue;
        }
        *per_path_syms.entry(path.to_string()).or_insert(0) += 1;
    }

    for path in &file_paths {
        if let Some(ps) = &path_scope {
            if !ps.accept(path) {
                continue;
            }
        }
        let dir = directory_of(path);
        *per_dir_files.entry(dir.clone()).or_insert(0) += 1;
        let n = per_path_syms.get(path).copied().unwrap_or(0);
        *per_dir_syms.entry(dir).or_insert(0) += n;
    }

    // Roll counts up to every ancestor so `recursive_symbol_count`
    // reflects everything under each directory.
    let mut recursive: HashMap<String, usize> = HashMap::new();
    for (dir, count) in &per_dir_syms {
        for ancestor in ancestors_inclusive(dir) {
            *recursive.entry(ancestor).or_insert(0) += count;
        }
    }

    // Materialize entries: every directory that appears as either an
    // immediate contributor or an ancestor with rolled-up children.
    let mut all_dirs: BTreeSet<String> = BTreeSet::new();
    all_dirs.extend(per_dir_files.keys().cloned());
    all_dirs.extend(recursive.keys().cloned());

    let mut entries: Vec<DirectoryTreeEntry> = all_dirs
        .into_iter()
        .map(|dir| {
            let file_count = per_dir_files.get(&dir).copied().unwrap_or(0);
            let symbol_count = per_dir_syms.get(&dir).copied().unwrap_or(0);
            let recursive_symbol_count = recursive.get(&dir).copied().unwrap_or(symbol_count);
            DirectoryTreeEntry {
                path: dir,
                file_count,
                symbol_count,
                recursive_symbol_count,
            }
        })
        .collect();

    // Descending by recursive count; tie-break on path so the result is
    // reproducible across runs on the same index.
    entries.sort_by(|a, b| {
        b.recursive_symbol_count
            .cmp(&a.recursive_symbol_count)
            .then_with(|| a.path.cmp(&b.path))
    });
    entries.truncate(top_n);
    Ok(entries)
}

/// POSIX directory part of a relative path. Returns `"."` for files at
/// the project root. An absolute leading slash (defensive — indexed
/// paths go through `to_rel_posix` so this should never fire in
/// practice) collapses to `"."` so the root bucket is not double-counted
/// against the `"/"` synthetic ancestor.
fn directory_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => ".".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// All ancestor directories of `dir` (inclusive), in deepest-first
/// order. Root collapses to `"."` so files at the project root roll
/// into a stable bucket. The `"."` ancestor is always present at the
/// tail so every entry contributes to a single root rollup.
fn ancestors_inclusive(dir: &str) -> Vec<String> {
    if dir == "." {
        return vec![".".to_string()];
    }
    let mut out = Vec::new();
    let mut cur = dir.to_string();
    loop {
        out.push(cur.clone());
        match cur.rfind('/') {
            // `Some(0)` only happens if a malformed absolute path leaks
            // through `directory_of`. Stop without pushing a `"/"`
            // ancestor — the root bucket is the trailing `"."` below.
            Some(0) => break,
            Some(i) => cur.truncate(i),
            None => break,
        }
    }
    out.push(".".to_string());
    out
}
