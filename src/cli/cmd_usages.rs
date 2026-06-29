//! `vex usages` — find references to a symbol via the refs FST (default)
//! or the v5 reference_edges section (`--strict`). Extracted from
//! `cli/mod.rs` in S1 Group D.2. `--workspace` (multi-repo) fans the
//! lookup over every member of a `.vex-workspace.toml`, grouped by repo.
//!
//! Phase 2 (v1.21): the ref-fetching + def-site / scope / docs filtering
//! routes through [`StrictRefsChannel`] / [`FstRefsChannel`] in the
//! `channel` module so the binder / FST split shares one implementation
//! with `vex impact`. Only `filter_path` and `diff` (the cmd_usages-
//! specific filters that don't fit the channel abstraction) are applied
//! locally, and the prefix-suggestion fallback stays here because it's
//! a query-shape concern, not a reference-resolution one.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{diff_filter_meta, resolve_diff_filter, resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::scope;
use crate::channel::{
    build_def_sites, Channel, ChannelContext, FstRefsChannel, HitLocation, StrictRefsChannel,
};
use crate::protocol::capabilities;
use crate::store::reader::IndexReader;
use crate::util::config::VexConfig;
use crate::workspace;

/// One repo's usages lookup: the surviving hits plus the counters the
/// single-repo `--why` trace / `_meta` envelope need. `available = false`
/// reproduces the v1.20.x bail conditions (no refs / `--strict` on a
/// pre-v5 index) without aborting a workspace fanout mid-loop.
struct UsagesOutcome {
    available: bool,
    unavailable_reason: Option<String>,
    entries: Vec<HitLocation>,
    total: usize,
    hits_before_filter: usize,
    def_site_dropped: usize,
    docs_dropped: usize,
    diff_retained: usize,
    diff_dropped: usize,
    changed_paths: Option<crate::util::git_diff::ChangedPaths>,
    /// `(name, usage_count)` did-you-mean suggestions (owned so the index
    /// reader can drop). `None` unless the exact lookup found nothing.
    prefix_suggestions: Option<Vec<(String, usize)>>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn usages(
    ctx: &CmdCtx<'_>,
    name: String,
    limit: usize,
    filter_path: Option<String>,
    auto_update: bool,
    no_stale_check: bool,
    strict: bool,
    why: bool,
    include_self: bool,
    include_docs: bool,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
    workspace: bool,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;

    if workspace {
        return usages_workspace(
            ctx,
            &name,
            limit,
            filter_path.as_deref(),
            auto_update,
            no_stale_check,
            strict,
            include_self,
            include_docs,
            &path_scope,
            &diff,
        );
    }

    let root = resolve_root(None)?.canonicalize()?;
    let outcome = usages_in_root(
        &root,
        ctx.cfg,
        ctx.excludes,
        ctx.local_cache_active,
        &name,
        limit,
        filter_path.as_deref(),
        auto_update,
        no_stale_check,
        strict,
        include_self,
        include_docs,
        &path_scope,
        &diff,
    )?;

    // Preserve the v1.20.x bail messages so existing scripts + docs still
    // match. The channel reports the same conditions via `available:
    // false`; convert into an `anyhow::bail!` here so the single-repo CLI
    // exit-code contract is unchanged.
    if !outcome.available {
        bail!(
            "{}",
            outcome
                .unavailable_reason
                .unwrap_or_else(|| "no refs in index — re-run `vex index` to rebuild".into())
        );
    }

    // v1.12.0 S8.2 — signal "no usages found" once for the exit-code
    // contract. Applies in both JSON and text formats.
    if outcome.entries.is_empty() {
        crate::cli::exit_code::signal_no_results();
    }

    let trace_mode: &'static str = if strict { "strict" } else { "fst_lookup" };
    let trace_mode_legacy: &'static str = if strict { "strict" } else { "text_scan" };
    let why_trace = if why {
        Some(crate::cli::trace::UsagesTrace {
            mode: trace_mode,
            mode_legacy: trace_mode_legacy,
            hits_before_filter: outcome.hits_before_filter,
            hits_after_filter: outcome.total,
            prefix_suggestions: outcome.prefix_suggestions.as_ref().map(|v| v.len()),
            def_site_dropped: outcome.def_site_dropped,
            docs_dropped: outcome.docs_dropped,
            filter_applied: crate::cli::trace::FilterSnapshot {
                filter: filter_path.clone(),
                include: scope.include.clone(),
                exclude: scope.exclude.clone(),
            },
        })
    } else {
        None
    };

    match ctx.format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = outcome
                .entries
                .iter()
                .map(|h| serde_json::json!({ "path": h.path, "line": h.line }))
                .collect();
            let mut meta = super::output::default_meta_for(&root);
            meta.diff_filter = diff_filter_meta(
                &diff,
                outcome.changed_paths.as_ref(),
                outcome.diff_retained,
                outcome.diff_dropped,
            );
            if let Some(ref t) = why_trace {
                meta.why_trace = serde_json::to_value(t).ok();
            }
            print_envelope(&json, capabilities::current(), meta);
        }
        OutputFormat::Text | OutputFormat::Compact => {
            if outcome.entries.is_empty() {
                println!("No usages found for \"{name}\"");
                if let Some(prefix_results) = outcome.prefix_suggestions.as_deref() {
                    if !prefix_results.is_empty() {
                        println!("\nDid you mean:");
                        for (n, count) in prefix_results.iter().take(5) {
                            println!("  {n} ({count} usages)");
                        }
                    }
                }
            } else {
                println!(
                    "{name}: {} usages (showing {})",
                    outcome.total,
                    outcome.entries.len()
                );
                for h in &outcome.entries {
                    println!("  {}:{}", h.path, h.line);
                }
            }
        }
    }

    if let Some(trace) = why_trace.as_ref() {
        crate::cli::trace::emit_why_trace(trace)?;
        if let Some(df) = diff_filter_meta(
            &diff,
            outcome.changed_paths.as_ref(),
            outcome.diff_retained,
            outcome.diff_dropped,
        ) {
            crate::cli::trace::emit_diff_filter(&df)?;
        }
    }

    Ok(())
}

