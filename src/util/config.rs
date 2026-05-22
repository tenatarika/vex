use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;
use xxhash_rust::xxh3::xxh3_64;

/// On-disk representation of `.vex.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VexConfig {
    /// Glob patterns to exclude (gitignore syntax, applied on top of .gitignore)
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Default output format: "text", "json", or "compact"
    pub format: Option<String>,

    /// Enable semantic embeddings by default
    pub semantic: Option<bool>,

    /// Automatically update the index before search if stale
    pub auto_update: Option<bool>,

    /// Embedder identifier for semantic indexing. Defaults to `"minilm-l6-v2"`
    /// when omitted. Use `vex --help` or the docs to list known IDs.
    pub embedder: Option<String>,

    /// Override the cache root used to store the index for this project.
    ///
    /// Accepts:
    ///   * absolute paths (`/var/cache/vex`, `C:\vex-cache`),
    ///   * `~`-prefixed paths (`~/.vex-cache`),
    ///   * relative paths — resolved against the directory that holds the
    ///     `.vex.toml` (typical use: `"./.vex/cache"`).
    pub cache_dir: Option<String>,

    /// Store the index inside the project as `<project>/.vex_cache/`,
    /// skipping the project-hash subdirectory entirely. Equivalent to
    /// `cache_dir = "./.vex_cache"` but without the hash layer — useful
    /// when the cache should travel with the project (renames, moves).
    /// Overridden by `cache_dir`, `--cache-dir`, or `$VEX_CACHE_DIR`.
    pub local_cache: Option<bool>,

    /// Default thread count for parallel indexing operations.
    ///   * `Some(0)` — explicit "use all cores"
    ///   * `Some(N)` (N > 0) — use exactly N workers
    ///   * `None` — 80% of available cores, rounded up (default)
    pub jobs: Option<usize>,

    /// Build the persistent call-graph section during `vex index`. When
    /// `false`, `vex callers`/`vex callees` fall back to live-scan
    /// (~seconds on medium repos) but indexing is materially faster on
    /// monorepos. Default `true`. Resolution order for individual builds:
    /// CLI flag > this field > previous manifest > default.
    pub call_graph: Option<bool>,

    /// Build the BM25 channel during `vex index`. When `false`, hybrid
    /// search falls back to structural-only (+ semantic if enabled). Same
    /// resolution order as `call_graph`. Default `true`.
    pub bm25: Option<bool>,

    /// Build the v6 pattern-skeleton side-section during `vex index`
    /// (11.4). When `false`, `vex pattern` keeps using its live-scan
    /// path. Same resolution order as `call_graph` / `bm25`. Default
    /// `true`.
    pub pattern_index: Option<bool>,

    /// Directory containing the `.vex.toml` that produced this config.
    /// Used to resolve relative `cache_dir` paths.
    #[serde(skip)]
    pub source_dir: Option<PathBuf>,
}

/// Search for `.vex.toml` starting from `start_dir`, walking up to filesystem root.
/// Returns the parsed config, or a default if no file is found.
pub fn load_config(start_dir: &Path) -> Result<VexConfig> {
    let mut dir = start_dir.to_path_buf();
    loop {
        let candidate = dir.join(".vex.toml");
        if candidate.is_file() {
            let content = std::fs::read_to_string(&candidate)
                .with_context(|| format!("read {}", candidate.display()))?;
            let mut config: VexConfig = toml::from_str(&content)
                .with_context(|| format!("parse {}", candidate.display()))?;
            config.source_dir = Some(dir.clone());
            tracing::debug!(path = %candidate.display(), "loaded config");
            return Ok(config);
        }
        if !dir.pop() {
            break;
        }
    }
    Ok(VexConfig::default())
}

/// Default .vex.toml content with comments explaining each option.
pub const DEFAULT_CONFIG: &str = r#"# vex configuration — https://github.com/tenatarika/vex
#
# Place this file in your project root as .vex.toml

