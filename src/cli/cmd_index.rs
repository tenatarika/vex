//! `vex index` — one-shot index build from scratch. Extracted from
//! `cli/mod.rs` in S1 Group C. `--workspace` (multi-repo) fans the same
//! per-repo build over every member of a `.vex-workspace.toml`.

use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Result};

use super::args::OutputFormat;
use super::common::{
    build_index_options, resolve_embedder, resolve_root, resolve_semantic, CmdCtx,
};
use super::output::print_envelope;
use crate::index::pipeline;
use crate::protocol::capabilities;
use crate::util::config::{self, VexConfig};
use crate::workspace;

/// CLI build flags shared by every repo in a run (single or workspace).
/// Bundled so the per-root core and the two entry points don't each carry
/// the full positional list. Module-private — only the three fns below
/// touch it.
struct IndexFlags {
    semantic: bool,
    no_semantic: bool,
    drop_semantic: bool,
    embedder: Option<String>,
    no_call_graph: bool,
    no_bm25: bool,
    no_pattern_index: bool,
    history: bool,
    history_depth: Option<usize>,
    gpu: bool,
    no_gpu: bool,
    device: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn index(
    ctx: &CmdCtx<'_>,
    path: Option<std::path::PathBuf>,
    semantic: bool,
    no_semantic: bool,
    drop_semantic: bool,
    embedder: Option<String>,
    _jobs: Option<usize>,
    no_call_graph: bool,
    no_bm25: bool,
    no_pattern_index: bool,
    no_wait: bool,
    history: bool,
    history_depth: Option<usize>,
    gpu: bool,
    no_gpu: bool,
    device: Option<String>,
    workspace: bool,
) -> Result<()> {
    let flags = IndexFlags {
        semantic,
        no_semantic,
        drop_semantic,
        embedder,
        no_call_graph,
        no_bm25,
        no_pattern_index,
        history,
        history_depth,
        gpu,
        no_gpu,
        device,
    };

    if workspace {
        return index_workspace(ctx, path, &flags, no_wait);
    }

    let root = resolve_root(path)?;
    let start = Instant::now();
    let outcome = run_for_root(
        &root,
        ctx.cfg,
        ctx.excludes,
        &flags,
        no_wait,
        ctx.local_cache_active,
    )?;

    let Some((count, _rebuilt)) = outcome else {
        // --no-wait + lock held: emit a structured "busy" outcome and exit
        // 0 (matches `git pull`'s "Already up to date." UX — the user
        // explicitly opted into no-op-when-busy semantics, so this is not
        // a failure).
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
                println!("Skipped: another vex instance is indexing (--no-wait).");
            }
        }
        return Ok(());
    };

    let elapsed = start.elapsed();
    let with_semantic = resolve_semantic(flags.semantic, flags.no_semantic, ctx.cfg);
    let index_path = config::index_path(&root.canonicalize()?);

    match ctx.format {
        OutputFormat::Json => {
            let json = serde_json::json!({
                "symbols": count,
                "elapsed_ms": elapsed.as_millis(),
                "embeddings": with_semantic,
                "index": index_path.to_string_lossy(),
            });
            print_envelope(
                &json,
                capabilities::current(),
                super::output::default_meta_for(&root),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            println!("Indexed {count} symbols in {elapsed:.2?}");
            if with_semantic {
                println!("Embeddings: enabled");
            }
            println!("Index: {}", index_path.display());
        }
    }
    Ok(())
}