/// Resolve `name`'s usages in one repo. Returns an `available = false`
/// outcome (rather than erroring) for the "no refs" / "`--strict` needs a
/// v5 index" conditions so a workspace fanout can report them per-repo.
#[allow(clippy::too_many_arguments)]
fn usages_in_root(
    root: &Path,
    cfg: &VexConfig,
    excludes: &[String],
    local_cache_active: bool,
    name: &str,
    limit: usize,
    filter_path: Option<&str>,
    auto_update: bool,
    no_stale_check: bool,
    strict: bool,
    include_self: bool,
    include_docs: bool,
    path_scope: &scope::PathScope,
    diff: &DiffFilterArgs,
) -> Result<UsagesOutcome> {
    let index_path = ensure_index_ready(
        root,
        auto_update,
        no_stale_check,
        false,
        local_cache_active,
        cfg,
    )?;
    let reader = IndexReader::open(&index_path).context("open index")?;
    usages_from_reader(
        &reader,
        root,
        excludes,
        name,
        limit,
        filter_path,
        strict,
        include_self,
        include_docs,
        path_scope,
        diff,
    )
}

/// Compute one repo's usages outcome from an already-open index reader.
/// Split out of [`usages_in_root`] so the workspace fanout can open each
/// member's reader ONCE (Phase 6.1) and reuse it for both the per-repo
/// outcome and the cross-repo unresolved-ref lookup — no double-open.
#[allow(clippy::too_many_arguments)]
fn usages_from_reader(
    reader: &IndexReader,
    root: &Path,
    excludes: &[String],
    name: &str,
    limit: usize,
    filter_path: Option<&str>,
    strict: bool,
    include_self: bool,
    include_docs: bool,
    path_scope: &scope::PathScope,
    diff: &DiffFilterArgs,
) -> Result<UsagesOutcome> {
    let changed_paths = resolve_diff_filter(root, diff)?;

    let file_paths = reader.file_paths();
    // Build def_sites only when the FST channel will consult it (non-strict
    // mode honours `--include-self`). Empty map in strict mode — the binder
    // excludes def-sites by construction.
    let def_sites = if !strict {
        build_def_sites(reader, name)
    } else {
        HashMap::new()
    };

    let channel_ctx = ChannelContext {
        reader,
        root,
        symbol: name,
        file_paths: &file_paths,
        def_sites: &def_sites,
        path_scope,
        excludes,
        filter_def_sites: !include_self,
        exclude_docs: !include_docs,
        // cmd_usages never runs the call-graph channels, so depth is a
        // no-op here. Pass `1` (the impact default) for clarity.
        depth: 1,
    };

    let output = if strict {
        StrictRefsChannel.run(&channel_ctx)?
    } else {
        FstRefsChannel.run(&channel_ctx)?
    };

    if !output.available {
        let reason = if strict {
            format!(
                "--strict needs a v5 index with reference_edges (this index is v{} or has no resolved refs). Re-run `vex index` to rebuild.",
                reader.header().version
            )
        } else {
            "no refs in index — re-run `vex index` to rebuild".to_string()
        };
        return Ok(UsagesOutcome {
            available: false,
            unavailable_reason: Some(reason),
            entries: Vec::new(),
            total: 0,
            hits_before_filter: 0,
            def_site_dropped: 0,
            docs_dropped: 0,
            diff_retained: 0,
            diff_dropped: 0,
            changed_paths,
            prefix_suggestions: None,
        });
    }

    let hits_before_filter = output.pre_filter_count;
    let def_site_dropped = output.dropped.def_site;
    let docs_dropped = output.dropped.docs;

    // Apply `filter_path` (substring) and `diff` (changed-path set) on the
    // channel's surviving hits — command-specific filters outside the
    // channel abstraction.
    let post_filter: Vec<HitLocation> = output
        .hits
        .into_iter()
        .filter(|h| {
            let filter_ok = filter_path.is_none_or(|fp| h.path.contains(fp));
            let diff_ok = changed_paths.as_ref().is_none_or(|cp| cp.contains(&h.path));
            filter_ok && diff_ok
        })
        .collect();
    let total = post_filter.len();
    let diff_retained = total;
    let diff_dropped = hits_before_filter
        .saturating_sub(total)
        .saturating_sub(def_site_dropped)
        .saturating_sub(docs_dropped);
    let entries: Vec<HitLocation> = post_filter.into_iter().take(limit).collect();

    // Prefix-suggestion fallback: only when no exact hits, FST path only.
    // Owned (name, count) so the reader can drop.
    let prefix_suggestions = if entries.is_empty() && !strict {
        reader.ref_reader().map(|rr| {
            rr.find_by_prefix(name)
                .into_iter()
                .map(|(n, refs)| (n.to_string(), refs.len()))
                .collect::<Vec<_>>()
        })
    } else {
        None
    };

    Ok(UsagesOutcome {
        available: true,
        unavailable_reason: None,
        entries,
        total,
        hits_before_filter,
        def_site_dropped,
        docs_dropped,
        diff_retained,
        diff_dropped,
        changed_paths,
        prefix_suggestions,
    })
}