# Glob patterns to exclude from indexing (gitignore syntax, on top of .gitignore)
# exclude = [
#     "vendor/**",
#     "node_modules/**",
#     "*.generated.go",
#     "dist/**",
# ]

# Default output format: "text", "json", or "compact"
# format = "text"

# Enable semantic embeddings by default (slower indexing, enables meaning-based search)
# semantic = false

# Automatically run `vex update` before search if the index is stale
# auto_update = false

# Embedder used for semantic indexing. Known IDs: minilm-l6-v2 (default).
# Changing the embedder requires a full reindex.
# embedder = "minilm-l6-v2"

# Cache directory override. Defaults to the platform cache location.
#   macOS:   ~/Library/Caches/vex
#   Linux:   $XDG_CACHE_HOME/vex   (fallback: ~/.cache/vex)
#   Windows: %LOCALAPPDATA%\vex    (fallback: %USERPROFILE%\AppData\Local\vex)
# Accepts absolute paths, "~/..." or paths relative to this file (e.g. "./.vex/cache").
# Can also be overridden per-invocation with --cache-dir or $VEX_CACHE_DIR.
# cache_dir = "./.vex/cache"

# Store the index inside the project as `<project>/.vex_cache/`. Useful when
# the cache should travel with the project (e.g. on a moved or renamed
# directory). vex writes a `.gitignore` inside it so contents are not
# committed. Overridden by `cache_dir`, `--cache-dir`, or $VEX_CACHE_DIR.
# local_cache = false

# Thread count for parallel indexing (index/update/watch).
#   * unset  — 80% of available cores, rounded up (default, leaves headroom)
#   * 0      — use all cores (explicit opt-in to max throughput)
#   * N      — exactly N workers
# Overridable per-invocation with `-j/--jobs` or $VEX_JOBS.
# jobs = 4

# Build the persistent call-graph section. Disabling falls back to live-scan
# for `vex callers`/`vex callees` (slower per-query, but saves indexing
# time on large monorepos). The opt-out is persisted in the manifest so
# `vex update` does not silently re-add the section.
# Per-invocation override: `vex index --no-call-graph`.
# call_graph = true

# Build the BM25 channel. Disabling drops the third RRF channel and keeps
# only structural (+ semantic). Same persistence rules as `call_graph`.
# Per-invocation override: `vex index --no-bm25`.
# bm25 = true
"#;

/// Process-global override for the cache root. Set once at CLI startup
/// (`set_cache_override`), read by every `index_dir` call. We rely on a
/// global because every cli sub-command threads `index_path()` through
/// many call sites — propagating an extra param everywhere would be churn
/// with no behavioural benefit.
static CACHE_OVERRIDE: OnceLock<CacheLayout> = OnceLock::new();

#[derive(Clone, Debug)]
struct CacheLayout {
    root: PathBuf,
    /// When true, `index_dir(project_root)` returns `root` as-is. When
    /// false (the usual platform-cache case), a hash subdirectory is
    /// appended so multiple projects share the same root safely.
    skip_hash_subdir: bool,
}

/// Install a process-wide override for the cache root. No-op if called twice.
///
/// `skip_hash_subdir = true` is used by the local-cache mode where the
/// cache directory is unique to one project and the hash adds no value.
pub fn set_cache_override(path: PathBuf, skip_hash_subdir: bool) {
    let _ = CACHE_OVERRIDE.set(CacheLayout {
        root: path,
        skip_hash_subdir,
    });
}

/// Return the worker count that was *explicitly* requested by the user
/// — CLI flag, env var, or config field, in that priority. `None` means
/// "the user did not pick a value; caller decides whether to apply a
/// default". Callers that always want a concrete number can use the
/// higher-level [`resolve_jobs`] wrapper.
pub fn resolve_explicit_jobs(cli_jobs: Option<usize>, cfg: &VexConfig) -> Option<usize> {
    if let Some(n) = cli_jobs {
        return Some(n);
    }
    if let Ok(env) = std::env::var("VEX_JOBS") {
        let trimmed = env.trim();
        if !trimmed.is_empty() {
            match trimmed.parse::<usize>() {
                Ok(n) => return Some(n),
                Err(_) => {
                    eprintln!(
                        "warning: VEX_JOBS={env:?} is not a valid integer — falling back to config or default."
                    );
                }
            }
        }
    }
    cfg.jobs
}

