//! Tracked-file discovery for the Phase 14.7 blob-SHA parse cache.
//!
//! Thin caller of the `Vcs` trait: the git-specific `ls-files -s` + dirty-tree
//! (`diff-files`) logic now lives in `GitVcs::tracked_content_ids`
//! (`src/vcs/git.rs`), routed via the resolved backend (VCS-BACKENDS Phase 5).
//! This wrapper maps a declined/failed result to an empty map — the pipeline
//! treats that as "no blob cache this run" and routes everything through the
//! existing xxh3/mtime path (best-effort, correctness unaffected, no error).
//!
//! Consequence of routing through the backend: `--vcs none` (or forcing a
//! non-content-addressed backend like `arc`/`svn`) disables the git blob cache
//! for that run, which is the intended trait semantic — the user asked for a
//! backend that does not offer content-addressed ids.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Content ids (blob SHAs) for tracked regular files, keyed by canonical
/// absolute path. Empty when the resolved backend has no content-addressed
/// store (svn/none, or arc pending field-verification) or git is unavailable —
/// a cache miss, not an error (see the inverted-H2 note on
/// [`crate::vcs::Vcs::tracked_content_ids`]).
pub fn discover_tracked_blobs(repo_root: &Path) -> HashMap<PathBuf, String> {
    crate::vcs::resolve(repo_root)
        .tracked_content_ids(repo_root)
        .unwrap_or_default()
}
