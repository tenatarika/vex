//! Version-control backend abstraction.
//!
//! Covers **diff-scoping only** — `ensure_repo` + `changed_paths`
//! (`docs/VCS-BACKENDS.md`). git is the default and fully verified backend;
//! [`ArcVcs`] (Yandex Arc) is a **provisional, research-grounded** backend
//! reachable via explicit `--vcs arc` (Phase 3, unverified against a real
//! `arc` — see `arc.rs`); svn is still a `NoVcs` floor (Phase 4). Blob-cache,
//! history, and staleness are NOT routed through this trait yet — they stay
//! git-only and hit their existing fallbacks on non-git checkouts.

use std::path::Path;

mod arc;
mod detect;
mod git;
mod none;

pub use arc::ArcVcs;
pub use detect::{install_override, resolve};
pub use git::GitVcs;
pub use none::NoVcs;

/// Which VCS backend the resolver picked for a request. Set by `vcs::detect`
/// from the override chain / marker walk; [`NoVcs`] reports the *detected*
/// kind so an inert backend still names what it saw (`_meta.vex.dev/vcs`
/// reporting lands with the first non-git backend — Phase 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcsKind {
    Git,
    Arc,
    Svn,
    None,
}

impl VcsKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            VcsKind::Git => "git",
            VcsKind::Arc => "arc",
            VcsKind::Svn => "svn",
            VcsKind::None => "none",
        }
    }
}

/// Feature bits a backend advertises so callers degrade deliberately instead
/// of guessing. Grows additively as later phases add operations
/// (`content_addressed`, `rename_follow`, `sha_revisions`).
//
// `merge_base` is not consulted yet — git always supports `SinceBranched`, and
// the only backend that declines it (svn, Phase 4) currently routes through
// `NoVcs` which rejects every scope. The field exists so the svn backend can
// gate `SinceBranched` precisely instead of a blanket decline.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct VcsCapabilities {
    /// `DiffScope::SinceBranched` is supported (git/arc: yes; svn: no — svn
    /// branches are directory copies with no clean merge-base).
    pub merge_base: bool,
}

/// Error from a VCS operation.
///
/// The `Unsupported` vs `Failed` split is load-bearing (H2): a backend that
/// structurally *can't* answer (svn + merge-base) is distinct from one that
/// *could* but errored. Callers MUST NOT collapse either into an empty result
/// set — a silent `Ok(vec![])` turns a broken filter into "your query matched
/// nothing".
#[derive(Debug)]
pub enum VcsError {
    /// The backend cannot perform this operation at all (capability gap) —
    /// e.g. [`NoVcs`] on any scope, or (Phase 4) svn + `SinceBranched`.
    Unsupported(String),
    /// The backend attempted the operation and it failed.
    Failed(anyhow::Error),
}

impl VcsError {
    /// Collapse into an `anyhow::Error` for call sites still on
    /// `anyhow::Result`, preserving the message verbatim so existing
    /// error-substring tests continue to hold.
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            VcsError::Unsupported(m) => anyhow::anyhow!(m),
            VcsError::Failed(e) => e,
        }
    }
}

pub type VcsResult<T> = Result<T, VcsError>;

/// Selects which set of changed paths a search-shaped command restricts to.
///
/// Backend-agnostic: each backend translates it to its own CLI. The `rev`
/// string in `Since` passes through verbatim — the user speaks their backend's
/// revision language (`HEAD~1` for git/arc, `-r 42` for svn).
///
/// The variants are mutually exclusive at the CLI layer (clap
/// `conflicts_with_all`). We carry a borrowed `&str` for `Since` so the
/// caller's existing `String` flows straight through without an extra alloc.
#[derive(Debug, Clone, Copy)]
pub enum DiffScope<'a> {
    /// `--since <rev>` — files changed between `<rev>..HEAD`.
    Since(&'a str),
    /// `--since-branched` — files changed since branch diverged from
    /// main/master (origin first, then local). Requires `merge_base`.
    SinceBranched,
    /// `--changed-only` — working-tree dirty + untracked.
    ChangedOnly,
}

impl DiffScope<'_> {
    /// Human-readable label used by the `_meta["vex.dev/diff_filter"].scope`
    /// JSON field. Stable string — wire-format consumers may key on it.
    pub fn label(&self) -> &'static str {
        match self {
            DiffScope::Since(_) => "since",
            DiffScope::SinceBranched => "since_branched",
            DiffScope::ChangedOnly => "changed_only",
        }
    }
}

/// A version-control backend. One instance per invocation (v1 constructs
/// [`GitVcs`] directly; Phase 2 resolves via detection into a `OnceLock`,
/// mirroring `util::config`'s `CacheResolver`).
pub trait Vcs: Send + Sync {
    /// The backend kind, for diagnostics / `_meta.vex.dev/vcs` reporting.
    // No consumer yet: `_meta.vex.dev/vcs` emission is deferred to the first
    // non-git backend (Phase 3), where "which backend answered" stops being
    // trivially "git". `NoVcs` stores the kind regardless so it's ready.
    #[allow(dead_code)]
    fn kind(&self) -> VcsKind;

    /// Feature bits (e.g. `merge_base`), consulted before backend-specific
    /// scopes once a backend can partially support them (svn, Phase 4).
    #[allow(dead_code)]
    fn capabilities(&self) -> VcsCapabilities;

    /// H3 — repo-validity pre-flight. Load-bearing, not incidental: a bare
    /// `git diff` outside a repo exits 0 with help text and yields a silent
    /// empty change set. Callers MUST pre-flight this before trusting
    /// [`Vcs::changed_paths`]. `Failed` carries a backend-specific reason.
    fn ensure_repo(&self, root: &Path) -> VcsResult<()>;

    /// Raw (un-normalized) changed-path list for `scope`.
    ///
    /// Contract (H2): **never map a backend error to `Ok(vec![])`.** A
    /// non-zero backend exit is `Failed`; a scope the backend can't express
    /// (svn + `SinceBranched`) is `Unsupported`. `Ok(vec![])` means — and only
    /// means — "resolved, nothing changed".
    fn changed_paths(&self, root: &Path, scope: DiffScope) -> VcsResult<Vec<String>>;
}