/// Resolve the worker thread count for parallel indexing, honouring
/// CLI > env > config > default.
///
/// When no explicit value is configured anywhere, falls back to
/// `default_thread_count()` (80% of available cores, min 1) so that the
/// machine stays responsive during long index builds. `0` from any
/// explicit source is treated as "leave rayon at its global default (all
/// cores)" — that is the way to opt back into the old behaviour.
pub fn resolve_jobs(cli_jobs: Option<usize>, cfg: &VexConfig) -> usize {
    resolve_explicit_jobs(cli_jobs, cfg).unwrap_or_else(default_thread_count)
}

/// Default worker count when no explicit setting was provided: 80% of the
/// available cores, rounded up, with a floor of 1. Designed to leave
/// enough headroom for an editor / browser / language server on the same
/// machine while still being parallel enough for large repos.
///
/// Note: on machines with 1–4 cores, `ceil(N * 0.8)` produces `N` itself
/// — i.e. no headroom is applied. The floor matters more than the
/// ceiling for small hosts; capping further would leave fewer than 3
/// workers, which makes index builds noticeably slower for little gain.
pub fn default_thread_count() -> usize {
    let total = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // ceil(total * 0.8) without floating-point.
    (total * 4).div_ceil(5).max(1)
}

/// Configure the global rayon thread pool. Safe to call from multiple
/// commands — only the first call wins (rayon enforces this). A value of
/// `0` leaves rayon at its default (one worker per logical CPU).
pub fn init_rayon_pool(jobs: usize) {
    if jobs == 0 {
        return;
    }
    if let Err(e) = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build_global()
    {
        // Most likely cause: a previous command already initialized the
        // pool. Surface as debug since this is benign in long-running
        // processes (watch mode).
        tracing::debug!(jobs, error = %e, "rayon pool already configured");
    }
}

/// Outcome of `resolve_cache_root` — carries the layout decision so
/// `set_cache_override` can skip the project-hash subdirectory for
/// project-local caches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCache {
    pub root: PathBuf,
    pub skip_hash_subdir: bool,
}

/// Resolve the cache root for a given config, honouring CLI > env > config > default.
///
/// `cli_override` is the value of `--cache-dir`; pass `None` if not provided.
/// `cfg` may have `cache_dir` set in `.vex.toml`; `source_dir` is used as
/// the anchor for relative paths.
pub fn resolve_cache_root(cli_override: Option<&Path>, cfg: &VexConfig) -> ResolvedCache {
    if let Some(p) = cli_override {
        return ResolvedCache {
            root: expand_user(p),
            skip_hash_subdir: false,
        };
    }
    if let Ok(env) = std::env::var("VEX_CACHE_DIR") {
        if !env.is_empty() {
            return ResolvedCache {
                root: expand_user(Path::new(&env)),
                skip_hash_subdir: false,
            };
        }
    }
    if let Some(s) = cfg.cache_dir.as_deref() {
        let raw = Path::new(s);
        let expanded = expand_user(raw);
        // Reject `..` traversal regardless of whether the resulting path is
        // absolute or relative. `cache_dir = "~/../etc/evil"` becomes
        // absolute after tilde-expansion, so the check has to look at the
        // *expanded* path, not just the relative branch. A committed
        // `.vex.toml` is an untrusted input from the perspective of a
        // contributor running `vex` — we never let it redirect the index
        // above its intended location.
        if expanded
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            // Show both the raw config value AND what tilde-expansion produced
            // so a user who wrote `cache_dir = "~/../etc"` can see that the
            // expansion (not the literal `~/..`) is what triggered the block.
            eprintln!(
                "warning: cache_dir {s:?} (expanded to {expanded}) contains `..` traversal — ignoring and using platform default.",
                expanded = expanded.display()
            );
            return ResolvedCache {
                root: default_cache_root(),
                skip_hash_subdir: false,
            };
        }
        let root = if expanded.is_absolute() {
            expanded
        } else if let Some(anchor) = cfg.source_dir.as_deref() {
            anchor.join(expanded)
        } else {
            expanded
        };
        return ResolvedCache {
            root,
            skip_hash_subdir: false,
        };
    }
    if cfg.local_cache.unwrap_or(false) {
        if let Some(anchor) = cfg.source_dir.as_deref() {
            return ResolvedCache {
                root: anchor.join(".vex_cache"),
                skip_hash_subdir: true,
            };
        }
        // No anchor → no project root known. Falling back silently here
        // is the same class of bug that caused the original Windows
        // `/tmp` issue, so make the misconfiguration visible.
        eprintln!(
            "warning: local_cache = true but no .vex.toml was located — falling back to platform default."
        );
    }
    ResolvedCache {
        root: default_cache_root(),
        skip_hash_subdir: false,
    }
}

