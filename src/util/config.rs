use std::path::{Path, PathBuf};

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
            let config: VexConfig = toml::from_str(&content)
                .with_context(|| format!("parse {}", candidate.display()))?;
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
"#;

/// Get the cache directory for vex indexes.
///
/// macOS: ~/Library/Caches/vex/<hash>/
/// Linux: $XDG_CACHE_HOME/vex/<hash>/
pub fn index_dir(project_root: &std::path::Path) -> PathBuf {
    let hash = xxh3_64(project_root.to_string_lossy().as_bytes());
    let cache = dirs_cache_dir().join("vex").join(format!("{hash:016x}"));
    cache
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

fn dirs_cache_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        dirs_home().join("Library").join("Caches")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs_home().join(".cache"))
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}
