//! The `none` / no-backend floor.
//!
//! Used when no VCS is detected, when the user forces `--vcs none`, or when a
//! backend is detected but not yet implemented (Arc/svn in Phase 2, before
//! their backends land). Every operation declines cleanly — diff-scoping is
//! simply unavailable — rather than shelling out or guessing.

use std::path::Path;

use super::{DiffScope, Vcs, VcsCapabilities, VcsError, VcsKind, VcsResult};

/// A backend that performs no VCS operations. Carries the `detected` kind so
/// it can report *why* it's inert (genuinely no VCS vs. a backend that isn't
/// implemented yet) in its errors and, later, in `_meta`.
#[derive(Debug, Clone, Copy)]
pub struct NoVcs {
    detected: VcsKind,
}

impl NoVcs {
    pub fn new(detected: VcsKind) -> Self {
        Self { detected }
    }

    /// Reason string tailored to why this backend is inert. A detected but
    /// unimplemented backend (Arc/svn) says so; a genuinely VCS-less directory
    /// reuses the historical git-only wording verbatim (git is the sole
    /// functional backend), so the message and existing tests are unchanged
    /// from Phase 1 for the common no-repo case.
    fn reason(&self, root: &Path) -> String {
        match self.detected {
            // Fully defensive since Phase 4: `detect::backend_for` routes both
            // `Arc`→`ArcVcs` and `Svn`→`SvnVcs`, so neither reaches `NoVcs` in
            // practice. Kept so a future path that reintroduces
            // `NoVcs::new(Arc|Svn)` still produces a coherent message.
            VcsKind::Arc | VcsKind::Svn => format!(
                "the {} backend is unavailable here; diff-scoping requires git. \
                 Re-run without --since/--since-branched/--changed-only, or use \
                 --vcs git if a nested git checkout applies.",
                self.detected.as_str()
            ),
            _ => format!(
                "not a git repository at {}: --since/--since-branched/--changed-only require a git checkout",
                root.display()
            ),
        }
    }
}

impl Vcs for NoVcs {
    fn kind(&self) -> VcsKind {
        self.detected
    }

    fn capabilities(&self) -> VcsCapabilities {
        VcsCapabilities { merge_base: false }
    }

    fn ensure_repo(&self, root: &Path) -> VcsResult<()> {
        Err(VcsError::Unsupported(self.reason(root)))
    }

    fn changed_paths(&self, root: &Path, _scope: DiffScope) -> VcsResult<Vec<String>> {
        Err(VcsError::Unsupported(self.reason(root)))
    }
}