/// Build options for one repo from its own config + the shared CLI flags,
/// then run the pipeline. Returns `None` only when `--no-wait` lost the
/// build lock. `local_cache_active` is always false in workspace mode.
fn run_for_root(
    root: &Path,
    cfg: &VexConfig,
    excludes: &[String],
    flags: &IndexFlags,
    no_wait: bool,
    local_cache_active: bool,
) -> Result<Option<(usize, bool)>> {
    let with_semantic = resolve_semantic(flags.semantic, flags.no_semantic, cfg);
    let embedder_id = resolve_embedder(flags.embedder.as_deref(), cfg);
    // Resolve the embedding device (CLI > .vex.toml > VEX_DEVICE > compile-time
    // default). `gpu_explicit` is set only for an explicit CLI request so it
    // bypasses the miss-count gate; `.vex.toml gpu = true` stays gated.
    let cli_gpu = flags.gpu.then_some(true).or(flags.no_gpu.then_some(false));
    let resolved_device = crate::embed::Device::resolve(
        flags.device.as_deref(),
        cli_gpu,
        cfg.device.as_deref(),
        cfg.gpu,
    )?;
    let gpu_explicit = flags.device.is_some() || matches!(cli_gpu, Some(true));
    if local_cache_active {
        let cache_root = config::index_dir(root);
        std::fs::create_dir_all(&cache_root).ok();
        config::write_local_cache_gitignore(&cache_root);
    }
    // Fresh `vex index` ignores any prior manifest (it's about to be
    // overwritten). CLI flag > .vex.toml > default(true).
    let mut opts = build_index_options(
        with_semantic,
        flags.no_call_graph,
        flags.no_bm25,
        flags.no_pattern_index,
        flags.history,
        flags.history_depth,
        false, // `vex index` is a clean rebuild — no `--no-history` semantics
        cfg,
        None,
    );
    // v1.15.1: `--drop-semantic` requires `--no-semantic` (clap-enforced),
    // so `with_semantic` is guaranteed false here when `drop_semantic` is
    // true. The flag is request-scoped — never persisted into the manifest.
    opts.drop_semantic = flags.drop_semantic;
    opts.device = resolved_device;
    opts.gpu_explicit = gpu_explicit;

    if no_wait {
        pipeline::run_or_busy(root, opts, &embedder_id, excludes)
    } else {
        Ok(Some(pipeline::run(root, opts, &embedder_id, excludes)?))
    }
}

/// `vex index --workspace`: index every member of the nearest
/// `.vex-workspace.toml`. Each member uses its OWN `.vex.toml` for build
/// settings (excludes / embedder / sections) layered under the shared CLI
/// flags, and lands in its own per-repo index dir.
fn index_workspace(
    ctx: &CmdCtx<'_>,
    path: Option<std::path::PathBuf>,
    flags: &IndexFlags,
    no_wait: bool,
) -> Result<()> {
    // A hash-less cache layout (`local_cache` / a bare `--cache-dir`) would
    // collapse every member into one index dir. Refuse rather than corrupt.
    if ctx.local_cache_active {
        bail!(
            "workspace mode does not support local_cache / a hash-less cache dir — \
             members would collide into one index dir; use the platform cache"
        );
    }

    let start_dir = resolve_root(path)?;
    let ws = workspace::Workspace::find_and_load(&start_dir)?;
    let base = ws.base().to_path_buf();

    let start = Instant::now();
    let mut results = Vec::with_capacity(ws.members.len());
    for m in &ws.members {
        // Member's own .vex.toml (walking up to the workspace root for the
        // shared fallback) drives excludes/embedder/sections. It cannot set
        // a cache override — rejected at workspace load — so `index_dir`'s
        // global (platform) layout still applies.
        let member_cfg = config::load_config(&m.root)?;
        // `local_cache_active` is hard-coded false here: the cache layout is
        // fixed process-globally at dispatch (set_cache_override is a
        // OnceLock from the workspace-root config) and the local_cache guard
        // above already bailed if it was hash-less. `run_for_root` must NOT
        // re-derive the layout from `member_cfg`, or members could collide.
        let outcome = run_for_root(
            &m.root,
            &member_cfg,
            &member_cfg.exclude,
            flags,
            no_wait,
            false,
        )?;
        results.push((
            m.display_name.clone(),
            outcome.map(|(c, _)| c),
            m.index_dir(),
        ));
    }
    let elapsed = start.elapsed();
    let total: usize = results.iter().filter_map(|(_, c, _)| *c).sum();

    match ctx.format {
        OutputFormat::Json => {
            let repos: Vec<_> = results
                .iter()
                .map(|(name, count, dir)| {
                    serde_json::json!({
                        "repo": name,
                        "symbols": count,
                        "index": dir.to_string_lossy(),
                    })
                })
                .collect();
            print_envelope(
                serde_json::json!({
                    "workspace": ws.file.to_string_lossy(),
                    "repos": repos,
                    "total_symbols": total,
                    "elapsed_ms": elapsed.as_millis(),
                }),
                capabilities::current(),
                super::output::default_meta_for(&base),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            println!(
                "Indexed workspace {} ({} repos):",
                ws.file.display(),
                ws.members.len()
            );
            for (name, count, _) in &results {
                match count {
                    Some(c) => println!("  {name}: {c} symbols"),
                    None => println!("  {name}: skipped (another vex instance is indexing)"),
                }
            }
            println!("Total: {total} symbols in {elapsed:.2?}");
        }
    }
    Ok(())
}
