//! Index bootstrap + staleness orchestration extracted from `cli/mod.rs`
//! in S1. These helpers compose `pipeline::run` / `pipeline::update` with
//! the staleness probe so the per-command handlers stay focused on their
//! own work — see `.claude/Task/S1-cli-mod-decomposition.md`.

use anyhow::{bail, Context, Result};

use super::common::{resolve_embedder, resolve_section_enabled};
use crate::index::pipeline;
use crate::util::config;

/// Check index staleness and optionally auto-update.
///
/// Uses a cheap HEAD-only check by default (1 subprocess). Only runs the
/// expensive dirty-tree check when auto-update is enabled.
pub(crate) fn handle_staleness(
    root: &std::path::Path,
    auto_update_flag: bool,
    no_stale_check: bool,
    cfg: &config::VexConfig,
) -> Result<()> {
    if no_stale_check {
        return Ok(());
    }
    let manifest_path = config::manifest_path(root);
    let manifest = crate::index::manifest::Manifest::load(&manifest_path)?;
    let should_auto = auto_update_flag || cfg.auto_update.unwrap_or(false);
    // Deep check (dirty files) only when auto-update is on — avoids 2 extra subprocesses
    let freshness = crate::index::staleness::check(root, &manifest, should_auto);

    match freshness {
        crate::index::staleness::Freshness::Fresh => Ok(()),
        crate::index::staleness::Freshness::Unknown => {
            tracing::debug!(
                "cannot determine index freshness (no git_head/indexed_at in manifest)"
            );
            Ok(())
        }
        crate::index::staleness::Freshness::Stale { changed_count } => {
            if should_auto {
                let semantic = cfg.semantic.unwrap_or(false);
                let embedder_id = resolve_embedder(None, cfg);

                // Refuse to silently switch embedders during auto-update —
                // the user would not see the model change and the new index
                // would produce silently-wrong results for previously cached
                // queries. Force them to run `vex index --semantic` so the
                // intent is explicit.
                if semantic {
                    if let Some(stored) = manifest.embedder_id.as_deref() {
                        if stored != embedder_id {
                            bail!(
                                "auto-update would switch embedder from `{stored}` (manifest at {}) \
                                 to `{embedder_id}` (current config). Refusing — run \
                                 `vex index --semantic --embedder {embedder_id}` explicitly.",
                                manifest_path.display()
                            );
                        }
                    } else if embedder_id != crate::embed::DEFAULT_EMBEDDER {
                        // Manifest lost or pre-9.1 with no recorded embedder.
                        // We assume default for back-compat (see
                        // check_embedder_match), but config asks for non-default.
                        eprintln!(
                            "Warning: manifest has no recorded embedder; auto-update will \
                             rebuild with `{embedder_id}`. If the existing index was built \
                             with a different model the new search results will diverge."
                        );
                    }
                }

                eprintln!("Index stale, auto-updating...");
                // Inherit prior section composition from the manifest;
                // auto-update never silently grows new sections.
                let opts = pipeline::IndexOptions {
                    with_embeddings: semantic,
                    with_call_graph: resolve_section_enabled(
                        false,
                        cfg.call_graph,
                        manifest.call_graph,
                    ),
                    with_bm25: resolve_section_enabled(false, cfg.bm25, manifest.bm25),
                    with_pattern_index: resolve_section_enabled(
                        false,
                        cfg.pattern_index,
                        manifest.pattern_index,
                    ),
                };
                let (total, changed, deleted) =
                    pipeline::update(root, opts, &embedder_id, &cfg.exclude)?;
                if changed > 0 || deleted > 0 {
                    eprintln!(
                        "Updated: {changed} changed, {deleted} deleted, {total} total symbols"
                    );
                }
            } else if let Some(n) = changed_count {
                eprintln!("Warning: ~{n} file(s) changed since last index. Run `vex update`.");
            } else {
                eprintln!("Warning: index may be stale (HEAD changed). Run `vex update`.");
            }
            Ok(())
        }
    }
}

/// Outcome of [`ensure_index_exists`]: either the index already existed, or this
/// call bootstrapped it from scratch. Callers use [`IndexAvail::just_bootstrapped`]
/// to skip the redundant `handle_staleness` pass on a fresh manifest.
pub(crate) struct IndexAvail {
    pub(crate) path: std::path::PathBuf,
    pub(crate) just_bootstrapped: bool,
}

