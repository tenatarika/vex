//! `vex watch` — long-running file-system watcher that runs an initial
//! index then keeps it incrementally updated. Extracted from
//! `cli/mod.rs` in S1 Group C; kept in a dedicated file so the watch
//! crate's tokio/signal imports don't leak into Index/Update.
//!
//! `--workspace` (multi-repo Phase 7) watches every `.vex-workspace.toml`
//! member at once, routing a changed file to its owning member's
//! incremental update. See `docs/MULTIREPO-PHASE7.md`.

use anyhow::{Context, Result};

use super::common::{
    build_index_options, resolve_embedder, resolve_root, resolve_semantic, CmdCtx,
};
use crate::util::config;
use crate::watch::handler::MemberWatch;
use crate::workspace;

#[allow(clippy::too_many_arguments)]
pub(crate) fn watch(
    ctx: &CmdCtx<'_>,
    path: Option<std::path::PathBuf>,
    semantic: bool,
    no_semantic: bool,
    embedder: Option<String>,
    _jobs: Option<usize>,
    no_call_graph: bool,
    no_bm25: bool,
    no_pattern_index: bool,
    workspace: bool,
) -> Result<()> {
    if workspace {
        return watch_workspace(
            ctx,
            path,
            semantic,
            no_semantic,
            embedder,
            no_call_graph,
            no_bm25,
            no_pattern_index,
        );
    }

    // Canonicalize once (see Update arm) so the manifest lookup
    // matches what `pipeline::run/update` will use.
    let root = resolve_root(path)?
        .canonicalize()
        .context("canonicalize project root")?;
    let with_semantic = resolve_semantic(semantic, no_semantic, ctx.cfg);
    let embedder_id = resolve_embedder(embedder.as_deref(), ctx.cfg);
    // Watch builds the initial index AND subsequent incremental
    // updates inside one long-running process. Both should use the
    // same composition. Real load errors surface via `?` —
    // `Manifest::load` already maps "file missing" to default.
    let prior_manifest = crate::index::manifest::Manifest::load(&config::manifest_path(&root))?;
    let opts = build_index_options(
        with_semantic,
        no_call_graph,
        no_bm25,
        no_pattern_index,
        false, // watch doesn't surface --history (uses sticky-via-manifest only)
        None,
        false, // --no-history not surfaced on watch
        ctx.cfg,
        Some(&prior_manifest),
    );
    crate::watch::handler::watch(&root, opts, &embedder_id, ctx.excludes)?;
    Ok(())
}

/// `vex watch --workspace`: build each member's initial index, then keep
/// every member incrementally fresh from one watcher (multi-repo Phase 7).
/// Each member's own `.vex.toml` drives its opts/embedder/excludes/cache
/// (the Phase 2 `CacheResolver` was installed at dispatch).
#[allow(clippy::too_many_arguments)]
fn watch_workspace(
    _ctx: &CmdCtx<'_>,
    path: Option<std::path::PathBuf>,
    semantic: bool,
    no_semantic: bool,
    embedder: Option<String>,
    no_call_graph: bool,
    no_bm25: bool,
    no_pattern_index: bool,
) -> Result<()> {
    let start_dir = resolve_root(path)?;
    let ws = workspace::Workspace::find_and_load(&start_dir)?;

    // Build one MemberWatch per member from its OWN config (+ shared flags +
    // its own prior manifest). `MemberWatch.root` is the canonical
    // `Member.root` verbatim — the routing invariant (events `starts_with`
    // the watched root) depends on it (docs/MULTIREPO-PHASE7.md §9).
    let mut members: Vec<MemberWatch> = Vec::with_capacity(ws.members.len());
    for m in &ws.members {
        let member_cfg = config::load_config(&m.root)?;
        let with_semantic = resolve_semantic(semantic, no_semantic, &member_cfg);
        let embedder_id = resolve_embedder(embedder.as_deref(), &member_cfg);
        let prior_manifest =
            crate::index::manifest::Manifest::load(&config::manifest_path(&m.root))?;
        let opts = build_index_options(
            with_semantic,
            no_call_graph,
            no_bm25,
            no_pattern_index,
            false,
            None,
            false,
            &member_cfg,
            Some(&prior_manifest),
        );
        members.push(MemberWatch {
            root: m.root.clone(),
            display_name: m.display_name.clone(),
            opts,
            embedder_id,
            excludes: member_cfg.exclude,
        });
    }

    crate::watch::handler::watch_workspace(members)?;
    Ok(())
}
