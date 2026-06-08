//! Shared, stateless helpers used by `cli::dispatch` and the per-command
//! handler modules. These were extracted from `cli/mod.rs` in S1 to make
//! per-command handlers self-contained — see
//! `.claude/Task/S1-cli-mod-decomposition.md`.
//!
//! Everything here is `pub(crate)`: callable from any `cli::cmd_*` module
//! but not exposed beyond the crate boundary.

use anyhow::{Context, Result};

use super::args::{self, Commands, OutputFormat};
use super::scope;
use crate::index::manifest::Manifest;
use crate::index::pipeline;
use crate::util::config;

/// Shared dispatch-level state. Built once in `dispatch()` from the
/// loaded `.vex.toml`, the resolved CLI flags, and the cache-override
/// outcome, then threaded into every extracted `cmd_*` handler. Cuts
/// 3–4 args off each handler signature and matches the architect's
/// MUST-FIX-#3 recommendation from the S1 plan review.
pub(crate) struct CmdCtx<'a> {
    pub cfg: &'a config::VexConfig,
    pub format: OutputFormat,
    pub excludes: &'a [String],
    pub local_cache_active: bool,
}

pub(crate) fn resolve_root(path: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("get working directory"),
    }
}

/// Extract the `-j/--jobs` hint from a subcommand for the rayon-pool
/// initialization. Only the three indexing commands carry the flag.
pub(crate) fn extract_jobs_hint(cmd: &Commands) -> Option<usize> {
    match cmd {
        Commands::Index { jobs, .. }
        | Commands::Update { jobs, .. }
        | Commands::Watch { jobs, .. } => *jobs,
        _ => None,
    }
}

/// Extract the --path hint from a subcommand for config loading.
pub(crate) fn extract_path_hint(cmd: &Commands) -> Option<std::path::PathBuf> {
    match cmd {
        Commands::Index { path, .. }
        | Commands::Update { path, .. }
        | Commands::Watch { path, .. }
        | Commands::Grep { path, .. }
        | Commands::Status { path, .. }
        | Commands::Implementations { path, .. }
        | Commands::Callers { path, .. }
        | Commands::Callees { path, .. }
        | Commands::Diff { path, .. }
        | Commands::Paths { path, .. }
        | Commands::Reachable { path, .. }
        | Commands::Check { path, .. }
        | Commands::Pattern { path, .. }
        | Commands::Similar { path, .. }
        | Commands::Duplicates { path, .. }
        | Commands::Eval { path, .. }
        | Commands::Bundle { path, .. } => path.clone(),
        _ => None,
    }
}

/// Resolve semantic flag: --semantic wins, --no-semantic wins, else config, else false.
pub(crate) fn resolve_semantic(
    cli_semantic: bool,
    cli_no_semantic: bool,
    cfg: &config::VexConfig,
) -> bool {
    if cli_semantic {
        true
    } else if cli_no_semantic {
        false
    } else {
        cfg.semantic.unwrap_or(false)
    }
}

/// Resolve embedder id: CLI flag wins, else .vex.toml, else DEFAULT.
pub(crate) fn resolve_embedder(cli_embedder: Option<&str>, cfg: &config::VexConfig) -> String {
    crate::embed::resolve_embedder(cli_embedder, cfg.embedder.as_deref())
}

/// Resolve whether an index section should be built.
///
/// Precedence (highest first):
///   1. CLI `--no-...` flag → forces `false`.
///   2. `.vex.toml` value → wins over the manifest so a project-wide
///      preference can override a one-off opt-out from a previous build.
///   3. Previous manifest value → sticky across `vex update`; without
///      this, a no-bm25 build would silently grow a BM25 section on the
///      next update.
///   4. Default `true`.
///
/// Used for both `call_graph` and `bm25`. For `vex index` callers, pass
/// `None` for `manifest_value` — the manifest is about to be overwritten.
pub(crate) fn resolve_section_enabled(
    cli_no_flag: bool,
    cfg_value: Option<bool>,
    manifest_value: Option<bool>,
) -> bool {
    if cli_no_flag {
        return false;
    }
    if let Some(v) = cfg_value {
        return v;
    }
    if let Some(v) = manifest_value {
        return v;
    }
    true
}

