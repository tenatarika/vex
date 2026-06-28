//! Workspace (multi-repository) support.
//!
//! Phase 1 of the multi-repo design (`docs/MULTIREPO.md`): parse a
//! `.vex-workspace.toml` manifest and resolve it into a set of member
//! repos, each of which keeps its own independent per-repo index dir
//! (identical to single-repo mode — the cache is already keyed by a hash
//! of the canonical root). No query fanout / indexing wiring yet.
//!
//! ## Invariants
//!
//! - **Canonicalize once.** Member roots are canonicalized exactly here,
//!   at load. Downstream code consumes [`Member::root`] and must never
//!   re-derive a root from a relative manifest path or a member's own
//!   `.vex.toml` — that is the macOS `/tmp`-symlink cache-fallback hazard.
//! - **Disjoint members.** Two members may not resolve to the same path,
//!   and one member may not be nested inside another.
//! - **Platform-cache only (MVP).** A member whose `.vex.toml` sets
//!   `cache_dir` / `local_cache` is rejected: honouring it needs the
//!   `CacheResolver` workspace-root/member-root split that is not built
//!   yet (`docs/MULTIREPO.md` §6.1).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::util::config::{self, VexConfig};

/// The workspace manifest filename, looked up at / above a directory.
pub const WORKSPACE_FILE: &str = ".vex-workspace.toml";

#[derive(Debug, Deserialize)]
struct WorkspaceManifest {
    /// `[[repo]]` table array. Renamed so the TOML key is the singular
    /// `repo`, matching the one-entry-per-table convention.
    #[serde(default, rename = "repo")]
    repos: Vec<RepoEntry>,
}

#[derive(Debug, Deserialize)]
struct RepoEntry {
    /// Member path, absolute or relative to the workspace file's dir.
    path: PathBuf,
    /// Optional display name; defaults to the resolved dir's file name.
    #[serde(default)]
    name: Option<String>,
}

/// A resolved workspace member repo.
#[derive(Debug, Clone)]
pub struct Member {
    /// Canonical filesystem root of the member repo.
    pub root: PathBuf,
    /// Display name (manifest `name`, else the dir's file name).
    pub display_name: String,
}

impl Member {
    /// The member's per-repo index directory — identical to what
    /// single-repo mode computes for this root, so a repo indexed
    /// standalone and as a member shares one index dir.
    pub fn index_dir(&self) -> PathBuf {
        config::index_dir(&self.root)
    }
}

/// A loaded workspace: a disjoint set of member repos plus the canonical
/// path of the manifest they came from.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Resolved, disjoint member repos in manifest order.
    pub members: Vec<Member>,
    /// Canonical path of the loaded `.vex-workspace.toml`.
    pub file: PathBuf,
}

impl Workspace {
    /// Parse and resolve a workspace manifest at `file`. Enforces the
    /// module invariants; returns an error describing the first violation.
    pub fn load(file: &Path) -> Result<Workspace> {
        let content = std::fs::read_to_string(file)
            .with_context(|| format!("read workspace file {}", file.display()))?;
        let manifest: WorkspaceManifest =
            toml::from_str(&content).with_context(|| format!("parse {}", file.display()))?;
        let file = file
            .canonicalize()
            .with_context(|| format!("canonicalize {}", file.display()))?;
        // A canonicalized file path always has a parent (it is never the
        // filesystem root); falling back to CWD here would silently resolve
        // members against the wrong base.
        let base = file
            .parent()
            .expect("canonicalized workspace file has a parent directory");

        if manifest.repos.is_empty() {
            bail!("workspace {} declares no [[repo]] members", file.display());
        }

        let mut members: Vec<Member> = Vec::with_capacity(manifest.repos.len());
        for entry in &manifest.repos {
            members.push(resolve_member(entry, base)?);
        }
        reject_overlaps(&members)?;

        Ok(Workspace { members, file })
    }
}

/// Resolve a single manifest entry into a [`Member`], canonicalizing its
/// path and enforcing the directory + platform-cache constraints.
fn resolve_member(entry: &RepoEntry, base: &Path) -> Result<Member> {
    let joined = if entry.path.is_absolute() {
        entry.path.clone()
    } else {
        base.join(&entry.path)
    };
    let root = joined.canonicalize().with_context(|| {
        format!(
            "workspace member path {:?} does not resolve (relative to {})",
            entry.path,
            base.display()
        )
    })?;
    if !root.is_dir() {
        bail!("workspace member {:?} is not a directory", entry.path);
    }

    // MVP: per-member cache_dir / local_cache would need the not-yet-built
    // CacheResolver (docs/MULTIREPO.md §6.1). `local_cache` is especially
    // unsafe — it drops the hash subdir, so two members sharing one local
    // cache root would collide. Only the member's OWN .vex.toml counts: an
    // ancestor config (e.g. at the workspace root) is the shared/global
    // config, applied to every member by design, not a per-member surprise.
    if member_sets_cache_override(&root)? {
        bail!(
            "workspace member {:?} sets cache_dir/local_cache in its own .vex.toml — \
             not supported in workspace mode yet (docs/MULTIREPO.md §6.1); remove it \
             or index this repo standalone",
            entry.path
        );
    }

    let display_name = entry.name.clone().unwrap_or_else(|| {
        root.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.path.to_string_lossy().into_owned())
    });
    Ok(Member { root, display_name })
}

/// Whether the member's **own** `.vex.toml` (directly at `root`, NOT an
/// ancestor's — unlike `config::load_config`'s walk-up) declares a cache
/// override. Ancestor configs are shared/global and applied to all
/// members, so they are not a per-member rejection trigger.
fn member_sets_cache_override(root: &Path) -> Result<bool> {
    let own = root.join(".vex.toml");
    if !own.is_file() {
        return Ok(false);
    }
    let content =
        std::fs::read_to_string(&own).with_context(|| format!("read {}", own.display()))?;
    let cfg: VexConfig =
        toml::from_str(&content).with_context(|| format!("parse {}", own.display()))?;
    Ok(cfg.cache_dir.is_some() || cfg.local_cache == Some(true))
}

/// Reject members that resolve to the same path or that are nested inside
/// one another — members must be disjoint so their indexes never overlap.
fn reject_overlaps(members: &[Member]) -> Result<()> {
    for (i, a) in members.iter().enumerate() {
        for b in &members[i + 1..] {
            // Equal paths also satisfy `starts_with` below, but check them
            // first so the error says "same path" rather than "nested".
            if a.root == b.root {
                bail!(
                    "workspace members {:?} and {:?} resolve to the same path {}",
                    a.display_name,
                    b.display_name,
                    a.root.display()
                );
            }
            if a.root.starts_with(&b.root) || b.root.starts_with(&a.root) {
                bail!(
                    "workspace members {:?} ({}) and {:?} ({}) are nested; members \
                     must be disjoint",
                    a.display_name,
                    a.root.display(),
                    b.display_name,
                    b.root.display()
                );
            }
        }
    }
    Ok(())
}

/// Walk up from `start` looking for a [`WORKSPACE_FILE`]. Mirrors
/// `config::load_config`'s walk-up so a command run from inside a member
/// can still find its workspace root. The returned path is **not**
/// canonicalized (it is joined onto `start`'s components); pass it to
/// [`Workspace::load`], which canonicalizes internally.
pub fn find_workspace_file(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(WORKSPACE_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}
