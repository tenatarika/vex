use std::path::Path;

use anyhow::Result;

/// Watch a project directory for changes and trigger incremental re-indexing.
///
/// Phase 3: uses notify crate with debouncing.
pub fn watch(_root: &Path) -> Result<()> {
    // TODO: implement with notify + notify-debouncer-full
    // 1. Create debounced watcher (500ms window)
    // 2. On file change: determine language, re-parse, update store
    // 3. On file delete: remove from index
    // 4. Run in blocking loop until SIGINT
    tracing::info!("watch mode not yet implemented");
    Ok(())
}
