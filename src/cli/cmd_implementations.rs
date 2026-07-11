//! `vex implementations` — find types implementing a base class/interface.
//! Extracted from `cli/mod.rs` in S1 Group D.
//!
//! P3 (`docs/HIERARCHY-EDGES.md` §7, §8) — swapped from an always-live
//! tree-sitter walk to an index lookup via the v8 typed hierarchy edge
//! section (`reader.find_hierarchy_edges_by_symbol`), with the original
//! live walk (`crate::hierarchy::find_implementations`) kept as the
//! fallback for indexes that predate the section (pre-v8, or a v8 index
//! whose extraction never ran / found nothing). The fallback trigger is
//! deliberately narrow: it fires ONLY when the index can't be opened at
//! all, or `reader.has_hierarchy_edges()` is false. A real empty result
//! for a specific query (the section exists, resolves the name, but the
//! symbol simply has no recorded children) does NOT fall back — that
//! would silently paper over "genuinely nothing" with a second, slower
//! search that (being a live walk) could disagree with the index for
//! unrelated reasons, defeating the point of having an index at all.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{resolve_diff_filter, resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::scope;
use crate::hierarchy::ImplMatch;
use crate::protocol::capabilities;
use crate::store::format::EdgeKind;
use crate::store::reader::IndexReader;

#[allow(clippy::too_many_arguments)]
pub(crate) fn implementations(
    ctx: &CmdCtx<'_>,
    name: String,
    path: Option<std::path::PathBuf>,
    limit: usize,
    auto_update: bool,
    no_stale_check: bool,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    // Canonicalize up front (matches `cmd_check.rs`/`cmd_impact.rs`/
    // `cmd_status.rs`/`cmd_usages.rs`) — required for cache-path
    // writer/reader symmetry. `config::index_dir` hashes the raw path
    // bytes when `VEX_CACHE_DIR`/`--cache-dir` is active, so an
    // uncanonicalized `--path` (e.g. `/tmp/...` on macOS, a symlink to
    // `/private/tmp/...`) resolves to a DIFFERENT cache directory than
    // the one `vex index` wrote to from a canonicalized cwd — silently
    // "no index found" instead of the real one. Before P3 this command
    // never touched the index at all (pure live walk, which already
    // canonicalizes internally — see `hierarchy::find_implementations`),
    // so this wasn't reachable; the new index-lookup path changes that.
    let root = resolve_root(path)?
        .canonicalize()
        .context("canonicalize root")?;
    // Staleness / auto-update is handled exactly once, inside
    // `try_open_reader` → `ensure_index_ready` (which every sibling
    // index-backed command relies on). Do NOT also call `handle_staleness`
    // here: `ensure_index_ready` already runs it for an existing index, so
    // a standalone call would fire it twice and, with `--auto-update` on a
    // stale index, attempt the rebuild twice.
    let changed_paths = resolve_diff_filter(&root, &diff)?;
    let start = Instant::now();
    let fetch_limit = if path_scope.is_empty() && changed_paths.is_none() {
        limit
    } else {
        usize::MAX
    };

    // Attempt the index path first. Any failure to open the index at all
    // (no index directory, corrupt mmap, pre-v3 header, ...) is folded
    // into "no hierarchy section" — this command must never hard-error
    // just because there's no index, matching its pre-P3 behavior of
    // never touching an index in the first place.
    let reader = try_open_reader(&root, auto_update, no_stale_check, ctx);

    let matches: Vec<ImplMatch> = match reader {
        Some(reader) if reader.has_hierarchy_edges() => implementations_from_index(&reader, &name),
        _ => crate::hierarchy::find_implementations(&root, &name, fetch_limit, ctx.excludes)?,
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

    render_impl_matches(&ctx.format, &matches, &name, &root, elapsed);
    Ok(())
}

/// Best-effort index open for the `implementations` fast path. Deliberately
/// swallows every error (missing `.vex` dir, stale-but-not-auto-updating,
/// corrupt mmap, pre-v3 header, ...) into `None` — the caller treats that
/// identically to "no hierarchy section" and falls back to the live walk.
/// This mirrors the pre-P3 behavior where `vex implementations` never
/// touched an index and therefore never errored on one being absent.
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

/// Index-backed implementations lookup (P3). Resolves `name` to every
/// matching symbol index via the FST (a name can resolve to more than one
/// symbol — same class name defined in different files/modules — so we
/// query the hierarchy section once per candidate and union the results),
/// then for each candidate looks up its CHILDREN via
/// `find_hierarchy_edges_by_symbol` (the section is keyed by `to_sym_idx`,
/// i.e. the PARENT — see `docs/HIERARCHY-EDGES.md` §3.1). Every edge with
/// an unrecognised/reserved `EdgeKind` byte, or whose child symbol / file
/// can't be resolved, is skipped rather than guessed at — matches the
/// project-wide "degrade to empty/skip on malformed input, never panic"
/// convention for reader-derived data.
fn implementations_from_index(reader: &IndexReader, name: &str) -> Vec<ImplMatch> {
    let candidates = resolve_name_to_indices(reader, name);
    if candidates.is_empty() {
        return Vec::new();
    }

    // Resolved once, reused for every edge — `file_paths()` copies the
    // whole file table out of the mmap, so doing it per-edge would be
    // needlessly quadratic in the (usually small) result set.
    let file_paths = reader.file_paths();

    let mut out = Vec::new();
    for to_idx in candidates {
        for edge in reader.find_hierarchy_edges_by_symbol(to_idx) {
            let Some(relation) = relation_label(&edge) else {
                continue;
            };
            let Some(child) = reader.symbol(edge.from_sym_idx as usize) else {
                continue;
            };
            let Some(child_path) = file_paths.get(edge.from_file_id as usize) else {
                continue;
            };
            out.push(ImplMatch {
                path: child_path.clone(),
                line: edge.line() as usize,
                name: reader.read_string(child.name_offset).to_string(),
                base: name.to_string(),
                relation,
            });
        }
    }
    out
}

/// Map a raw `HierarchyEdge`'s kind byte to the `&'static str` relation
/// label `ImplMatch` expects (matching the live-walk's vocabulary — see
/// `hierarchy::queries::relation_label`). `EdgeKind::Uses` prints as
/// `"uses"` (PHP `use` / Ruby mixin composition, carried by the same
/// section per §3.2). An unrecognised discriminant byte (reserved for a
/// future kind, or corrupt data) yields `None` — the caller skips the
/// edge rather than fabricating a label.
fn relation_label(edge: &crate::store::format::HierarchyEdge) -> Option<&'static str> {
    match EdgeKind::try_from(edge.edge_kind_bits()) {
        Ok(EdgeKind::Extends) => Some("extends"),
        Ok(EdgeKind::Implements) => Some("implements"),
        Ok(EdgeKind::Uses) => Some("uses"),
        Err(()) => None,
    }
}

