//! Yandex **Arc** backend (diff-scoping) — **PROVISIONAL / UNVERIFIED**.
//!
//! Arc is Yandex-internal and the `arc` CLI could not be run on the dev
//! machine, so every command shape below is grounded in **public research**
//! (third-party arc clients: EVGVir/yandex-arc, anton-rudeshko/zsh-arc; the
//! Yandex Habr writeup) rather than a field capture. Each invocation is marked
//! `FIELD-VERIFY` with its confidence — these MUST be confirmed against a real
//! `arc` install before this backend is trusted in anger.
//!
//! **Reachability (Phase 3):** `ArcVcs` is selected only via an explicit
//! override (`--vcs arc` / `VEX_VCS=arc` / `.vex.toml vcs = "arc"`) or a `.arc`
//! marker. The `arc root` FUSE auto-probe in detection is **deferred** (it is
//! unverifiable and would add VFS latency to every `arc`-on-PATH invocation) —
//! see `docs/VCS-BACKENDS.md`.
//!
//! Key Arc facts driving the shapes below:
//! - Detection primitive is `arc root` (exit-code + path), NOT
//!   `arc rev-parse --is-inside-work-tree` (the latter is not attested).
//! - Arc's trunk is literally `trunk`; the remote is `arcadia` → merge-base
//!   ladder is `arcadia/trunk` → `trunk` (NOT git's main/master).
//! - `--json` is Arc's stable machine contract; `arc status --json` already
//!   includes untracked files (no separate `ls-files --others`).
//! - `-z` null-terminated output is NOT attested → we newline-split (paths
//!   with embedded newlines are unsupported until `-z`/`--json` diff is
//!   field-verified).

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::{DiffScope, Vcs, VcsCapabilities, VcsError, VcsKind, VcsResult};

/// Yandex Arc VCS backend. Fieldless — every op shells out to `arc` in `root`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ArcVcs;

impl Vcs for ArcVcs {
    fn kind(&self) -> VcsKind {
        VcsKind::Arc
    }

    fn capabilities(&self) -> VcsCapabilities {
        // Arc has `merge-base` (verified: `arc merge-base --leftmost trunk …`).
        VcsCapabilities { merge_base: true }
    }

    fn ensure_repo(&self, root: &Path) -> VcsResult<()> {
        // FIELD-VERIFY (high): `arc root` prints the working-copy root and exits
        // non-zero outside one — the attested detection idiom (zsh-arc
        // `arc root || echo .`). No `arc rev-parse --is-inside-work-tree`.
        match arc(root, &["root"]) {
            Ok(_) => Ok(()),
            Err(e) => Err(VcsError::Failed(e.context(format!(
                "not an arc working copy at {} (or `arc` unavailable): \
                 --since/--since-branched/--changed-only require an arc checkout",
                root.display()
            )))),
        }
    }

    fn changed_paths(&self, root: &Path, scope: DiffScope) -> VcsResult<Vec<String>> {
        arc_changed_paths(root, scope).map_err(VcsError::Failed)
    }
}

fn arc_changed_paths(root: &Path, scope: DiffScope) -> Result<Vec<String>> {
    match scope {
        DiffScope::Since(rev) => arc_diff_name_only(root, rev, "HEAD"),
        DiffScope::SinceBranched => {
            let base = resolve_arc_merge_base(root)?;
            arc_diff_name_only(root, &base, "HEAD")
        }
        DiffScope::ChangedOnly => arc_status_changed(root),
    }
}