/// Expand a leading `~` to the user's home directory. Returns the input
/// unchanged when home cannot be determined or the path does not start
/// with `~`.
fn expand_user(p: &Path) -> PathBuf {
    let s = match p.to_str() {
        Some(s) => s,
        None => return p.to_path_buf(),
    };
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    } else if s == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}

/// Get the cache directory for vex indexes.
///
/// Honours `set_cache_override()` if installed; otherwise uses the platform
/// default. A project-root hash is appended as a subdirectory unless the
/// installed override declared the layout as project-local.
pub fn index_dir(project_root: &std::path::Path) -> PathBuf {
    let layout = match CACHE_OVERRIDE.get() {
        Some(l) => l.clone(),
        None => CacheLayout {
            root: default_cache_root(),
            skip_hash_subdir: false,
        },
    };
    if layout.skip_hash_subdir {
        return layout.root;
    }
    let hash = xxh3_64(project_root.to_string_lossy().as_bytes());
    layout.root.join(format!("{hash:016x}"))
}

/// Write a `.gitignore` with `*` into `dir` so the project-local cache
/// is not accidentally committed. Idempotent and race-safe: uses
/// `create_new` so a concurrent process (or a planted symlink) cannot
/// trick us into overwriting an existing file. Failures other than
/// "already exists" are logged at debug — a missing .gitignore is
/// annoying, not fatal.
pub fn write_local_cache_gitignore(dir: &Path) {
    use std::io::Write;
    let path = dir.join(".gitignore");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(b"# auto-generated by vex\n*\n") {
                tracing::debug!(path = %path.display(), error = %e, "failed to write .gitignore");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "failed to create .gitignore");
        }
    }
}

/// Full path to the index file for a project.
pub fn index_path(project_root: &std::path::Path) -> PathBuf {
    index_dir(project_root).join("index.vex")
}

/// Full path to the HNSW index file (for fast semantic search).
pub fn hnsw_path(project_root: &std::path::Path) -> PathBuf {
    index_dir(project_root).join("index.hnsw")
}

/// Full path to the manifest file (tracks file hashes for incremental updates).
pub fn manifest_path(project_root: &std::path::Path) -> PathBuf {
    index_dir(project_root).join("manifest.json")
}

/// Cache directory for embedding model files (e.g. MiniLM ONNX weights).
/// The model is identical across projects and is ~86 MB, so we store it
/// at `<cache-root>/embeddings/` instead of per-project. Crucially we
/// never append the project-hash subdir (every project would otherwise
/// re-download the same model into its own bucket).
///
/// Note on `local_cache = true`: the override root is itself the
/// project's `.vex_cache/`, so the model lands at
/// `<project>/.vex_cache/embeddings/`. That keeps the model with the
/// project — the user explicitly opted in to a portable layout. The
/// design only deduplicates across projects sharing the *platform*
/// cache root; deliberately portable setups still pay the per-project
/// cost.
pub fn embed_cache_dir() -> PathBuf {
    let root = match CACHE_OVERRIDE.get() {
        Some(layout) => layout.root.clone(),
        None => default_cache_root(),
    };
    root.join("embeddings")
}