/// Resolve `name` to every symbol index whose (case-insensitively
/// compared) name matches exactly, via the symbol FST. A name can map to
/// several indices — e.g. two files each define a `Base` class — so
/// every caller of this helper is expected to query per-candidate and
/// union the results rather than picking one arbitrarily. Mirrors the
/// exact-match pattern in `cmd_check.rs::check_in_root` and
/// `cmd_usages.rs::member_defines`. Returns an empty `Vec` when the index
/// has no symbol FST at all (older format) or nothing matches.
pub(crate) fn resolve_name_to_indices(reader: &IndexReader, name: &str) -> Vec<u32> {
    let Some(sym_fst) = reader.symbol_fst_reader() else {
        return Vec::new();
    };
    let lower = name.to_lowercase();
    sym_fst
        .find(name)
        .into_iter()
        .filter(|&idx| {
            reader
                .symbol(idx as usize)
                .is_some_and(|r| reader.read_string(r.name_offset).to_lowercase() == lower)
        })
        .collect()
}

/// Shared filter+render tail for `Vec<ImplMatch>` regardless of which
/// data source produced it (index lookup or live walk) — this is the
/// byte-for-byte-preserved rendering logic from before P3; only the data
/// source changed, not the output shape.
fn render_impl_matches(
    format: &OutputFormat,
    matches: &[ImplMatch],
    name: &str,
    root: &Path,
    elapsed: std::time::Duration,
) {
    // v1.12.0 S8.2 — extends exit-code contract to `vex implementations`.
    if matches.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    match format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "base": m.base,
                        "relation": m.relation,
                        "path": m.path,
                        "line": m.line,
                    })
                })
                .collect();
            print_envelope(
                &json,
                capabilities::current(),
                super::output::default_meta_for(root),
            );
        }
        OutputFormat::Text => {
            if matches.is_empty() {
                println!("No implementations of \"{name}\" in {elapsed:.2?}");
            } else {
                println!(
                    "{name}: {} implementations in {elapsed:.2?}\n",
                    matches.len()
                );
                for m in matches {
                    println!("  {:<40} ({})  {}:{}", m.name, m.relation, m.path, m.line);
                }
            }
        }
        OutputFormat::Compact => {
            for m in matches {
                println!("{} {} {} {}:{}", m.relation, m.base, m.name, m.path, m.line);
            }
        }
    }
}
