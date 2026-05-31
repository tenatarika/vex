//! `vex watch` — long-running file-system watcher that runs an initial
//! index then keeps it incrementally updated. Extracted from
//! `cli/mod.rs` in S1 Group C; kept in a dedicated file so the watch
//! crate's tokio/signal imports don't leak into Index/Update.

use anyhow::{Context, Result};

use super::common::{build_index_options, resolve_embedder, resolve_root, resolve_semantic};
use crate::util::config;

#[allow(clippy::too_many_arguments)]
pub(crate) fn watch(
    path: Option<std::path::PathBuf>,
    semantic: bool,
    no_semantic: bool,
    embedder: Option<String>,
    _jobs: Option<usize>,
    no_call_graph: bool,
    no_bm25: bool,
    no_pattern_index: bool,
    cfg: &config::VexConfig,
    excludes: &[String],
) -> Result<()> {
    // Canonicalize once (see Update arm) so the manifest lookup
    // matches what `pipeline::run/update` will use.
    let root = resolve_root(path)?
        .canonicalize()
        .context("canonicalize project root")?;
    let with_semantic = resolve_semantic(semantic, no_semantic, cfg);
    let embedder_id = resolve_embedder(embedder.as_deref(), cfg);
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
        cfg,
        Some(&prior_manifest),
    );
    crate::watch::handler::watch(&root, opts, &embedder_id, excludes)?;
    Ok(())
}
