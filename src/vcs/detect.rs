//! Backend resolution: pick the [`Vcs`] for a repo root.
//!
//! Precedence (first wins): `--vcs` flag (process-global override) → `VEX_VCS`
//! env → `.vex.toml` `vcs` → marker auto-detection → `none`.
//!
//! **Phase 2 caveats (docs/VCS-BACKENDS.md §6):**
//! - Arc/svn have no backend yet, so a detected/forced Arc or Svn resolves to
//!   [`NoVcs`] (diff-scoping declines cleanly). git is the only functional
//!   backend.
//! - Detection is **markers only** — no `arc root` FUSE probe yet. Probing and
//!   preferring Arc over a nested `.git` would *regress* git-in-arc monorepo
//!   users (they'd lose the diff-scoping the nested `.git` gives them today),
//!   so the Arc-preference lands with `ArcVcs` in Phase 3.

use std::path::Path;
use std::sync::OnceLock;

use super::{ArcVcs, GitVcs, NoVcs, Vcs, VcsKind};

/// Process-global `--vcs` override, installed once from the parsed CLI in
/// dispatch (mirrors `util::config`'s `CACHE_RESOLVER`). Outer `Option` =
/// "was `install_override` called"; inner `Option` = the forced kind
/// (`None` inner ⇒ `--vcs auto` / flag absent ⇒ fall through to env/config).
static VCS_OVERRIDE: OnceLock<Option<VcsKind>> = OnceLock::new();

/// Install the `--vcs` override (call once, early in dispatch). `None` means
/// "no explicit kind" (auto). Idempotent no-op if already set.
///
/// **Do NOT call this from a `#[cfg(test)]` unit test.** `VCS_OVERRIDE` is a
/// process-global `OnceLock` with no safe reset, and nextest runs a crate's
/// unit tests in a *single* binary/process — so one test installing an
/// override would permanently pin it for every subsequent test in that binary.
/// Cover the override/precedence chain with subprocess integration tests
/// (`tests/cli_vcs_test.rs`) instead. Same hazard and mitigation as
/// `util::config::CACHE_RESOLVER`.
pub fn install_override(forced: Option<VcsKind>) {
    let _ = VCS_OVERRIDE.set(forced);
}

/// Resolve the effective backend for `root`.
pub fn resolve(root: &Path) -> Box<dyn Vcs> {
    backend_for(effective_kind(root))
}

fn backend_for(kind: VcsKind) -> Box<dyn Vcs> {
    match kind {
        VcsKind::Git => Box::new(GitVcs),
        // Phase 3: Arc has a (provisional, research-grounded) backend.
        VcsKind::Arc => Box::new(ArcVcs),
        // Svn has no backend yet (Phase 4); `None` is the genuine floor.
        // Both decline via `NoVcs`, which reports the detected kind.
        other => Box::new(NoVcs::new(other)),
    }
}

fn effective_kind(root: &Path) -> VcsKind {
    // 1. `--vcs` flag (forced kind, if any).
    if let Some(Some(forced)) = VCS_OVERRIDE.get() {
        return *forced;
    }
    // 2. `VEX_VCS` env.
    if let Ok(raw) = std::env::var("VEX_VCS") {
        if let Some(kind) = parse_override(&raw, "VEX_VCS") {
            return kind;
        }
    }
    // 3. `.vex.toml` `vcs`.
    if let Ok(cfg) = crate::util::config::load_config(root) {
        if let Some(raw) = cfg.vcs.as_deref() {
            if let Some(kind) = parse_override(raw, ".vex.toml `vcs`") {
                return kind;
            }
        }
    }
    // 4. Marker auto-detection.
    detect_kind(root)
}

/// Parse an override string from the *lenient* tiers (`VEX_VCS`, `.vex.toml`).
/// `"auto"`/empty and unknown values return `None` (fall through to the next
/// tier); unknown values additionally warn so a typo isn't silently ignored.
/// By design this is more forgiving than the `--vcs` flag, which clap's
/// `value_enum` rejects at parse time with a hard error — a flag typo is
/// immediately visible via `--help`, whereas an env/config typo is not, so it
/// degrades to auto-detect with a warning rather than aborting the command.
/// `source` names the origin for the warning.
fn parse_override(raw: &str, source: &str) -> Option<VcsKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "git" => Some(VcsKind::Git),
        "arc" => Some(VcsKind::Arc),
        "svn" => Some(VcsKind::Svn),
        "none" => Some(VcsKind::None),
        "auto" | "" => None,
        other => {
            tracing::warn!(
                value = other,
                source = source,
                "unknown vcs value; expected git|arc|svn|none|auto — ignoring"
            );
            None
        }
    }
}

/// Walk up from `root` for the innermost VCS marker. Markers only (see the
/// module note on why the `arc root` probe is deferred to Phase 3). git wins
/// a co-located tie so git-in-arc monorepos keep their diff-scoping.
fn detect_kind(root: &Path) -> VcsKind {
    for ancestor in root.ancestors() {
        if ancestor.join(".git").exists() {
            return VcsKind::Git;
        }
        if ancestor.join(".svn").exists() {
            return VcsKind::Svn;
        }
        if ancestor.join(".arc").exists() {
            return VcsKind::Arc;
        }
    }
    VcsKind::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_override_maps_known_kinds_and_ignores_auto() {
        assert!(matches!(parse_override("git", "t"), Some(VcsKind::Git)));
        assert!(matches!(parse_override("ARC", "t"), Some(VcsKind::Arc)));
        assert!(matches!(parse_override("svn", "t"), Some(VcsKind::Svn)));
        assert!(matches!(parse_override("none", "t"), Some(VcsKind::None)));
        assert!(parse_override("auto", "t").is_none());
        assert!(parse_override("", "t").is_none());
        assert!(parse_override("bogus", "t").is_none());
    }

    #[test]
    fn detect_kind_finds_git_marker_and_defaults_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(detect_kind(tmp.path()), VcsKind::None);
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        assert_eq!(detect_kind(tmp.path()), VcsKind::Git);
    }

    #[test]
    fn detect_kind_git_wins_colocated_arc_marker() {
        // git-in-arc monorepo: a co-located `.git` must win so diff-scoping
        // isn't lost (Phase 2 has no Arc backend). Marker walk hits `.git`
        // first in the same dir.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::create_dir(tmp.path().join(".arc")).unwrap();
        assert_eq!(detect_kind(tmp.path()), VcsKind::Git);
    }
}