/// Verify that the embedder requested for semantic search matches the one
/// recorded in the manifest at `root`. Pre-9.1 manifests without
/// `embedder_id` are treated as the default embedder for back-compat.
pub(crate) fn check_embedder_match(root: &std::path::Path, requested: &str) -> Result<()> {
    let manifest_path = config::manifest_path(root);
    let manifest = crate::index::manifest::Manifest::load(&manifest_path)?;
    // Append manifest path so the user can `cat` it to inspect the
    // recorded embedder_id when the mismatch is surprising. The
    // embed-module bail message is generic on purpose.
    crate::embed::check_embedder_match(manifest.embedder_id.as_deref(), requested)
        .with_context(|| format!("manifest: {}", manifest_path.display()))
}

/// Resolve output format: CLI flag wins, else config, else Compact.
///
/// Compact is the default since v1.10.1 — single-line records, optimized for
/// LLM / agent token efficiency. The verbose multi-line `Text` form stays
/// available via `.vex.toml`'s `format = "text"` or `--format text`.
pub(crate) fn resolve_format(cli: Option<OutputFormat>, cfg: &config::VexConfig) -> OutputFormat {
    if let Some(f) = cli {
        return f;
    }
    match cfg.format.as_deref() {
        Some("json") => OutputFormat::Json,
        Some("text") => OutputFormat::Text,
        Some("compact") | None => OutputFormat::Compact,
        Some(other) => {
            eprintln!("warning: unknown format \"{other}\" in .vex.toml, using \"compact\"");
            OutputFormat::Compact
        }
    }
}

/// Extract the body of a symbol at `(path, line)` for `--explain` output.
/// Returns an empty string on any failure — `--explain` is a UX nicety
/// and should never abort the surrounding `similar` / `duplicates` run
/// just because one file is missing or unparseable. We surface a
/// one-line stderr warning so a regression (file deleted under the
/// index, language detection broken) is visible instead of silently
/// degrading the explanation to `jaccard 0.00`.
pub(crate) fn fetch_symbol_body(path: &str, line: usize, kind: &str) -> String {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "warning: could not read {path}:{line} for --explain ({e}); \
                 reasoning will be incomplete for this match"
            );
            return String::new();
        }
    };
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let body = if kind == "heading" {
        crate::parse::body::extract_heading_body(&content, line, 0)
    } else if let Some(lang) = crate::parse::language::Language::from_extension(ext) {
        crate::parse::body::extract_symbol_body_ts(&content, line, lang, 0)
    } else {
        crate::parse::body::extract_symbol_body(&content, line, 0)
    };
    match body {
        Ok(b) => b.body,
        Err(e) => {
            eprintln!(
                "warning: could not extract body for {path}:{line} ({e}); \
                 reasoning will be incomplete for this match"
            );
            String::new()
        }
    }
}

/// Diff lines beyond this cap are summarised as
/// `... (N more lines truncated)`. Picked to fit a single terminal
/// screenful without overwhelming compact output.
pub(crate) const EXPLAIN_MAX_DIFF_LINES: usize = 30;

/// Translate the CLI metadata flags into a `MetadataFilter`.
/// `--no-async` produces `async_required = Some(false)`; the
/// `conflicts_with` on the args struct keeps the combination
/// consistent with `--async-only`.
pub(crate) fn build_metadata_filter(
    meta: &args::MetadataArgs,
) -> Result<crate::search::metadata::MetadataFilter> {
    let visibility = meta
        .visibility
        .as_deref()
        .map(|v| v.parse::<crate::search::metadata::Visibility>())
        .transpose()?;
    let async_required = if meta.async_only {
        Some(true)
    } else if meta.no_async {
        Some(false)
    } else {
        None
    };
    let static_required = if meta.static_only { Some(true) } else { None };
    let sealed_required = if meta.sealed_only { Some(true) } else { None };
    Ok(crate::search::metadata::MetadataFilter {
        visibility,
        async_required,
        static_required,
        sealed_required,
    })
}

