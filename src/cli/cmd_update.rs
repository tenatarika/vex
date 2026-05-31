//! `vex update` — incremental rebuild honouring sticky section opt-outs
//! from the prior manifest. Extracted from `cli/mod.rs` in S1 Group C.

use std::time::Instant;

use anyhow::{Context, Result};

use super::args::OutputFormat;
use super::common::{
    build_index_options, resolve_embedder, resolve_root, resolve_semantic, CmdCtx,
};
use crate::index::pipeline;
use crate::util::config;

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
) -> Result<()> {
    // Canonicalize once at the top so `prior_manifest`'s lookup
    // path matches the one `pipeline::update` uses internally —
    // a divergent path would map to a different cache subdir and
    // silently drop the sticky-opt-out invariant.
    let root = resolve_root(path)?
        .canonicalize()
        .context("canonicalize project root")?;
    let start = Instant::now();
    let with_semantic = resolve_semantic(semantic, no_semantic, ctx.cfg);
    let embedder_id = resolve_embedder(embedder.as_deref(), ctx.cfg);
    // `update` consults the previous manifest so an unflagged call
    // does not silently re-add a section the user opted out of.
    // Surface real load errors via `?` — `Manifest::load` already
    // returns `Ok(default)` for the missing-file case, so any `Err`
    // here is a parse or IO failure we must not swallow.
    let prior_manifest = crate::index::manifest::Manifest::load(&config::manifest_path(&root))?;
    let opts = build_index_options(
        with_semantic,
        no_call_graph,
        no_bm25,
        no_pattern_index,
        ctx.cfg,
        Some(&prior_manifest),
    );
    let (total, changed, deleted) = pipeline::update(&root, opts, &embedder_id, ctx.excludes)?;
    let elapsed = start.elapsed();

    match ctx.format {
        OutputFormat::Json => {
            let json = serde_json::json!({
                "symbols": total,
                "changed": changed,
                "deleted": deleted,
                "elapsed_ms": elapsed.as_millis(),
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
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