/// Run `arc` with `args` in `root`, returning stdout on success.
///
/// A spawn failure (arc not installed) and a non-zero exit both surface as
/// `Err` with actionable context — never a silent empty set (H2).
fn arc(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = std::process::Command::new("arc")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| {
            format!(
                "failed to invoke `arc {}` — is the arc CLI installed and on PATH?",
                args.join(" ")
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("arc {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(output.stdout)
}

/// FIELD-VERIFY (diff --name-only high; two-arg form high; `-z`/`--` unknown):
/// `arc diff <from> <to> --name-only --no-color --` → newline-separated paths.
/// The two positional-rev form (not a `..` range) is what real arc clients use;
/// `--no-color` strips ANSI; no `-z` is attested so we split on newlines.
///
/// The trailing `--` mirrors `GitVcs::git_diff_name_only`'s flag-injection
/// guard: a `--since` value beginning with `-` (e.g. from a programmatic/MCP
/// caller) must not be parsed as an `arc` flag. FIELD-VERIFY that `arc diff`
/// accepts a trailing `--` and that it fully prevents a leading-`-` revision
/// from being read as an option; if arc rejects `--`, drop it and instead
/// validate/reject `-`-leading revs up front.
fn arc_diff_name_only(root: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let stdout = arc(root, &["diff", from, to, "--name-only", "--no-color", "--"])?;
    Ok(split_lines(&stdout))
}

/// FIELD-VERIFY (status --json high; untracked-inclusion high): `arc status
/// --json` → `{ "status": { "changed":[{"path":…}], "staged":[…],
/// "untracked":[…], "unmerged":[…] } }`. Untracked files are already included,
/// so no separate `ls-files --others` call is needed.
fn arc_status_changed(root: &Path) -> Result<Vec<String>> {
    let stdout = arc(root, &["status", "--json"])?;
    let text = String::from_utf8_lossy(&stdout);
    parse_arc_status_json(&text)
        .with_context(|| format!("could not parse `arc status --json` output: {text:.200}"))
}

/// Pure parser for `arc status --json`, split out so it is unit-testable
/// without an `arc` binary. Collects `path` from the `changed`, `staged`, and
/// `untracked` arrays under the top-level `status` object; tolerates any of
/// them being absent.
fn parse_arc_status_json(text: &str) -> Result<Vec<String>> {
    let root: Value = serde_json::from_str(text.trim()).context("arc status JSON")?;
    let status = root.get("status").unwrap_or(&root);
    let mut out = Vec::new();
    for group in ["changed", "staged", "untracked"] {
        if let Some(arr) = status.get(group).and_then(Value::as_array) {
            for entry in arr {
                if let Some(p) = entry.get("path").and_then(Value::as_str) {
                    out.push(p.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// FIELD-VERIFY (merge-base high; trunk=`trunk`/remote=`arcadia` high):
/// resolve the merge-base for `SinceBranched` against Arc's trunk. The ladder
/// is `arcadia/trunk` (remote) → `trunk` (local) — NOT git's main/master,
/// which do not exist in Arcadia.
fn resolve_arc_merge_base(root: &Path) -> Result<String> {
    const CANDIDATES: &[&str] = &["arcadia/trunk", "trunk"];
    let mut tried = Vec::with_capacity(CANDIDATES.len());
    for cand in CANDIDATES {
        if let Some(base) = try_arc_merge_base(root, cand)? {
            return Ok(base);
        }
        tried.push(*cand);
    }
    bail!(
        "--since-branched: no merge-base found against any of {}. \
         Use `vex search ... --since <rev>` with an explicit arc revision instead.",
        tried.join(", ")
    );
}

/// FIELD-VERIFY: `arc merge-base --leftmost <ref> HEAD`. `--leftmost` is the
/// attested variant (zsh-arc `arc merge-base --leftmost trunk …`). A missing
/// ref / no merge-base is a normal fall-through (`Ok(None)`); only an arc
/// spawn/other failure is `Err`.
fn try_arc_merge_base(root: &Path, reference: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("arc")
        .args(["merge-base", "--leftmost", reference, "HEAD"])
        .current_dir(root)
        .output()
        .with_context(|| "failed to invoke `arc merge-base` — is the arc CLI on PATH?")?;
    if output.status.success() {
        let base = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!base.is_empty()).then_some(base));
    }
    // Unknown ref / no merge-base → try the next candidate. (We can't cheaply
    // distinguish "ref absent" from other non-zero exits without field data;
    // treating all non-zero here as fall-through mirrors the git backend.)
    Ok(None)
}

/// Split newline-terminated arc output into a clean `Vec<String>`. Tolerant of
/// a trailing newline and blank lines. (Newline-based because `arc`'s `-z`
/// support is unverified — see the module note.)
fn split_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_arc_status_json_collects_changed_staged_untracked_paths() {
        // Shape per the Emacs arc client's `arc status --json` parsing.
        let json = r#"{
            "status": {
                "changed":   [{"path": "src/a.rs"}, {"path": "src/b.rs"}],
                "staged":    [{"path": "src/c.rs"}],
                "unmerged":  [{"path": "src/ignored_here.rs"}],
                "untracked": [{"path": "new.rs"}]
            }
        }"#;
        let mut paths = parse_arc_status_json(json).unwrap();
        paths.sort();
        assert_eq!(paths, vec!["new.rs", "src/a.rs", "src/b.rs", "src/c.rs"]);
    }

    #[test]
    fn parse_arc_status_json_tolerates_missing_groups() {
        let json = r#"{ "status": { "changed": [{"path": "only.rs"}] } }"#;
        assert_eq!(parse_arc_status_json(json).unwrap(), vec!["only.rs"]);
        let empty = r#"{ "status": {} }"#;
        assert!(parse_arc_status_json(empty).unwrap().is_empty());
    }

    #[test]
    fn split_lines_drops_blanks_and_trailing_newline() {
        assert_eq!(split_lines(b"a.rs\nb.rs\n"), vec!["a.rs", "b.rs"]);
        assert_eq!(split_lines(b""), Vec::<String>::new());
    }
}
