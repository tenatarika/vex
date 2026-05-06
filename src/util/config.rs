use std::path::PathBuf;

use xxhash_rust::xxh3::xxh3_64;

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