/// Whether `reader`'s index defines a symbol named exactly `name`
/// (case-insensitive). Cross-repo owner detection — the member holding the
/// definition that another member's unresolved refs resolve to.
fn member_defines(reader: &IndexReader, name: &str) -> bool {
    let Some(sym_fst) = reader.symbol_fst_reader() else {
        return false;
    };
    let lower = name.to_lowercase();
    sym_fst.find(name).into_iter().any(|idx| {
        reader
            .symbol(idx as usize)
            .is_some_and(|r| reader.read_string(r.name_offset).to_lowercase() == lower)
    })
}

/// Gtags-style cross-repo fallback hits for one non-owner member, from its
/// already-open reader (Phase 6.1 — no re-open). Surfaces the unresolved-by-
/// name refs the member's own Pass-2 dropped because the symbol lives in a
/// sibling repo. Empty when the member has no v7 unresolved section.
///
/// The caller gates on (a) some member owning `name` and (b) this member not
/// being an owner. A member that DEFINES `name` cannot have `name` in its
/// unresolved section anyway — the writer capture gate
/// (`!name_to_global.contains_key`) excludes any name with a local def — so
/// skipping owners never drops a hit. `path_scope` + `filter_path` apply;
/// the per-member `diff` filter does not (changed-path sets don't compose).
fn cross_repo_hits(
    reader: &IndexReader,
    name: &str,
    limit: usize,
    filter_path: Option<&str>,
    path_scope: &scope::PathScope,
) -> Vec<HitLocation> {
    let file_paths = reader.file_paths();
    reader
        .find_unresolved_refs_by_name(name)
        .into_iter()
        .filter_map(|e| {
            let path = file_paths.get(e.from_file_id as usize)?.clone();
            if !path_scope.accept(&path) {
                return None;
            }
            if filter_path.is_some_and(|f| !path.contains(f)) {
                return None;
            }
            Some(HitLocation { path, line: e.line })
        })
        .take(limit)
        .collect()
}