/// Resolve the index file path, bootstrapping the index in place when
/// the caller has `auto_update` set and no index exists yet. Replaces
/// the bare `if !index_path.exists() { bail!(...) }` pattern that every
/// index-backed command used to inline.
///
/// Returns an [`IndexAvail`] so the caller can immediately open the
/// index and also know whether this call bootstrapped it (used by
/// [`ensure_index_ready`] to skip a redundant staleness check on a
/// freshly built manifest). Callers that need a *semantic* index
/// (`Similar`, `Duplicates`) pass `needs_semantic = true` so the
/// bootstrap rebuilds with embeddings instead of structural-only.
///
/// `local_cache_active` mirrors the flag computed in `dispatch()` so we
/// can write the project-local `.gitignore` for `local_cache = true`
/// users on the *first* invocation — otherwise the bootstrap path
/// would skip the safeguard that `Commands::Index` applies.
pub(crate) fn ensure_index_exists(
    root: &std::path::Path,
    auto_update_flag: bool,
    needs_semantic: bool,
    local_cache_active: bool,
    cfg: &config::VexConfig,
) -> Result<IndexAvail> {
    let index_path = config::index_path(root);
    if index_path.exists() {
        return Ok(IndexAvail {
            path: index_path,
            just_bootstrapped: false,
        });
    }
    let should_auto = auto_update_flag || cfg.auto_update.unwrap_or(false);
    let cmd_hint = if needs_semantic {
        "vex index --semantic"
    } else {
        "vex index"
    };
    if !should_auto {
        bail!(
            "No index found.\n  Expected: {}\n  Run `{cmd_hint}` to build one, or set `auto_update = true` in .vex.toml to bootstrap on first use.",
            index_path.display()
        );
    }
    let with_semantic = needs_semantic || cfg.semantic.unwrap_or(false);
    eprintln!(
        "No index for this project yet — bootstrapping (auto_update = true).\nThis is a one-time cost; subsequent runs reuse the index."
    );
    if with_semantic {
        // The model is shared across projects, but a fresh machine
        // pays the download once. Warning here means the user sees an
        // explanation right before fastembed's progress bar appears.
        eprintln!(
            "Note: first semantic index downloads the MiniLM ONNX model (~86 MB) to the shared embedding cache."
        );
    }
    if local_cache_active {
        let cache_root = config::index_dir(root);
        std::fs::create_dir_all(&cache_root).ok();
        config::write_local_cache_gitignore(&cache_root);
    }
    let embedder_id = resolve_embedder(None, cfg);
    // Bootstrap honours `.vex.toml` section opt-outs but cannot consult a
    // previous manifest (this branch only fires when none exists).
    let opts = pipeline::IndexOptions {
        with_embeddings: with_semantic,
        with_call_graph: resolve_section_enabled(false, cfg.call_graph, None),
        with_bm25: resolve_section_enabled(false, cfg.bm25, None),
        with_pattern_index: resolve_section_enabled(false, cfg.pattern_index, None),
    };
    let count = pipeline::run(root, opts, &embedder_id, &cfg.exclude)
        .with_context(|| format!("bootstrap index for {}", root.display()))?;
    eprintln!(
        "Bootstrap complete: {count} symbols indexed{}.",
        if with_semantic {
            " with semantic embeddings"
        } else {
            ""
        }
    );
    Ok(IndexAvail {
        path: index_path,
        just_bootstrapped: true,
    })
}

/// Common bootstrap-then-staleness flow for index-backed commands.
///
/// Composes [`ensure_index_exists`] and [`handle_staleness`], skipping the latter
/// when the index was just bootstrapped (a freshly built manifest is guaranteed
/// fresh, so re-running the git/mtime probe is pure waste). Returns the resolved
/// `index.vex` path.
pub(crate) fn ensure_index_ready(
    root: &std::path::Path,
    auto_update_flag: bool,
    no_stale_check: bool,
    needs_semantic: bool,
    local_cache_active: bool,
    cfg: &config::VexConfig,
) -> Result<std::path::PathBuf> {
    let avail = ensure_index_exists(
        root,
        auto_update_flag,
        needs_semantic,
        local_cache_active,
        cfg,
    )?;
    if !avail.just_bootstrapped {
        handle_staleness(root, auto_update_flag, no_stale_check, cfg)?;
    }
    Ok(avail.path)
}
