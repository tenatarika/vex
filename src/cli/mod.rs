pub mod args;
pub mod output;
pub mod scope;

use std::time::Instant;

use anyhow::{bail, Context, Result};
use args::{Cli, Commands, OutputFormat};
use clap::CommandFactory;

use crate::index::pipeline;
use crate::search::{fusion, semantic, structural};
use crate::store::reader::IndexReader;
use crate::util::config;

fn resolve_root(path: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("get working directory"),
    }
}

/// Extract the `-j/--jobs` hint from a subcommand for the rayon-pool
/// initialization. Only the three indexing commands carry the flag.
fn extract_jobs_hint(cmd: &Commands) -> Option<usize> {
    match cmd {
        Commands::Index { jobs, .. }
        | Commands::Update { jobs, .. }
        | Commands::Watch { jobs, .. } => *jobs,
        _ => None,
    }
}

/// Extract the --path hint from a subcommand for config loading.
fn extract_path_hint(cmd: &Commands) -> Option<std::path::PathBuf> {
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
        | Commands::Duplicates { path, .. } => path.clone(),
        _ => None,
    }
}

/// Resolve semantic flag: --semantic wins, --no-semantic wins, else config, else false.
fn resolve_semantic(cli_semantic: bool, cli_no_semantic: bool, cfg: &config::VexConfig) -> bool {
    if cli_semantic {
        true
    } else if cli_no_semantic {
        false
    } else {
        cfg.semantic.unwrap_or(false)
    }
}