/// Platform-default cache root with the `vex/` segment appended.
fn default_cache_root() -> PathBuf {
    platform_cache_base().join("vex")
}

/// Platform cache base, without the `vex` suffix.
fn platform_cache_base() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            return home.join("Library").join("Caches");
        }
        PathBuf::from(".cache")
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(v) = std::env::var("LOCALAPPDATA") {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
        if let Ok(v) = std::env::var("USERPROFILE") {
            if !v.is_empty() {
                return PathBuf::from(v).join("AppData").join("Local");
            }
        }
        // Some shells (msys2, cygwin, git-bash) set HOME on Windows —
        // honour it as a last resort before falling back to a project-local
        // cache so MCP spawners without LOCALAPPDATA still work.
        if let Ok(v) = std::env::var("HOME") {
            if !v.is_empty() {
                return PathBuf::from(v).join("AppData").join("Local");
            }
        }
        eprintln!("warning: no LOCALAPPDATA/USERPROFILE/HOME set; using ./.vex/cache as fallback");
        PathBuf::from(".vex").join("cache")
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        if let Ok(v) = std::env::var("XDG_CACHE_HOME") {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
        if let Some(home) = home_dir() {
            return home.join(".cache");
        }
        eprintln!("warning: no HOME/XDG_CACHE_HOME set; using ./.vex/cache as fallback");
        PathBuf::from(".vex").join("cache")
    }
}

