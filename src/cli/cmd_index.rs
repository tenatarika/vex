//! `vex index` — one-shot index build from scratch. Extracted from
//! `cli/mod.rs` in S1 Group C.

use std::time::Instant;

use anyhow::Result;

use super::args::OutputFormat;
use super::common::{
    build_index_options, resolve_embedder, resolve_root, resolve_semantic, CmdCtx,
};
use super::output::print_envelope;
use crate::index::pipeline;
use crate::protocol::capabilities;
use crate::util::config;

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
) -> Result<()> {
    let root = resolve_root(path)?;
    let start = Instant::now();
    let with_semantic = resolve_semantic(semantic, no_semantic, ctx.cfg);
    let embedder_id = resolve_embedder(embedder.as_deref(), ctx.cfg);
    // Resolve the embedding device (CLI > .vex.toml > VEX_DEVICE > compile-time
    // default). `gpu_explicit` is set only for an explicit CLI request so it
    // bypasses the miss-count gate; `.vex.toml gpu = true` stays gated.
    let cli_gpu = gpu.then_some(true).or(no_gpu.then_some(false));
    let resolved_device = crate::embed::Device::resolve(
        device.as_deref(),
        cli_gpu,
        ctx.cfg.device.as_deref(),
        ctx.cfg.gpu,
    )?;
    let gpu_explicit = device.is_some() || matches!(cli_gpu, Some(true));
    if ctx.local_cache_active {
        let cache_root = config::index_dir(&root);
        std::fs::create_dir_all(&cache_root).ok();
        config::write_local_cache_gitignore(&cache_root);
    }
    // Fresh `vex index` ignores any prior manifest (it's about to be
    // overwritten). CLI flag > .vex.toml > default(true).
    let mut opts = build_index_options(
        with_semantic,
        no_call_graph,
        no_bm25,
        no_pattern_index,
        history,
        history_depth,
        false, // `vex index` is a clean rebuild — no `--no-history` semantics
        ctx.cfg,
        None,
    );
    // v1.15.1: `--drop-semantic` requires `--no-semantic` (clap-enforced),
    // so `with_semantic` is guaranteed false here when `drop_semantic` is
    // true. The flag is request-scoped — never persisted into the manifest.
    opts.drop_semantic = drop_semantic;
    opts.device = resolved_device;
    opts.gpu_explicit = gpu_explicit;
    let outcome = if no_wait {
        pipeline::run_or_busy(&root, opts, &embedder_id, ctx.excludes)?
    } else {
        Some(pipeline::run(&root, opts, &embedder_id, ctx.excludes)?)
    };

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