/// Resolve embedder id: CLI flag wins, else .vex.toml, else DEFAULT.
fn resolve_embedder(cli_embedder: Option<&str>, cfg: &config::VexConfig) -> String {
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
fn resolve_section_enabled(
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
fn check_embedder_match(root: &std::path::Path, requested: &str) -> Result<()> {
    let manifest_path = config::manifest_path(root);
    let manifest = crate::index::manifest::Manifest::load(&manifest_path)?;
    // Append manifest path so the user can `cat` it to inspect the
    // recorded embedder_id when the mismatch is surprising. The
    // embed-module bail message is generic on purpose.
    crate::embed::check_embedder_match(manifest.embedder_id.as_deref(), requested)
        .with_context(|| format!("manifest: {}", manifest_path.display()))
}

/// Resolve output format: CLI flag wins, else config, else Text.
fn resolve_format(cli: Option<OutputFormat>, cfg: &config::VexConfig) -> OutputFormat {
    if let Some(f) = cli {
        return f;
    }
    match cfg.format.as_deref() {
        Some("json") => OutputFormat::Json,
        Some("compact") => OutputFormat::Compact,
        Some("text") | None => OutputFormat::Text,
        Some(other) => {
            eprintln!("warning: unknown format \"{other}\" in .vex.toml, using \"text\"");
            OutputFormat::Text
        }
    }
}

/// Check index staleness and optionally auto-update.
///
/// Uses a cheap HEAD-only check by default (1 subprocess). Only runs the
/// expensive dirty-tree check when auto-update is enabled.
fn handle_staleness(
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
struct IndexAvail {
    path: std::path::PathBuf,
    just_bootstrapped: bool,
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
fn ensure_index_exists(
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
fn ensure_index_ready(
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

/// Extract the body of a symbol at `(path, line)` for `--explain` output.
/// Returns an empty string on any failure — `--explain` is a UX nicety
/// and should never abort the surrounding `similar` / `duplicates` run
/// just because one file is missing or unparseable. We surface a
/// one-line stderr warning so a regression (file deleted under the
/// index, language detection broken) is visible instead of silently
/// degrading the explanation to `jaccard 0.00`.
fn fetch_symbol_body(path: &str, line: usize, kind: &str) -> String {
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
const EXPLAIN_MAX_DIFF_LINES: usize = 30;

/// Translate the CLI metadata flags into a `MetadataFilter`.
/// `--no-async` produces `async_required = Some(false)`; the
/// `conflicts_with` on the args struct keeps the combination
/// consistent with `--async-only`.
fn build_metadata_filter(
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

fn apply_path_filters(
    results: Vec<crate::search::SearchResult>,
    filter: Option<&str>,
    scope: &scope::PathScope,
) -> Vec<crate::search::SearchResult> {
    if filter.is_none() && scope.is_empty() {
        return results;
    }
    results
        .into_iter()
        .filter(|r| filter.map_or(true, |fp| r.path.contains(fp)) && scope.accept(&r.path))
        .collect()
}

pub fn dispatch(cli: Cli) -> Result<()> {
    // Load project config from .vex.toml — anchored to project root, not cwd
    let root_hint = extract_path_hint(&cli.command);
    let config_root = resolve_root(root_hint)?;
    let cfg = config::load_config(&config_root)?;
    let format = resolve_format(cli.format, &cfg);
    let excludes = &cfg.exclude;

    // Install the cache-root override (CLI > env > config > platform default).
    // Done once here so every config::index_path/index_dir call downstream
    // sees the resolved value without us threading it through 20+ call sites.
    let resolved_cache = config::resolve_cache_root(cli.cache_dir.as_deref(), &cfg);
    let local_cache_active = resolved_cache.skip_hash_subdir;
    config::set_cache_override(resolved_cache.root, resolved_cache.skip_hash_subdir);

    // Configure the global rayon pool before any par_iter runs.
    //   * Indexing commands (Index/Update/Watch) always init — that is
    //     where the 80% default earns its keep on long runs.
    //   * Non-indexing commands only init when the user has an explicit
    //     setting (CLI/env/config). Otherwise we leave rayon at its lazy
    //     default so a fast `vex check` / `vex search` does not spawn
    //     parked worker threads it never uses.
    let jobs_cli = extract_jobs_hint(&cli.command);
    let is_indexing_cmd = matches!(
        cli.command,
        Commands::Index { .. } | Commands::Update { .. } | Commands::Watch { .. }
    );
    if is_indexing_cmd {
        config::init_rayon_pool(config::resolve_jobs(jobs_cli, &cfg));
    } else if let Some(n) = config::resolve_explicit_jobs(jobs_cli, &cfg) {
        config::init_rayon_pool(n);
    }

    match cli.command {
        Commands::Index {
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
        } => {
            let root = resolve_root(path)?;
            let start = Instant::now();
            let with_semantic = resolve_semantic(semantic, no_semantic, &cfg);
            let embedder_id = resolve_embedder(embedder.as_deref(), &cfg);
            let _ = jobs; // honoured via dispatch-level rayon init
            if local_cache_active {
                let cache_root = config::index_dir(&root);
                std::fs::create_dir_all(&cache_root).ok();
                config::write_local_cache_gitignore(&cache_root);
            }
            // Fresh `vex index` ignores any prior manifest (it's about to
            // be overwritten). CLI flag > .vex.toml > default(true).
            let opts = pipeline::IndexOptions {
                with_embeddings: with_semantic,
                with_call_graph: resolve_section_enabled(no_call_graph, cfg.call_graph, None),
                with_bm25: resolve_section_enabled(no_bm25, cfg.bm25, None),
                with_pattern_index: resolve_section_enabled(
                    no_pattern_index,
                    cfg.pattern_index,
                    None,
                ),
            };
            let count = pipeline::run(&root, opts, &embedder_id, excludes)?;
            let elapsed = start.elapsed();
            let index_path = config::index_path(&root.canonicalize()?);

            match &format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "symbols": count,
                        "elapsed_ms": elapsed.as_millis(),
                        "embeddings": with_semantic,
                        "index": index_path.to_string_lossy(),
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
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
        Commands::Search {
            query,
            limit,
            semantic,
            no_semantic,
            filter_path,
            kind,
            context_path,
            auto_update,
            no_stale_check,
            no_bm25,
            meta,
            why,
            scope,
        } => {
            let semantic = resolve_semantic(semantic, no_semantic, &cfg);
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let metadata_filter = build_metadata_filter(&meta)?;
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                semantic,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;

            // Over-fetch when a path filter is active — the post-filter
            // `take(limit)` runs AFTER the results are produced, so a narrow
            // include/exclude or substring would silently truncate matches.
            // Bound by `symbol_count()`, not `usize::MAX`, because index-backed
            // results cannot exceed the symbol table.
            let fetch_limit = if filter_path.is_some() || !path_scope.is_empty() {
                reader.symbol_count()
            } else {
                limit
            };

            let structural_results = structural::search_with_fuzzy(&reader, &query, fetch_limit);

            // BM25 channel: auto-on when the index has BM25 data, opt-out
            // with `--no-bm25`. Returns empty for short queries or when no
            // term hits — safe to always run.
            let bm25_results = if !no_bm25 && reader.has_bm25() {
                crate::search::bm25::search(&reader, &query, fetch_limit)
            } else {
                Vec::new()
            };

            // Capture pre-fusion channel snapshots for `--why`. Clone only
            // when the flag is set so the fast path stays allocation-free.
            let trace_structural = if why {
                structural_results.clone()
            } else {
                Vec::new()
            };
            let trace_bm25 = if why {
                bm25_results.clone()
            } else {
                Vec::new()
            };
            let mut trace_semantic: Vec<crate::search::SearchResult> = Vec::new();

            let results = if semantic && reader.has_vectors() {
                let embedder_id = resolve_embedder(None, &cfg);
                // Warn (but don't fail) when the manifest doesn't record an
                // embedder — typically a pre-9.1 index or a deleted manifest.
                // We fall back to assuming DEFAULT_EMBEDDER; if the user
                // configured something else they'll see the warning and can
                // rebuild explicitly.
                let manifest =
                    crate::index::manifest::Manifest::load(&config::manifest_path(&root))?;
                if manifest.embedder_id.is_none() && embedder_id != crate::embed::DEFAULT_EMBEDDER {
                    eprintln!(
                        "Warning: index manifest has no recorded embedder; assuming \
                         `{}`. If the index was built with `{embedder_id}` the results \
                         may be off — rebuild with `vex index --semantic --embedder {embedder_id}` \
                         to make it explicit.",
                        crate::embed::DEFAULT_EMBEDDER
                    );
                }
                check_embedder_match(&root, &embedder_id)?;
                let mut embedder =
                    crate::embed::make_embedder(&embedder_id).context("load embedding model")?;
                let hnsw_path = config::hnsw_path(&root);
                let semantic_results = semantic::search_with_embedder(
                    &reader,
                    embedder.as_mut(),
                    &query,
                    fetch_limit,
                    &hnsw_path,
                )?;
                if why {
                    trace_semantic = semantic_results.clone();
                }
                fusion::fuse3(structural_results, bm25_results, semantic_results, limit)
            } else {
                if semantic && !reader.has_vectors() {
                    eprintln!("Warning: no embeddings in index. Run `vex index --semantic` first.");
                }
                if bm25_results.is_empty() {
                    structural_results
                } else {
                    // 2-channel fusion when semantic is off but BM25 is available.
                    fusion::fuse_many(vec![structural_results, bm25_results], limit)
                }
            };

            let rerank_ctx = crate::search::rerank::RerankContext {
                kind_hints: crate::search::rerank::KindSelector::parse_many(&kind)?,
                context_path: context_path.as_deref(),
            };
            let results = crate::search::rerank::rerank(&query, &rerank_ctx, results);
            let results: Vec<_> = apply_path_filters(results, filter_path.as_deref(), &path_scope)
                .into_iter()
                .filter(|r| metadata_filter.matches(r.signature.as_deref()))
                .take(limit)
                .collect();

            if why {
                let filter = crate::search::trace::FilterSnapshot {
                    filter: filter_path.clone(),
                    include: scope.include.clone(),
                    exclude: scope.exclude.clone(),
                    kind: kind.clone(),
                };
                let trace = crate::search::trace::SearchTrace::from_channels(
                    &query,
                    &trace_structural,
                    &trace_bm25,
                    &trace_semantic,
                    &results,
                    filter,
                );
                // stderr so `vex search Foo --why | jq` keeps working —
                // stdout stays a pure result list.
                eprintln!("{}", serde_json::to_string(&trace)?);
            }

            if results.is_empty() {
                match &format {
                    OutputFormat::Json => println!("[]"),
                    OutputFormat::Text | OutputFormat::Compact => {
                        println!("No results for \"{query}\"")
                    }
                }
            } else {
                let is_fuzzy = results
                    .iter()
                    .any(|r| matches!(r.match_type, crate::search::MatchType::Fuzzy));
                if is_fuzzy {
                    match &format {
                        OutputFormat::Text | OutputFormat::Compact => {
                            eprintln!("(fuzzy match — no exact results for \"{query}\")\n");
                        }
                        _ => {}
                    }
                }
                output::print_results(&results, &format);
            }
            Ok(())
        }
        Commands::Usages {
            name,
            limit,
            filter_path,
            auto_update,
            no_stale_check,
            strict,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                false,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;
            let ref_reader = reader
                .ref_reader()
                .context("no refs in index — re-run `vex index` to rebuild")?;
            let file_paths = reader.file_paths();

            // `--strict` reads from the v5 reference_edges section
            // (binder-resolved refs only). The legacy FST still backs
            // the non-strict path because it captures identifiers in
            // every supported language, including the 16 without a
            // scope binder yet.
            let entries: Vec<crate::store::refs_fst::RefEntry> = if strict {
                if !reader.has_ref_edges() {
                    anyhow::bail!(
                        "--strict needs a v5 index with reference_edges (this index is v{} or has no resolved refs). Re-run `vex index` to rebuild.",
                        reader.header().version
                    );
                }
                let sym_fst = reader
                    .symbol_fst_reader()
                    .context("symbol FST missing — re-run `vex index` to rebuild for --strict")?;
                let sym_indices = sym_fst.find(&name);
                let mut out = Vec::new();
                for sym_idx in sym_indices {
                    for edge in reader.find_ref_edges_by_symbol(sym_idx) {
                        out.push(crate::store::refs_fst::RefEntry {
                            file_id: edge.from_file_id,
                            line: edge.line,
                        });
                    }
                }
                out
            } else {
                ref_reader.find(&name)
            };
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|e| {
                    let path = match file_paths.get(e.file_id as usize) {
                        Some(p) => p.as_str(),
                        None => return false,
                    };
                    let filter_ok = filter_path.as_deref().map_or(true, |fp| path.contains(fp));
                    filter_ok && path_scope.accept(path)
                })
                .collect();
            let total = entries.len();
            let entries: Vec<_> = entries.into_iter().take(limit).collect();

            match &format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|e| {
                            let path = file_paths
                                .get(e.file_id as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("?");
                            serde_json::json!({
                                "path": path,
                                "line": e.line,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if entries.is_empty() {
                        println!("No usages found for \"{name}\"");

                        let prefix_results = ref_reader.find_by_prefix(&name);
                        if !prefix_results.is_empty() {
                            println!("\nDid you mean:");
                            for (n, refs) in prefix_results.iter().take(5) {
                                println!("  {n} ({} usages)", refs.len());
                            }
                        }
                    } else {
                        println!("{name}: {total} usages (showing {})", entries.len());
                        for e in &entries {
                            let path = file_paths
                                .get(e.file_id as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("?");
                            println!("  {path}:{}", e.line);
                        }
                    }
                }
            }
            Ok(())
        }
        Commands::Pattern {
            pattern,
            lang,
            path,
            limit,
            why,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?;
            let language = crate::parse::language::Language::from_extension(&lang)
                .or(match lang.as_str() {
                    "rust" => Some(crate::parse::language::Language::Rust),
                    "python" => Some(crate::parse::language::Language::Python),
                    "go" => Some(crate::parse::language::Language::Go),
                    "java" => Some(crate::parse::language::Language::Java),
                    "csharp" | "cs" => Some(crate::parse::language::Language::CSharp),
                    "ruby" | "rb" => Some(crate::parse::language::Language::Ruby),
                    "swift" => Some(crate::parse::language::Language::Swift),
                    "kotlin" | "kt" => Some(crate::parse::language::Language::Kotlin),
                    "typescript" | "ts" | "tsx" => {
                        Some(crate::parse::language::Language::TypeScript)
                    }
                    "sql" => Some(crate::parse::language::Language::Sql),
                    "markdown" | "md" => Some(crate::parse::language::Language::Markdown),
                    "cpp" | "c++" | "cxx" | "c" => Some(crate::parse::language::Language::Cpp),
                    "php" | "phtml" => Some(crate::parse::language::Language::Php),
                    "bash" | "sh" | "shell" => Some(crate::parse::language::Language::Bash),
                    "lua" => Some(crate::parse::language::Language::Lua),
                    "css" => Some(crate::parse::language::Language::Css),
                    "html" | "htm" => Some(crate::parse::language::Language::Html),
                    "yaml" | "yml" => Some(crate::parse::language::Language::Yaml),
                    "toml" => Some(crate::parse::language::Language::Toml),
                    _ => None,
                })
                .with_context(|| format!("unknown language: {lang}"))?;

            let start = Instant::now();
            // Over-fetch when scope filters are active so post-filter truncation
            // does not silently drop matches the user expects to see.
            let fetch_limit = if path_scope.is_empty() {
                limit
            } else {
                usize::MAX
            };

            let (raw_matches, trace) =
                crate::pattern::scan_with_mode(&root, &pattern, language, fetch_limit, excludes)?;

            let matches: Vec<_> = raw_matches
                .into_iter()
                .filter(|m| path_scope.accept(&m.path))
                .take(limit)
                .collect();
            let elapsed = start.elapsed();

            match &format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = matches
                        .iter()
                        .map(|m| {
                            let mut obj = serde_json::json!({
                                "path": m.path,
                                "line": m.line,
                                "text": m.matched_text.lines().next().unwrap_or(""),
                            });
                            if !m.captures.is_empty() {
                                obj["captures"] = serde_json::json!(m
                                    .captures
                                    .iter()
                                    .map(|(k, v)| serde_json::json!({k: v}))
                                    .collect::<Vec<_>>());
                            }
                            obj
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if matches.is_empty() {
                        println!("No matches for pattern in {elapsed:.2?}");
                    } else {
                        println!("{} matches in {elapsed:.2?}\n", matches.len());
                        for m in &matches {
                            let first_line = m.matched_text.lines().next().unwrap_or("");
                            println!("{}:{}", m.path, m.line);
                            println!("  {first_line}");
                            for (name, value) in &m.captures {
                                println!("  ${name} = {value}");
                            }
                            println!();
                        }
                    }
                }
            }

            if why {
                // stderr keeps stdout a pure result stream — mirrors
                // `vex search --why` so `vex pattern 'pat' --why | jq` works.
                eprintln!("{}", serde_json::to_string(&trace)?);
            }

            Ok(())
        }
        Commands::Update {
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
        } => {
            // Canonicalize once at the top so `prior_manifest`'s lookup
            // path matches the one `pipeline::update` uses internally —
            // a divergent path would map to a different cache subdir and
            // silently drop the sticky-opt-out invariant.
            let root = resolve_root(path)?
                .canonicalize()
                .context("canonicalize project root")?;
            let start = Instant::now();
            let with_semantic = resolve_semantic(semantic, no_semantic, &cfg);
            let embedder_id = resolve_embedder(embedder.as_deref(), &cfg);
            let _ = jobs; // honoured via dispatch-level rayon init
                          // `update` consults the previous manifest so an unflagged call
                          // does not silently re-add a section the user opted out of.
                          // Surface real load errors via `?` — `Manifest::load` already
                          // returns `Ok(default)` for the missing-file case, so any `Err`
                          // here is a parse or IO failure we must not swallow.
            let prior_manifest =
                crate::index::manifest::Manifest::load(&config::manifest_path(&root))?;
            let opts = pipeline::IndexOptions {
                with_embeddings: with_semantic,
                with_call_graph: resolve_section_enabled(
                    no_call_graph,
                    cfg.call_graph,
                    prior_manifest.call_graph,
                ),
                with_bm25: resolve_section_enabled(no_bm25, cfg.bm25, prior_manifest.bm25),
                with_pattern_index: resolve_section_enabled(
                    no_pattern_index,
                    cfg.pattern_index,
                    prior_manifest.pattern_index,
                ),
            };
            let (total, changed, deleted) = pipeline::update(&root, opts, &embedder_id, excludes)?;
            let elapsed = start.elapsed();

            match &format {
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
                        println!("Updated in {elapsed:.2?}: {changed} changed, {deleted} deleted, {total} total symbols");
                    }
                }
            }
            Ok(())
        }
        Commands::Outline { file, kind } => cmd_outline(&file, kind.as_deref(), &format),
        Commands::Watch {
            path,
            semantic,
            no_semantic,
            embedder,
            jobs,
            no_call_graph,
            no_bm25,
            no_pattern_index,
        } => {
            // Canonicalize once (see Update arm) so the manifest lookup
            // matches what `pipeline::run/update` will use.
            let root = resolve_root(path)?
                .canonicalize()
                .context("canonicalize project root")?;
            let with_semantic = resolve_semantic(semantic, no_semantic, &cfg);
            let embedder_id = resolve_embedder(embedder.as_deref(), &cfg);
            let _ = jobs; // honoured via dispatch-level rayon init
                          // Watch builds the initial index AND subsequent incremental
                          // updates inside one long-running process. Both should use
                          // the same composition. Real load errors surface via `?` —
                          // `Manifest::load` already maps "file missing" to default.
            let prior_manifest =
                crate::index::manifest::Manifest::load(&config::manifest_path(&root))?;
            let opts = pipeline::IndexOptions {
                with_embeddings: with_semantic,
                with_call_graph: resolve_section_enabled(
                    no_call_graph,
                    cfg.call_graph,
                    prior_manifest.call_graph,
                ),
                with_bm25: resolve_section_enabled(no_bm25, cfg.bm25, prior_manifest.bm25),
                with_pattern_index: resolve_section_enabled(
                    no_pattern_index,
                    cfg.pattern_index,
                    prior_manifest.pattern_index,
                ),
            };
            crate::watch::handler::watch(&root, opts, &embedder_id, excludes)?;
            Ok(())
        }
        Commands::Show {
            symbols,
            limit,
            context,
            filter_path,
            kind,
            context_path,
            auto_update,
            no_stale_check,
            meta,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let metadata_filter = build_metadata_filter(&meta)?;
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                false,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;
            let fetch_limit = if filter_path.is_some() || !path_scope.is_empty() {
                reader.symbol_count()
            } else {
                limit
            };
            let mut json_items: Vec<serde_json::Value> = Vec::new();
            let mut printed = 0usize;

            let rerank_ctx = crate::search::rerank::RerankContext {
                kind_hints: crate::search::rerank::KindSelector::parse_many(&kind)?,
                context_path: context_path.as_deref(),
            };

            for symbol in &symbols {
                let results = structural::search_with_fuzzy(&reader, symbol, fetch_limit);
                let results = crate::search::rerank::rerank(symbol, &rerank_ctx, results);
                let results: Vec<_> =
                    apply_path_filters(results, filter_path.as_deref(), &path_scope)
                        .into_iter()
                        .filter(|r| metadata_filter.matches(r.signature.as_deref()))
                        .take(limit)
                        .collect();

                if results.is_empty() {
                    match &format {
                        OutputFormat::Json => {}
                        OutputFormat::Text | OutputFormat::Compact => {
                            if printed > 0 {
                                println!();
                            }
                            println!("No symbol found: \"{symbol}\"");
                            printed += 1;
                        }
                    }
                    continue;
                }

                for result in &results {
                    let content = std::fs::read_to_string(&result.path)
                        .with_context(|| format!("read {}", result.path))?;

                    let ext = std::path::Path::new(&result.path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("");

                    let body = if result.kind == "heading" {
                        crate::parse::body::extract_heading_body(&content, result.line, context)?
                    } else if let Some(lang) = crate::parse::language::Language::from_extension(ext)
                    {
                        crate::parse::body::extract_symbol_body_ts(
                            &content,
                            result.line,
                            lang,
                            context,
                        )?
                    } else {
                        crate::parse::body::extract_symbol_body(&content, result.line, context)?
                    };

                    match &format {
                        OutputFormat::Json => {
                            json_items.push(serde_json::json!({
                                "name": result.name,
                                "kind": result.kind,
                                "path": result.path,
                                "start_line": body.start_line,
                                "end_line": body.end_line,
                                "lines": body.lines,
                                "body": body.body,
                            }));
                        }
                        OutputFormat::Text => {
                            if printed > 0 {
                                println!();
                            }
                            println!(
                                "── {} ({}) {}:{}-{}",
                                result.name,
                                result.kind,
                                result.path,
                                body.start_line,
                                body.end_line
                            );
                            for (n, line) in body.body.lines().enumerate() {
                                println!("{:>4} | {}", body.start_line + n, line);
                            }
                            printed += 1;
                        }
                        OutputFormat::Compact => {
                            if printed > 0 {
                                println!();
                            }
                            println!(
                                "# {}:{}-{} ({})",
                                result.path, body.start_line, body.end_line, result.kind
                            );
                            println!("{}", body.body);
                            printed += 1;
                        }
                    }
                }
            }

            match &format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&json_items)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if printed == 0 {
                        println!("No symbols found");
                    }
                }
            }
            Ok(())
        }
        Commands::Status { path } => {
            let root = resolve_root(path)?
                .canonicalize()
                .context("canonicalize root")?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                match &format {
                    OutputFormat::Json => {
                        println!("{}", serde_json::json!({"error": "no index found"}));
                    }
                    OutputFormat::Text | OutputFormat::Compact => {
                        println!("No index found for {}", root.display());
                        println!("Run `vex index` to build one.");
                    }
                }
                return Ok(());
            }

            let meta = std::fs::metadata(&index_path)?;
            let reader = IndexReader::open(&index_path)?;

            match &format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "project": root.to_string_lossy(),
                        "index": index_path.to_string_lossy(),
                        "size_bytes": meta.len(),
                        "symbols": reader.symbol_count(),
                        "embeddings": reader.has_vectors(),
                        "call_graph": reader.has_call_graph(),
                        "bm25": reader.has_bm25(),
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    println!("Project:    {}", root.display());
                    println!("Index:      {}", index_path.display());
                    println!("Size:       {:.1} KB", meta.len() as f64 / 1024.0);
                    println!("Symbols:    {}", reader.symbol_count());
                    println!(
                        "Embeddings: {}",
                        if reader.has_vectors() { "yes" } else { "no" }
                    );
                    println!(
                        "Call graph: {}",
                        if reader.has_call_graph() { "yes" } else { "no" }
                    );
                    println!(
                        "BM25:       {}",
                        if reader.has_bm25() { "yes" } else { "no" }
                    );
                }
            }
            Ok(())
        }
        Commands::Grep {
            pattern,
            limit,
            filter_path,
            path,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?;
            // Over-fetch when scope filters are active so post-filter truncation
            // does not silently drop matches the user expects to see.
            let fetch_limit = if path_scope.is_empty() {
                limit
            } else {
                usize::MAX
            };
            let matches = crate::grep::search(
                &root,
                &pattern,
                filter_path.as_deref(),
                fetch_limit,
                excludes,
            )?;
            let matches: Vec<_> = matches
                .into_iter()
                .filter(|m| path_scope.accept(&m.path))
                .take(limit)
                .collect();

            match &format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = matches
                        .iter()
                        .map(|m| {
                            serde_json::json!({
                                "path": m.path,
                                "line": m.line,
                                "text": m.text,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text => {
                    if matches.is_empty() {
                        println!("No matches for \"{pattern}\"");
                    } else {
                        println!("{} matches\n", matches.len());
                        for m in &matches {
                            println!("{}:{}", m.path, m.line);
                            println!("  {}", m.text);
                        }
                    }
                }
                OutputFormat::Compact => {
                    for m in &matches {
                        println!("{}:{}  {}", m.path, m.line, m.text);
                    }
                }
            }
            Ok(())
        }
        Commands::Implementations {
            name,
            path,
            limit,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?;
            let start = Instant::now();
            let fetch_limit = if path_scope.is_empty() {
                limit
            } else {
                usize::MAX
            };
            let matches =
                crate::hierarchy::find_implementations(&root, &name, fetch_limit, excludes)?;
            let matches: Vec<_> = matches
                .into_iter()
                .filter(|m| path_scope.accept(&m.path))
                .take(limit)
                .collect();
            let elapsed = start.elapsed();

            match &format {
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
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text => {
                    if matches.is_empty() {
                        println!("No implementations of \"{name}\" in {elapsed:.2?}");
                    } else {
                        println!(
                            "{name}: {} implementations in {elapsed:.2?}\n",
                            matches.len()
                        );
                        for m in &matches {
                            println!("  {:<40} ({})  {}:{}", m.name, m.relation, m.path, m.line);
                        }
                    }
                }
                OutputFormat::Compact => {
                    for m in &matches {
                        println!("{} {} {} {}:{}", m.relation, m.base, m.name, m.path, m.line);
                    }
                }
            }
            Ok(())
        }
        Commands::Callers {
            name,
            path,
            limit,
            auto_update,
            no_stale_check,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            cmd_callgraph(
                &name,
                path,
                limit,
                true,
                auto_update,
                no_stale_check,
                local_cache_active,
                &cfg,
                &format,
                excludes,
                &path_scope,
            )
        }
        Commands::Callees {
            name,
            path,
            limit,
            auto_update,
            no_stale_check,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            cmd_callgraph(
                &name,
                path,
                limit,
                false,
                auto_update,
                no_stale_check,
                local_cache_active,
                &cfg,
                &format,
                excludes,
                &path_scope,
            )
        }
        Commands::Diff {
            base,
            path,
            limit,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?.canonicalize()?;
            let changes = crate::diff::diff_against_base(&root, &base, excludes, limit)?;
            let changes: Vec<_> = changes
                .into_iter()
                .filter(|c| path_scope.accept(&c.path))
                .collect();
            output::print_diff(&changes, &base, &format);
            Ok(())
        }
        Commands::Paths {
            from,
            to,
            max_hops,
            max_paths,
            path,
            auto_update,
            no_stale_check,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                false,
                local_cache_active,
                &cfg,
            )?;
            let reader = IndexReader::open(&index_path).context("open index")?;
            if !reader.has_call_graph() {
                bail!(
                    "no call graph in index — `vex paths` requires a v4 index built without \
                     `--no-call-graph`. Rebuild with `vex index` (or `vex index --auto-update`)."
                );
            }
            // Closure binds the reader so the BFS layer stays generic
            // over an in-memory mock for unit tests. The per-step fetch
            // cap (1024) is well above any realistic codebase fan-in;
            // when we see a node that hits it, we surface a warning so
            // a real saturation event is visible instead of silently
            // dropping callers.
            const CALLERS_FETCH_CAP: usize = 1024;
            let callers_of = |name: &str| {
                let callers =
                    crate::store::call_graph::find_callers_fast(&reader, name, CALLERS_FETCH_CAP);
                if callers.len() == CALLERS_FETCH_CAP {
                    eprintln!(
                        "warning: `{name}` has at least {CALLERS_FETCH_CAP} direct callers; \
                         multi-hop traversal may have dropped some — results below this node \
                         are incomplete"
                    );
                }
                callers
            };
            let paths =
                crate::callgraph::bfs::find_paths(callers_of, &from, &to, max_hops, max_paths);
            let paths: Vec<_> = paths
                .into_iter()
                .filter(|p| {
                    // Apply scope filter on the *intermediate* steps — if
                    // every intermediate path is excluded, drop the chain.
                    // `from`/`to` themselves have line=0 / no resolved
                    // path, so they're exempt from the include rule.
                    p.steps
                        .iter()
                        .filter(|s| !s.path.is_empty())
                        .all(|s| path_scope.accept(&s.path))
                })
                .collect();
            output::print_paths(&paths, &from, &to, &format);
            Ok(())
        }
        Commands::Reachable {
            target,
            max_hops,
            limit,
            path,
            auto_update,
            no_stale_check,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                false,
                local_cache_active,
                &cfg,
            )?;
            let reader = IndexReader::open(&index_path).context("open index")?;
            if !reader.has_call_graph() {
                bail!(
                    "no call graph in index — `vex reachable` requires a v4 index built without \
                     `--no-call-graph`. Rebuild with `vex index`."
                );
            }
            // When a scope filter is active a 4x over-fetch is not
            // enough — a narrow include could legitimately reject most
            // of the BFS frontier and leave the final list short. The
            // traversal is already bounded by `max_hops`, so we let it
            // run unbounded internally and apply `take(limit)` after
            // the filter.
            let fetch_limit = if path_scope.is_empty() {
                limit
            } else {
                usize::MAX
            };
            const CALLERS_FETCH_CAP: usize = 1024;
            let callers_of = |name: &str| {
                let callers =
                    crate::store::call_graph::find_callers_fast(&reader, name, CALLERS_FETCH_CAP);
                if callers.len() == CALLERS_FETCH_CAP {
                    eprintln!(
                        "warning: `{name}` has at least {CALLERS_FETCH_CAP} direct callers; \
                         reachable set may be incomplete past this node"
                    );
                }
                callers
            };
            let matches =
                crate::callgraph::bfs::find_reachable(callers_of, &target, max_hops, fetch_limit);
            let matches: Vec<_> = matches
                .into_iter()
                .filter(|m| path_scope.accept(&m.path))
                .take(limit)
                .collect();
            output::print_reachable(&matches, &target, &format);
            Ok(())
        }
        Commands::Check {
            names,
            path,
            auto_update,
            no_stale_check,
        } => {
            let root = resolve_root(path)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                false,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;

            // Case-insensitive exact match: FST candidates filtered by actual name
            let results: Vec<(String, bool)> = if let Some(fst) = reader.symbol_fst_reader() {
                names
                    .iter()
                    .map(|n| {
                        let lower = n.to_lowercase();
                        let found = fst.find(n).iter().any(|&idx| {
                            reader
                                .symbol(idx as usize)
                                .map(|r| reader.read_string(r.name_offset).to_lowercase() == lower)
                                .unwrap_or(false)
                        });
                        (n.clone(), found)
                    })
                    .collect()
            } else {
                // Fallback: build lowercased set for consistent case-insensitive matching
                let all_lower: std::collections::HashSet<String> = (0..reader.symbol_count())
                    .filter_map(|i| {
                        let rec = reader.symbol(i)?;
                        let name = reader.read_string(rec.name_offset);
                        if name.is_empty() {
                            None
                        } else {
                            Some(name.to_lowercase())
                        }
                    })
                    .collect();
                names
                    .iter()
                    .map(|n| (n.clone(), all_lower.contains(&n.to_lowercase())))
                    .collect()
            };

            match &format {
                OutputFormat::Json => {
                    let json: serde_json::Value = results
                        .iter()
                        .map(|(name, found)| serde_json::json!({ "name": name, "exists": found }))
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    for (name, found) in &results {
                        let mark = if *found { "+" } else { "-" };
                        println!("{mark} {name}");
                    }
                }
            }
            Ok(())
        }

        Commands::Similar {
            name,
            path,
            limit,
            threshold,
            filter_path,
            explain,
            auto_update,
            no_stale_check,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                true,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;
            if !reader.has_vectors() {
                bail!("No embeddings in index. Run `vex index --semantic` first.");
            }

            let hnsw = config::hnsw_path(&root);
            let fetch_limit = if filter_path.is_some() || !path_scope.is_empty() {
                reader.symbol_count()
            } else {
                limit
            };
            let matches = crate::search::similar::find_similar(
                &reader,
                &hnsw,
                &name,
                fetch_limit,
                threshold,
            )?;
            let matches: Vec<_> = matches
                .into_iter()
                .filter(|m| {
                    let filter_ok = filter_path
                        .as_deref()
                        .map_or(true, |fp| m.path.contains(fp));
                    filter_ok && path_scope.accept(&m.path)
                })
                .take(limit)
                .collect();

            // Build per-result explanations on demand. The seed body is
            // resolved once via `similar::resolve_seed_match`, which uses
            // the same symbol-FST lookup `find_similar` already ran —
            // sharing the entry point keeps both paths in sync.
            let explanations: Option<Vec<_>> = if explain && !matches.is_empty() {
                let seed_body = match crate::search::similar::resolve_seed_match(&reader, &name) {
                    Some(s) => fetch_symbol_body(&s.path, s.line, &s.kind),
                    None => {
                        // Should not happen — `find_similar` already
                        // resolved the seed seconds ago. If it does
                        // (e.g. index mutated between calls), surface
                        // it so the empty `jaccard 0.00 +N -0` rows
                        // aren't misread as a real result.
                        eprintln!(
                            "warning: --explain could not resolve seed symbol `{name}` \
                             after find_similar succeeded; reasoning will be incomplete"
                        );
                        String::new()
                    }
                };
                Some(
                    matches
                        .iter()
                        .map(|m| {
                            let body = fetch_symbol_body(&m.path, m.line, &m.kind);
                            crate::search::explain::explain_pair(
                                &seed_body,
                                &body,
                                EXPLAIN_MAX_DIFF_LINES,
                            )
                        })
                        .collect(),
                )
            } else {
                None
            };

            output::print_similar(&matches, &name, explanations.as_deref(), &format);
            Ok(())
        }
        Commands::Duplicates {
            path,
            threshold,
            limit,
            min_body_lines,
            filter_path,
            explain,
            auto_update,
            no_stale_check,
            scope,
        } => {
            let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
            let root = resolve_root(path)?.canonicalize()?;
            let index_path = ensure_index_ready(
                &root,
                auto_update,
                no_stale_check,
                true,
                local_cache_active,
                &cfg,
            )?;

            let reader = IndexReader::open(&index_path).context("open index")?;
            if !reader.has_vectors() {
                bail!("No embeddings in index. Run `vex index --semantic` first.");
            }

            let hnsw = config::hnsw_path(&root);
            // When --filter or --include/--exclude are active we must
            // over-fetch because the final `take(limit)` runs AFTER the
            // path filter; a narrow filter can drop most pairs. Mirrors
            // `vex similar --filter` (uses `symbol_count()`) but here the
            // upper bound is the pair population, so usize::MAX is the
            // right cap.
            let fetch_limit = if filter_path.is_some() || !path_scope.is_empty() {
                usize::MAX
            } else {
                limit
            };
            let pairs = crate::search::similar::find_duplicates(
                &reader,
                &hnsw,
                threshold,
                min_body_lines,
                fetch_limit,
            )?;
            let pairs: Vec<_> = pairs
                .into_iter()
                .filter(|(a, b)| {
                    let filter_ok = filter_path
                        .as_deref()
                        .map_or(true, |fp| a.path.contains(fp) || b.path.contains(fp));
                    filter_ok && path_scope.accept_pair(&a.path, &b.path)
                })
                .take(limit)
                .collect();

            let explanations: Option<Vec<_>> = if explain && !pairs.is_empty() {
                Some(
                    pairs
                        .iter()
                        .map(|(a, b)| {
                            let a_body = fetch_symbol_body(&a.path, a.line, &a.kind);
                            let b_body = fetch_symbol_body(&b.path, b.line, &b.kind);
                            crate::search::explain::explain_pair(
                                &a_body,
                                &b_body,
                                EXPLAIN_MAX_DIFF_LINES,
                            )
                        })
                        .collect(),
                )
            } else {
                None
            };

            output::print_duplicates(&pairs, explanations.as_deref(), &format);
            Ok(())
        }

        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_owned();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(())
        }

        Commands::Init => {
            let path = std::env::current_dir()
                .context("get working directory")?
                .join(".vex.toml");
            if path.exists() {
                bail!(".vex.toml already exists at {}", path.display());
            }
            std::fs::write(&path, config::DEFAULT_CONFIG)
                .with_context(|| format!("write {}", path.display()))?;
            println!("Created {}", path.display());
            Ok(())
        }

        Commands::SelfUpdate { check, yes } => {
            // Named at the call site so a future refactor cannot silently
            // swap the two boolean positional args.
            cmd_self_update(/*check_only=*/ check, /*no_confirm=*/ yes)
        }
    }
}

/// ed25519 public key used to verify release archives signed in CI via
/// `zipsign`. Anyone modifying this MUST also rotate the corresponding
/// private key stored in the `ZIPSIGN_PRIVATE_KEY` GitHub Secret —
/// otherwise every subsequent release will fail to verify on update.
///
/// Generation: `zipsign gen-key vex.priv vex.pub`. The 32 bytes below
/// are the raw contents of `vex.pub`.
const VEX_RELEASE_PUBKEY: &[u8] = &[
    0x03, 0x9e, 0x75, 0x96, 0xbe, 0x60, 0xaf, 0x61, 0xdf, 0xdf, 0xb7, 0x93, 0x07, 0xc3, 0x2e, 0x95,
    0x38, 0xc9, 0x35, 0xc0, 0xe2, 0x05, 0xcc, 0x9d, 0x0e, 0x31, 0xf9, 0x66, 0x7d, 0xa6, 0x49, 0x51,
];

/// Update the running binary from the latest GitHub release. The
/// self_update crate handles platform detection (target triple), archive
/// download, ed25519 signature verification, atomic file replacement,
/// and Windows-specific in-use-binary swap via a temp rename.
fn cmd_self_update(check_only: bool, no_confirm: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let status = self_update::backends::github::Update::configure()
        .repo_owner("tenatarika")
        .repo_name("vex")
        .bin_name("vex")
        .current_version(current)
        .show_download_progress(true)
        .no_confirm(no_confirm)
        .verifying_keys([*<&[u8; 32]>::try_from(VEX_RELEASE_PUBKEY)
            .expect("VEX_RELEASE_PUBKEY must be 32 bytes")])
        .build()
        .context("configure self-update client")?;

    if check_only {
        let release = status
            .get_latest_release()
            .context("fetch latest release from GitHub (offline or rate-limited?)")?;
        let latest = release.version.as_str();
        if latest == current {
            println!("vex is up to date ({current}).");
            return Ok(());
        }
        // Surface a semver parse failure as an error rather than silently
        // mis-reporting direction — a release tagged with an unexpected
        // prefix would otherwise produce a confusing "no action needed".
        let newer = self_update::version::bump_is_greater(current, latest)
            .with_context(|| format!("could not compare versions {current:?} and {latest:?}"))?;
        if newer {
            println!(
                "Update available: {current} → {latest} ({}).\nRun `vex self-update` (omit --check) to install.",
                release.name
            );
        } else {
            // Local build ahead of GitHub (e.g. a dev branch). Don't
            // pretend an update is needed — just report what's out there.
            println!("Latest release: {latest} (current: {current} — newer, no action needed).");
        }
        return Ok(());
    }

    let result = status.update().context("apply self-update")?;
    match result {
        self_update::Status::UpToDate(v) => println!("vex is already up to date ({v})."),
        self_update::Status::Updated(v) => println!("Updated to vex {v}. Restart any open shells."),
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_callgraph(
    name: &str,
    path: Option<std::path::PathBuf>,
    limit: usize,
    is_callers: bool,
    auto_update: bool,
    no_stale_check: bool,
    local_cache_active: bool,
    cfg: &config::VexConfig,
    format: &OutputFormat,
    excludes: &[String],
    path_scope: &scope::PathScope,
) -> Result<()> {
    let root = resolve_root(path)?;
    let label = if is_callers { "callers" } else { "callees" };
    let start = std::time::Instant::now();
    // Over-fetch when scope filters are active. Both the persistent FST and
    // live-scan paths accept `limit` as a hard cap, so without the inflation
    // a narrow `--include` would silently truncate matches.
    let fetch_limit = if path_scope.is_empty() {
        limit
    } else {
        usize::MAX
    };

    // Fast path: if a v4 index with a call graph exists for this project,
    // use the persistent FST (~4ms). Otherwise fall back to the live-scan
    // implementation that walks files with tree-sitter (~seconds).
    //
    // Bootstrap/staleness behaviour, gated on `auto_update`:
    //   * missing index + auto-update on → bootstrap, then read the v4 FST.
    //   * stale index  + auto-update on → handle_staleness() refreshes it.
    //   * missing index + auto-update off → silently use live-scan (preserves
    //     pre-10.2 UX for projects without an index).
    let canonical_root = root.canonicalize().ok();
    let index_path = canonical_root.as_ref().map(|r| config::index_path(r));
    if let (Some(croot), Some(idx)) = (canonical_root.as_ref(), index_path.as_ref()) {
        let should_auto = auto_update || cfg.auto_update.unwrap_or(false);
        if !idx.exists() {
            if should_auto {
                // Discard the IndexAvail return — we only need the side effect
                // of bootstrap. Reader is opened below the same way as before.
                ensure_index_exists(croot, auto_update, false, local_cache_active, cfg)?;
                // just-bootstrapped → manifest is fresh, skip handle_staleness
            }
            // else: live-scan path; no warning, this command supports it natively
        } else {
            handle_staleness(croot, auto_update, no_stale_check, cfg)?;
        }
    }

    let reader = match index_path.as_ref().filter(|p| p.exists()) {
        Some(p) => match crate::store::reader::IndexReader::open(p) {
            Ok(r) => Some(r),
            Err(e) => {
                // Surface the reason for falling back so a corrupt/locked
                // index doesn't masquerade as "no index found". Direct
                // stderr (not tracing::warn!) because the fallback is a
                // load-bearing UX event — without it the user sees the
                // ~seconds live-scan latency and has no clue why.
                // `{e:#}` includes the anyhow chain (e.g. open + path).
                eprintln!(
                    "Warning: index at {} exists but failed to open ({e:#}). Falling back to live callgraph scan.",
                    p.display()
                );
                None
            }
        },
        None => None,
    };

    let matches = match reader.as_ref() {
        Some(r) if r.has_call_graph() => {
            if is_callers {
                crate::store::call_graph::find_callers_fast(r, name, fetch_limit)
            } else {
                crate::store::call_graph::find_callees_fast(r, name, fetch_limit)
            }
        }
        _ => {
            if is_callers {
                crate::callgraph::find_callers(&root, name, fetch_limit, excludes)?
            } else {
                crate::callgraph::find_callees(&root, name, fetch_limit, excludes)?
            }
        }
    };
    let matches: Vec<_> = matches
        .into_iter()
        .filter(|m| path_scope.accept(&m.path))
        .take(limit)
        .collect();
    let elapsed = start.elapsed();

    match &format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "path": m.path,
                        "line": m.line,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
        OutputFormat::Text => {
            if matches.is_empty() {
                println!("No {label} of \"{name}\" in {elapsed:.2?}");
            } else {
                println!("{name}: {} {label} in {elapsed:.2?}\n", matches.len());
                for m in &matches {
                    println!("  {:<40} {}:{}", m.name, m.path, m.line);
                }
            }
        }
        OutputFormat::Compact => {
            for m in &matches {
                println!("{} {}:{}", m.name, m.path, m.line);
            }
        }
    }
    Ok(())
}

fn cmd_outline(file: &std::path::Path, kind: Option<&str>, format: &OutputFormat) -> Result<()> {
    use crate::index::symbols::SymbolKind;

    let kind_filter = kind.map(|k| k.parse::<SymbolKind>()).transpose()?;

    let content =
        std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;

    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .context("file has no extension")?;

    let lang = crate::parse::language::Language::from_extension(ext)
        .with_context(|| format!("unsupported language: .{ext}"))?;

    if let Err(e) = crate::parse::queries::try_get_query(lang) {
        bail!("failed to load grammar for {} (.{ext}): {e}", lang.as_str());
    }

    let rel = file.to_string_lossy().to_string();
    let parsed = crate::parse::parse_file(&rel, &content, lang)?;

    let symbols: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|s| kind_filter.map_or(true, |k| s.kind == k))
        .collect();

    print_outline(&symbols, file, kind_filter, format);
    Ok(())
}

fn print_outline(
    symbols: &[&crate::index::symbols::ParsedSymbol],
    file: &std::path::Path,
    kind_filter: Option<crate::index::symbols::SymbolKind>,
    format: &OutputFormat,
) {
    match &format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = symbols
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "kind": s.kind.as_str(),
                        "line": s.line,
                        "signature": s.signature,
                    })
                })
                .collect();
            // unwrap: serializing simple JSON values cannot fail
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }
        OutputFormat::Text | OutputFormat::Compact => {
            if symbols.is_empty() {
                if let Some(k) = kind_filter {
                    println!("No {k} symbols found in {}", file.display());
                } else {
                    println!("No symbols found in {}", file.display());
                }
            } else {
                println!("{}", file.display());
                for s in symbols {
                    println!("  {:<12} {:<40} line {}", s.kind.as_str(), s.name, s.line);
                    if let Some(sig) = &s.signature {
                        println!("               {sig}");
                    }
                }
            }
        }
    }
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