/// One workspace member's fully-resolved usages result: its per-repo
/// outcome, its stale reason (if any), and any cross-repo name-resolved
/// hits surfaced from its v7 unresolved section (Phase 6).
struct RepoResult {
    display: String,
    outcome: UsagesOutcome,
    stale: Option<String>,
    cross: Vec<HitLocation>,
}

/// `vex usages --workspace`: find usages in every member, grouped by repo.
/// Each member's index is opened ONCE (Phase 6.1 two-phase orchestration)
/// and reused for both the per-repo outcome and the cross-repo unresolved-
/// ref lookup. `--why` is a clap conflict with `--workspace`.
#[allow(clippy::too_many_arguments)]
fn usages_workspace(
    ctx: &CmdCtx<'_>,
    name: &str,
    limit: usize,
    filter_path: Option<&str>,
    auto_update: bool,
    no_stale_check: bool,
    strict: bool,
    include_self: bool,
    include_docs: bool,
    path_scope: &scope::PathScope,
    diff: &DiffFilterArgs,
) -> Result<()> {
    // Multi-repo Phase 2: per-member cache layouts come from the installed
    // resolver (unsafe workspace-root hash-less case rejected in
    // `cli::build_workspace_resolver`).
    let start_dir = resolve_root(None)?;
    let ws = workspace::Workspace::find_and_load(&start_dir)?;
    let base = ws.base().to_path_buf();

    // Phase 1: ensure + open every member's index ONCE; capture stale signal
    // and owner status (does this member define `name`?) while the reader is
    // live. Holding the readers lets phase 2 reuse them for the cross-repo
    // lookup instead of re-opening (Phase 6.1).
    crate::cli::stale_signal::reset();
    struct Opened {
        display: String,
        root: std::path::PathBuf,
        excludes: Vec<String>,
        reader: IndexReader,
        stale: Option<String>,
        defines: bool,
    }
    let mut opened: Vec<Opened> = Vec::with_capacity(ws.members.len());
    for m in &ws.members {
        let member_cfg = crate::util::config::load_config(&m.root)?;
        let index_path = ensure_index_ready(
            &m.root,
            auto_update,
            no_stale_check,
            false,
            crate::util::config::skip_hash_for(&m.root),
            &member_cfg,
        )?;
        let reader = IndexReader::open(&index_path).context("open index")?;
        let stale = crate::cli::stale_signal::take();
        let defines = strict && member_defines(&reader, name);
        opened.push(Opened {
            display: m.display_name.clone(),
            root: m.root.clone(),
            excludes: member_cfg.exclude,
            reader,
            stale,
            defines,
        });
    }

    // Owner = first member, in declared order, that defines `name` (strict
    // only). Drives cross-repo attribution; its presence gates the fallback
    // so truly-undefined names / typos never surface unresolved refs.
    let cross_owner: Option<String> = opened.iter().find(|o| o.defines).map(|o| o.display.clone());

    // Phase 2: per-member outcome + (non-owner) cross-repo hits, all from the
    // already-open reader. Cross-repo hits are name-resolved (a distinct
    // sub-tier from in-repo binder-confirmed refs) — they render under the
    // same repo section, so the member's stale_reason already covers them.
    let mut any = false;
    let mut per_repo: Vec<RepoResult> = Vec::with_capacity(opened.len());
    for o in &opened {
        let outcome = usages_from_reader(
            &o.reader,
            &o.root,
            &o.excludes,
            name,
            limit,
            filter_path,
            strict,
            include_self,
            include_docs,
            path_scope,
            diff,
        )?;
        any |= outcome.available && !outcome.entries.is_empty();
        let cross = if cross_owner.is_some() && !o.defines {
            cross_repo_hits(&o.reader, name, limit, filter_path, path_scope)
        } else {
            Vec::new()
        };
        any |= !cross.is_empty();
        per_repo.push(RepoResult {
            display: o.display.clone(),
            outcome,
            stale: o.stale.clone(),
            cross,
        });
    }

    if !any {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
        OutputFormat::Json => render_workspace_json(&ws, &base, &per_repo, cross_owner.as_deref()),
        OutputFormat::Text | OutputFormat::Compact => {
            render_workspace_text(name, &per_repo, cross_owner.as_deref())
        }
    }
    Ok(())
}

