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
            // In practice only `Svn` reaches this arm since Phase 3:
            // `detect::backend_for` routes `VcsKind::Arc` to `ArcVcs`, not
            // `NoVcs`. `Arc` is kept here defensively — if a future path
            // reintroduces `NoVcs::new(VcsKind::Arc)`, this wording still holds.
            VcsKind::Arc | VcsKind::Svn => format!(
                "the {} backend is not yet available (planned — see docs/VCS-BACKENDS.md); \
                 diff-scoping requires git. Re-run without --since/--since-branched/--changed-only, \
                 or use --vcs git if a nested git checkout applies.",
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
