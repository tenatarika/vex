//! `vex update` — incremental rebuild honouring sticky section opt-outs
//! from the prior manifest. Extracted from `cli/mod.rs` in S1 Group C.
//! `--workspace` (multi-repo) fans the incremental update over every
//! member of a `.vex-workspace.toml`.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};

use super::args::OutputFormat;
use super::common::{
    build_index_options, resolve_embedder, resolve_root, resolve_semantic, CmdCtx,
};
use super::output::print_envelope;
use crate::index::pipeline;
use crate::protocol::capabilities;
use crate::util::config::{self, VexConfig};
use crate::workspace;

/// `(total_symbols, changed, deleted)` from one repo's incremental update.
type UpdateStats = (usize, usize, usize);

/// CLI update flags shared by the single-repo and per-member paths.
pub(crate) struct UpdateFlags {
    pub semantic: bool,
    pub no_semantic: bool,
    pub embedder: Option<String>,
    pub no_call_graph: bool,
    pub no_bm25: bool,
    pub no_pattern_index: bool,
    pub history: bool,
    pub no_history: bool,
    pub gpu: bool,
    pub no_gpu: bool,
    pub device: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn update(
    ctx: &CmdCtx<'_>,
    path: Option<std::path::PathBuf>,
    semantic: bool,
    no_semantic: bool,
    embedder: Option<String>,
    _jobs: Option<usize>,
    no_call_graph: bool,
    no_bm25: bool,
    no_pattern_index: bool,
    no_wait: bool,
    history: bool,
    no_history: bool,
    gpu: bool,
    no_gpu: bool,
    device: Option<String>,
    workspace: bool,
) -> Result<()> {
    let flags = UpdateFlags {
        semantic,
        no_semantic,
        embedder,
        no_call_graph,
        no_bm25,
        no_pattern_index,
        history,
        no_history,
        gpu,
        no_gpu,
        device,
    };

    if workspace {
        return update_workspace(ctx, path, &flags, no_wait);
    }

    // Canonicalize once at the top so `prior_manifest`'s lookup path matches
    // the one `pipeline::update` uses internally — a divergent path would map
    // to a different cache subdir and silently drop the sticky-opt-out
    // invariant.
    let root = resolve_root(path)?
        .canonicalize()
        .context("canonicalize project root")?;
    let start = Instant::now();
    let outcome = update_one(&root, ctx.cfg, ctx.excludes, &flags, no_wait)?;

    let Some((total, changed, deleted)) = outcome else {
        // --no-wait + lock held: structured "busy" outcome, exit 0.
        match ctx.format {
            OutputFormat::Json => print_envelope(
                serde_json::json!({
                    "status": "busy",
                    "reason": "another vex instance holds the build lock",
                }),
                capabilities::current(),
                super::output::default_meta_for(&root),
            ),
            OutputFormat::Text | OutputFormat::Compact => {
                println!("Skipped: another vex instance is updating (--no-wait).");
            }
        }
        return Ok(());
    };

    let elapsed = start.elapsed();

    match ctx.format {
        OutputFormat::Json => {
            let json = serde_json::json!({
                "symbols": total,
                "changed": changed,
                "deleted": deleted,
                "elapsed_ms": elapsed.as_millis(),
            });
            print_envelope(
                &json,
                capabilities::current(),
                super::output::default_meta_for(&root),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            if changed == 0 && deleted == 0 {
                println!("Index up to date ({total} symbols)");
            } else {
                println!(
                    "Updated in {elapsed:.2?}: {changed} changed, {deleted} deleted, {total} total symbols"
                );
            }
        }
    }
    Ok(())
}

/// Incrementally update one repo. `root` MUST be canonical (the
/// prior-manifest lookup path has to match `pipeline::update`'s internal
/// one). Returns `None` only when `--no-wait` lost the build lock.
fn update_one(
    root: &Path,
    cfg: &VexConfig,
    excludes: &[String],
    flags: &UpdateFlags,
    no_wait: bool,
) -> Result<Option<UpdateStats>> {
    let with_semantic = resolve_semantic(flags.semantic, flags.no_semantic, cfg);
    let embedder_id = resolve_embedder(flags.embedder.as_deref(), cfg);
    // `update` consults the previous manifest so an unflagged call does not
    // silently re-add a section the user opted out of. `Manifest::load`
    // returns `Ok(default)` for the missing-file case, so any `Err` is a
    // parse/IO failure we must not swallow.
    let prior_manifest = crate::index::manifest::Manifest::load(&config::manifest_path(root))?;
    let mut opts = build_index_options(
        with_semantic,
        flags.no_call_graph,
        flags.no_bm25,
        flags.no_pattern_index,
        flags.history,
        None, // --history-depth is index-only; update inherits via manifest
        flags.no_history,
        cfg,
        Some(&prior_manifest),
    );
    let cli_gpu = flags.gpu.then_some(true).or(flags.no_gpu.then_some(false));
    opts.device = crate::embed::Device::resolve(
        flags.device.as_deref(),
        cli_gpu,
        cfg.device.as_deref(),
        cfg.gpu,
    )?;
    opts.gpu_explicit = flags.device.is_some() || matches!(cli_gpu, Some(true));

    if no_wait {
        pipeline::update_or_busy(root, opts, &embedder_id, excludes)
    } else {
        Ok(Some(pipeline::update(root, opts, &embedder_id, excludes)?))
    }
}

/// `vex update --workspace`: incrementally update every member of the
/// nearest `.vex-workspace.toml`, reporting per-repo changes. Each member
/// uses its own `.vex.toml` + prior manifest.
///
/// Membership note: a declared member whose path no longer exists is
/// rejected at workspace load (canonicalize fails). Detecting index dirs
/// orphaned by a *removed* member is not done — `index_dir` is keyed by
/// canonical path on a cache shared with standalone `vex index`, so an
/// "orphaned" dir may still be a live standalone index; auto-cleaning it
/// would be unsafe. See `docs/LIMITATIONS.md` §7.
fn update_workspace(
    ctx: &CmdCtx<'_>,
    path: Option<std::path::PathBuf>,
    flags: &UpdateFlags,
    no_wait: bool,
) -> Result<()> {
    // Multi-repo Phase 2: per-member cache layouts come from the installed
    // resolver; `update_one` derives every path via `config::*_path(m.root)`,
    // which honours the member's own layout. The unsafe workspace-root
    // hash-less case is rejected in `cli::build_workspace_resolver`.
    let start_dir = resolve_root(path)?;
    let ws = workspace::Workspace::find_and_load(&start_dir)?;
    let base = ws.base().to_path_buf();

    let start = Instant::now();
    let mut per_repo: Vec<(String, Option<UpdateStats>)> = Vec::with_capacity(ws.members.len());
    for m in &ws.members {
        let member_cfg = config::load_config(&m.root)?;
        let outcome = update_one(&m.root, &member_cfg, &member_cfg.exclude, flags, no_wait)?;
        per_repo.push((m.display_name.clone(), outcome));
    }
    let elapsed = start.elapsed();
    let total_changed: usize = per_repo
        .iter()
        .filter_map(|(_, o)| o.map(|(_, c, _)| c))
        .sum();

    match ctx.format {
        OutputFormat::Json => {
            let repos: Vec<_> = per_repo
                .iter()
                .map(|(repo, outcome)| match outcome {
                    Some((total, changed, deleted)) => serde_json::json!({
                        "repo": repo,
                        "symbols": total,
                        "changed": changed,
                        "deleted": deleted,
                    }),
                    None => serde_json::json!({ "repo": repo, "status": "busy" }),
                })
                .collect();
            print_envelope(
                serde_json::json!({
                    "workspace": ws.file.to_string_lossy(),
                    "repos": repos,
                    "total_changed": total_changed,
                    "elapsed_ms": elapsed.as_millis(),
                }),
                capabilities::current(),
                super::output::default_meta_for(&base),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            println!(
                "Updated workspace {} ({} repos):",
                ws.file.display(),
                ws.members.len()
            );
            for (repo, outcome) in &per_repo {
                match outcome {
                    Some((total, changed, deleted)) if *changed == 0 && *deleted == 0 => {
                        println!("  {repo}: up to date ({total} symbols)");
                    }
                    Some((total, changed, deleted)) => {
                        println!("  {repo}: {changed} changed, {deleted} deleted, {total} total");
                    }
                    None => println!("  {repo}: skipped (another vex instance is updating)"),
                }
            }
            println!("Total: {total_changed} changed in {elapsed:.2?}");
        }
    }
    Ok(())
}