/// Build a `pipeline::IndexOptions` from the three CLI no-flags + config
/// + an optional prior manifest.
///
/// `manifest: None` means "this is a fresh `vex index` and there is no
/// prior manifest to consult" — section precedence collapses to
/// CLI flag > config > default(true).
///
/// Centralising this in one place keeps the Index / Update / Watch arms
/// (S1 Group C) from duplicating the 3-line `with_call_graph: ...,
/// with_bm25: ..., with_pattern_index: ...` construction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_index_options(
    with_semantic: bool,
    no_call_graph: bool,
    no_bm25: bool,
    no_pattern_index: bool,
    with_history: bool,
    history_depth: Option<usize>,
    no_history: bool,
    cfg: &config::VexConfig,
    manifest: Option<&Manifest>,
) -> pipeline::IndexOptions {
    // Phase 14.8 Step 5b sticky-via-sentinel: `vex update` without an
    // explicit flag inherits the prior manifest's history decision.
    // `--history` always wins; `--no-history` always wins.
    // Otherwise: prior `history_indexed_at = Some(_)` → with_history=true.
    let history_was_indexed = manifest
        .and_then(|m| m.history_indexed_at.as_ref())
        .is_some();
    let resolved_with_history = if no_history {
        false
    } else if with_history {
        true
    } else {
        history_was_indexed
    };
    // Sticky depth: inherit from manifest if user didn't pass --history-depth.
    let resolved_history_depth = history_depth.or_else(|| manifest.and_then(|m| m.history_depth));

    pipeline::IndexOptions {
        with_embeddings: with_semantic,
        with_call_graph: resolve_section_enabled(
            no_call_graph,
            cfg.call_graph,
            manifest.and_then(|m| m.call_graph),
        ),
        with_bm25: resolve_section_enabled(no_bm25, cfg.bm25, manifest.and_then(|m| m.bm25)),
        with_pattern_index: resolve_section_enabled(
            no_pattern_index,
            cfg.pattern_index,
            manifest.and_then(|m| m.pattern_index),
        ),
        with_history: resolved_with_history,
        history_depth: resolved_history_depth,
        // `--no-history` is only meaningful when there's something to
        // drop. Setting the flag without a prior section is harmless
        // (the pipeline checks for the sidecar before deleting).
        drop_history: no_history && history_was_indexed,
    }
}

pub(crate) fn apply_path_filters(
    results: Vec<crate::search::SearchResult>,
    filter: Option<&str>,
    scope: &scope::PathScope,
) -> Vec<crate::search::SearchResult> {
    if filter.is_none() && scope.is_empty() {
        return results;
    }
    results
        .into_iter()
        .filter(|r| filter.is_none_or(|fp| r.path.contains(fp)) && scope.accept(&r.path))
        .collect()
}

/// Phase 13.7-D3: resolve the diff scope once per invocation and return a
/// concrete `ChangedPaths` set, or `None` when no diff flag was passed.
///
/// Keeping resolution at the CLI boundary means each match arm pays at most
/// one `git` round-trip even when post-filter loops over thousands of
/// results.
pub(crate) fn resolve_diff_filter(
    repo_root: &std::path::Path,
    diff: &args::DiffFilterArgs,
) -> Result<Option<crate::util::git_diff::ChangedPaths>> {
    match diff.scope() {
        Some(scope) => Ok(Some(crate::util::git_diff::ChangedPaths::resolve(
            repo_root, scope,
        )?)),
        None => Ok(None),
    }
}

/// Build the JSON `_meta["vex.dev/diff_filter"]` block when a diff filter
/// was active. Returned as a `serde_json::Value` so callers can merge it
/// into the search-envelope's `_meta` payload without coupling protocol
/// types to the CLI layer.
pub(crate) fn diff_filter_meta(
    diff: &args::DiffFilterArgs,
    changed: Option<&crate::util::git_diff::ChangedPaths>,
    retained: usize,
    dropped: usize,
) -> Option<serde_json::Value> {
    let scope = diff.scope()?;
    let changed_paths = changed.map(|c| c.len()).unwrap_or(0);
    Some(serde_json::json!({
        "scope": scope.label(),
        "changed_paths": changed_paths,
        "retained": retained,
        "dropped": dropped,
    }))
}

#[cfg(test)]
mod tests {
    use super::resolve_section_enabled;

    #[test]
    fn cli_no_flag_wins_over_everything() {
        assert!(!resolve_section_enabled(true, Some(true), Some(true)));
        assert!(!resolve_section_enabled(true, None, None));
        assert!(!resolve_section_enabled(true, Some(true), None));
    }

    #[test]
    fn config_wins_over_manifest_and_default() {
        assert!(!resolve_section_enabled(false, Some(false), Some(true)));
        assert!(resolve_section_enabled(false, Some(true), Some(false)));
    }

    #[test]
    fn manifest_used_when_no_cli_or_config() {
        assert!(!resolve_section_enabled(false, None, Some(false)));
        assert!(resolve_section_enabled(false, None, Some(true)));
    }

    #[test]
    fn defaults_to_true_when_all_unset() {
        assert!(resolve_section_enabled(false, None, None));
    }
}