/// User home directory across platforms. `None` if it cannot be determined.
fn home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if let Ok(v) = std::env::var("USERPROFILE") {
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
        if let Ok(v) = std::env::var("HOME") {
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOME")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // env mutations are process-global; serialize the tests that touch them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that restores env vars on drop — covers both the
    /// normal-return and the panic-unwind paths so a failing assertion
    /// in one test cannot leak mutated env into the next.
    struct EnvRestore(Vec<(String, Option<String>)>);

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn with_env_vars<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        let _restore = EnvRestore(saved);
        for (k, v) in vars {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        f();
    }

    #[test]
    fn cli_override_beats_everything() {
        with_env_vars(&[("VEX_CACHE_DIR", Some("/from/env"))], || {
            let cfg = VexConfig {
                cache_dir: Some("/from/config".into()),
                local_cache: Some(true),
                source_dir: Some(PathBuf::from("/projects/foo")),
                ..Default::default()
            };
            let resolved = resolve_cache_root(Some(Path::new("/from/cli")), &cfg);
            assert_eq!(resolved.root, PathBuf::from("/from/cli"));
            assert!(!resolved.skip_hash_subdir);
        });
    }

    #[test]
    fn env_beats_config() {
        with_env_vars(&[("VEX_CACHE_DIR", Some("/from/env"))], || {
            let cfg = VexConfig {
                cache_dir: Some("/from/config".into()),
                ..Default::default()
            };
            let resolved = resolve_cache_root(None, &cfg);
            assert_eq!(resolved.root, PathBuf::from("/from/env"));
        });
    }

    #[test]
    fn empty_env_falls_through_to_config() {
        with_env_vars(&[("VEX_CACHE_DIR", Some(""))], || {
            let cfg = VexConfig {
                cache_dir: Some("/from/config".into()),
                ..Default::default()
            };
            let resolved = resolve_cache_root(None, &cfg);
            assert_eq!(resolved.root, PathBuf::from("/from/config"));
        });
    }

    #[test]
    fn relative_config_path_resolves_against_source_dir() {
        with_env_vars(&[("VEX_CACHE_DIR", None)], || {
            let cfg = VexConfig {
                cache_dir: Some("./.vex/cache".into()),
                source_dir: Some(PathBuf::from("/projects/foo")),
                ..Default::default()
            };
            let resolved = resolve_cache_root(None, &cfg);
            assert_eq!(resolved.root, PathBuf::from("/projects/foo/./.vex/cache"));
            assert!(!resolved.skip_hash_subdir);
        });
    }

    #[test]
    fn tilde_expands_to_home() {
        with_env_vars(
            &[("VEX_CACHE_DIR", None), ("HOME", Some("/home/alice"))],
            || {
                let cfg = VexConfig {
                    cache_dir: Some("~/.vex-cache".into()),
                    ..Default::default()
                };
                let resolved = resolve_cache_root(None, &cfg);
                // On Windows `home_dir` reads USERPROFILE first; skip the
                // assertion there since HOME is the unix path.
                #[cfg(not(target_os = "windows"))]
                assert_eq!(resolved.root, PathBuf::from("/home/alice/.vex-cache"));
                #[cfg(target_os = "windows")]
                let _ = resolved;
            },
        );
    }

    #[test]
    fn local_cache_uses_project_root_no_hash() {
        with_env_vars(&[("VEX_CACHE_DIR", None)], || {
            let cfg = VexConfig {
                local_cache: Some(true),
                source_dir: Some(PathBuf::from("/projects/foo")),
                ..Default::default()
            };
            let resolved = resolve_cache_root(None, &cfg);
            assert_eq!(resolved.root, PathBuf::from("/projects/foo/.vex_cache"));
            assert!(resolved.skip_hash_subdir);
        });
    }

    #[test]
    fn rejects_path_traversal_in_relative_cache_dir() {
        with_env_vars(&[("VEX_CACHE_DIR", None)], || {
            let cfg = VexConfig {
                cache_dir: Some("../../../tmp/evil".into()),
                source_dir: Some(PathBuf::from("/projects/foo")),
                ..Default::default()
            };
            let resolved = resolve_cache_root(None, &cfg);
            // Falls back to platform default; never escapes the anchor.
            assert_ne!(resolved.root, PathBuf::from("/tmp/evil"));
            assert!(
                !resolved
                    .root
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "resolved path still contained `..`: {:?}",
                resolved.root
            );
        });
    }

    #[test]
    fn absolute_cache_dir_with_parent_dir_components_is_allowed() {
        // Path traversal is only a concern for *relative* paths anchored
        // at the project root. Absolute paths are explicit user intent.
        with_env_vars(&[("VEX_CACHE_DIR", None)], || {
            let cfg = VexConfig {
                cache_dir: Some("/var/cache/vex".into()),
                source_dir: Some(PathBuf::from("/projects/foo")),
                ..Default::default()
            };
            let resolved = resolve_cache_root(None, &cfg);
            assert_eq!(resolved.root, PathBuf::from("/var/cache/vex"));
        });
    }

    #[test]
    fn explicit_cache_dir_beats_local_cache() {
        with_env_vars(&[("VEX_CACHE_DIR", None)], || {
            let cfg = VexConfig {
                local_cache: Some(true),
                cache_dir: Some("/explicit".into()),
                source_dir: Some(PathBuf::from("/projects/foo")),
                ..Default::default()
            };
            let resolved = resolve_cache_root(None, &cfg);
            assert_eq!(resolved.root, PathBuf::from("/explicit"));
            assert!(!resolved.skip_hash_subdir);
        });
    }

    #[test]
    fn resolve_jobs_priority() {
        with_env_vars(&[("VEX_JOBS", None)], || {
            let mut cfg = VexConfig {
                jobs: Some(2),
                ..Default::default()
            };
            assert_eq!(resolve_jobs(Some(8), &cfg), 8);
            assert_eq!(resolve_jobs(None, &cfg), 2);
            // Explicit 0 from any source means "all cores" — preserved
            // verbatim by resolve_jobs.
            cfg.jobs = Some(0);
            assert_eq!(resolve_jobs(None, &cfg), 0);
            // Absence of every setting falls back to the 80% default,
            // which is always >= 1 on any host we can run on.
            cfg.jobs = None;
            let fallback = resolve_jobs(None, &cfg);
            assert!(fallback >= 1, "default jobs must be at least 1");
            assert_eq!(fallback, default_thread_count());
        });
        with_env_vars(&[("VEX_JOBS", Some("6"))], || {
            let cfg = VexConfig {
                jobs: Some(2),
                ..Default::default()
            };
            assert_eq!(resolve_jobs(None, &cfg), 6);
            assert_eq!(resolve_jobs(Some(8), &cfg), 8);
        });
    }

    #[test]
    fn default_thread_count_is_at_least_one() {
        // The host's available_parallelism is non-zero on any platform
        // we support, so the 80% fallback must always yield >= 1.
        let n = default_thread_count();
        assert!(n >= 1, "default_thread_count() returned 0");
    }

    #[test]
    fn jobs_zero_in_config_means_all_cores() {
        with_env_vars(&[("VEX_JOBS", None)], || {
            let cfg = VexConfig {
                jobs: Some(0),
                ..Default::default()
            };
            assert_eq!(resolve_jobs(None, &cfg), 0);
        });
    }

    #[test]
    fn vex_jobs_zero_env_means_all_cores() {
        // Symmetric to the config field — an explicit 0 in any source
        // is the opt-in to "use every available core".
        with_env_vars(&[("VEX_JOBS", Some("0"))], || {
            let cfg = VexConfig::default();
            assert_eq!(resolve_jobs(None, &cfg), 0);
            // Even with a non-zero config fallback, env wins.
            let cfg = VexConfig {
                jobs: Some(4),
                ..Default::default()
            };
            assert_eq!(resolve_jobs(None, &cfg), 0);
        });
    }

    #[test]
    fn tilde_with_parent_dir_is_rejected() {
        // Regression: `expand_user` turns `~/../etc/evil` into an
        // absolute path, which previously bypassed the traversal check.
        // The fixed code scans the *expanded* path for ParentDir.
        with_env_vars(
            &[("VEX_CACHE_DIR", None), ("HOME", Some("/home/alice"))],
            || {
                let cfg = VexConfig {
                    cache_dir: Some("~/../etc/evil".into()),
                    ..Default::default()
                };
                let resolved = resolve_cache_root(None, &cfg);
                #[cfg(not(target_os = "windows"))]
                {
                    assert_ne!(
                        resolved.root,
                        PathBuf::from("/home/alice/../etc/evil"),
                        "tilde-bypassed traversal slipped through"
                    );
                    assert!(
                        !resolved
                            .root
                            .components()
                            .any(|c| matches!(c, std::path::Component::ParentDir)),
                        "resolved path still contained `..`: {:?}",
                        resolved.root
                    );
                }
                #[cfg(target_os = "windows")]
                let _ = resolved;
            },
        );
    }

    #[test]
    fn explicit_jobs_returns_none_when_unset() {
        with_env_vars(&[("VEX_JOBS", None)], || {
            let cfg = VexConfig::default();
            assert_eq!(resolve_explicit_jobs(None, &cfg), None);
        });
    }

    #[test]
    fn explicit_jobs_picks_up_env() {
        with_env_vars(&[("VEX_JOBS", Some("3"))], || {
            let cfg = VexConfig::default();
            assert_eq!(resolve_explicit_jobs(None, &cfg), Some(3));
        });
    }

    #[test]
    fn config_anchors_at_source_dir() {
        let tmp = std::env::temp_dir().join(format!("vex-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".vex.toml"), "cache_dir = \"./.vex/cache\"\n").unwrap();
        let cfg = load_config(&tmp).unwrap();
        assert_eq!(cfg.source_dir.as_deref(), Some(tmp.as_path()));
        assert_eq!(cfg.cache_dir.as_deref(), Some("./.vex/cache"));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