/// Render workspace usages as a `--format json` envelope, grouped by repo.
fn render_workspace_json(
    ws: &workspace::Workspace,
    base: &Path,
    per_repo: &[RepoResult],
    cross_owner: Option<&str>,
) {
    let repos: Vec<_> = per_repo
        .iter()
        .map(|r| {
            let usages: Vec<_> = r
                .outcome
                .entries
                .iter()
                .map(|h| serde_json::json!({ "path": h.path, "line": h.line }))
                .collect();
            let mut obj = serde_json::json!({
                "repo": r.display,
                "total": r.outcome.total,
                "usages": usages,
            });
            if !r.outcome.available {
                obj["unavailable"] = serde_json::json!(r.outcome.unavailable_reason);
            }
            if let Some(reason) = &r.stale {
                obj["stale_reason"] = serde_json::json!(reason);
            }
            if !r.cross.is_empty() {
                obj["cross_repo_usages"] = serde_json::json!(r
                    .cross
                    .iter()
                    .map(|h| serde_json::json!({ "path": h.path, "line": h.line }))
                    .collect::<Vec<_>>());
                obj["resolves_to"] = serde_json::json!(cross_owner);
                obj["confidence"] = serde_json::json!("name");
            }
            obj
        })
        .collect();
    print_envelope(
        serde_json::json!({
            "workspace": ws.file.to_string_lossy(),
            "repos": repos,
        }),
        capabilities::current(),
        super::output::default_meta_for(base),
    );
}

/// Render workspace usages as grouped-by-repo text/compact output.
fn render_workspace_text(name: &str, per_repo: &[RepoResult], cross_owner: Option<&str>) {
    for r in per_repo {
        println!("── {} ──", r.display);
        if let Some(reason) = &r.stale {
            eprintln!("  (stale: {reason})");
        }
        if !r.outcome.available {
            println!(
                "  unavailable: {}",
                r.outcome.unavailable_reason.as_deref().unwrap_or("no refs")
            );
        } else if r.outcome.entries.is_empty() {
            println!("  No usages found for \"{name}\"");
        } else {
            println!(
                "  {name}: {} usages (showing {})",
                r.outcome.total,
                r.outcome.entries.len()
            );
            for h in &r.outcome.entries {
                println!("    {}:{}", h.path, h.line);
            }
        }
        if !r.cross.is_empty() {
            let owner = cross_owner.unwrap_or("?");
            println!("  cross-repo → {owner} (name-resolved):");
            for h in &r.cross {
                println!("    {}:{}", h.path, h.line);
            }
        }
    }
}

// is_doc_path unit tests live next to the function itself in
// `src/util/paths.rs` (moved here in v1.20.0 D4 so cmd_search and
// cmd_usages can share one filter).
