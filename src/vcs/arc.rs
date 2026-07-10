//! Yandex **Arc** backend (diff-scoping).
//!
//! Field-verified 2026-07-10 against a real `arc` install (an `arcadia`
//! working copy): the command shapes below are confirmed from live captures,
//! not research-grounded. `arc root`, `arc diff <from> <to> --name-only
//! --no-color`, `arc diff -B --name-only`, and `arc status --json` were all
//! run against real Arc — see the capture log in `docs/VCS-BACKENDS.md §7a`.
//!
//! **Reachability (Phase 3):** `ArcVcs` is selected only via an explicit
//! override (`--vcs arc` / `VEX_VCS=arc` / `.vex.toml vcs = "arc"`) or a `.arc`
//! marker. The `arc root` FUSE auto-probe in detection is **deferred** (it
//! would add VFS latency to every `arc`-on-PATH invocation) — see
//! `docs/VCS-BACKENDS.md`.
//!
//! Key Arc facts (all field-verified):
//! - Detection is `arc root` — prints the working-copy root, exits non-zero
//!   outside one.
//! - `arc diff -B --name-only` diffs `merge-base(trunk, HEAD)..HEAD` in a
//!   single command (default FROM=trunk, TO=HEAD) — the arc-native "since
//!   branched" scope, so no separate `merge-base` + `diff` two-step is needed.
//! - `arc status --json` → `{ "status": { changed/staged/untracked: [{path}] } }`,
//!   untracked already included (no separate `ls-files --others`).
//! - `--name-only` paths are repo-root-relative (like git) and
//!   newline-separated. Arc has no `-z` flag, so we newline-split; paths with
//!   embedded newlines are unsupported.
//! - Arc documents no `--` end-of-options terminator (unlike git), so a
//!   `--since` revision beginning with `-` is rejected up front rather than
//!   guarded with a trailing `--`.

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
        // Arc supports merge-base (`arc merge-base --leftmost trunk HEAD`,
        // field-verified) — though `SinceBranched` uses the simpler `arc diff
        // -B`, which computes the same base in one command.
        // `content_addressed: false` for now: arc blob SHAs are git-compatible
        // (it COULD feed the parse cache), but `arc ls-files` is not yet
        // field-verified — so arc declines `tracked_content_ids` until it is.
        VcsCapabilities {
            merge_base: true,
            content_addressed: false,
        }
    }

    fn ensure_repo(&self, root: &Path) -> VcsResult<()> {
        // `arc root` prints the working-copy root and exits non-zero outside
        // one (field-verified) — the detection idiom. The diff-scope path
        // fails loud on any arc error (H2).
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
        DiffScope::SinceBranched => arc_diff_base(root),
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

/// `arc diff <from> <to> --name-only --no-color` → newline-separated,
/// repo-root-relative paths (field-verified 2026-07-10). The two positional-rev
/// form (not a `..` range) is what real arc uses; `--no-color` forces color
/// mode `never`. Arc has no `-z` flag, so we newline-split.
///
/// Arc documents no `--` end-of-options terminator (unlike git), so the git
/// backend's `--` flag-injection guard is unavailable. Instead a revision
/// beginning with `-` is rejected up front — otherwise `arc` would parse it as
/// an option. `to` is always the literal `HEAD` here; only a caller-supplied
/// `--since` value can carry a leading `-`.
fn arc_diff_name_only(root: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    reject_flaglike_rev(from)?;
    reject_flaglike_rev(to)?;
    let stdout = arc(root, &["diff", from, to, "--name-only", "--no-color"])?;
    Ok(split_lines(&stdout))
}

/// `SinceBranched` via `arc diff -B --name-only --no-color`: `-B` diffs
/// `merge-base(FROM, TO)..TO` with FROM defaulting to `trunk` and TO to `HEAD`
/// (field-verified 2026-07-10) — the arc-native "changes since this branch left
/// trunk" scope, in a single command with no assumption about which trunk ref
/// name resolves (vs a manual `merge-base` + `diff` two-step).
fn arc_diff_base(root: &Path) -> Result<Vec<String>> {
    let stdout = arc(root, &["diff", "-B", "--name-only", "--no-color"])?;
    Ok(split_lines(&stdout))
}

/// Reject a revision that `arc` would parse as an option. Arc has no `--`
/// end-of-options terminator, so a leading-`-` rev (e.g. from a
/// programmatic/MCP `--since` caller) must be refused rather than smuggled
/// past a separator the way the git backend does.
fn reject_flaglike_rev(rev: &str) -> Result<()> {
    if rev.starts_with('-') {
        bail!(
            "invalid arc revision {rev:?}: begins with `-` and would be parsed \
             as a flag (arc has no `--` end-of-options terminator). Use a \
             revision that does not start with `-`."
        );
    }
    Ok(())
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
/// `untracked` arrays under the top-level `status` object.
///
/// **Graceful-degradation guard (fail loud, never silently empty):** the shape
/// is field-verified (§7a), but a future `arc` whose JSON differs would
/// otherwise parse cleanly and yield an **empty** change set — silently
/// reporting "nothing changed" and dropping every result from a
/// `--changed-only` search (the delete-safety footgun H2 exists to prevent,
/// but which H2's error→empty rule does NOT cover for a *successful* call with
/// an unexpected shape). So we require the shape to be *recognizable* — a
/// top-level `status` object, or at least one of the expected groups — and
/// `bail!` otherwise. An empty change set is only returned when the shape IS
/// recognized and the groups are genuinely empty.
fn parse_arc_status_json(text: &str) -> Result<Vec<String>> {
    let root: Value = serde_json::from_str(text.trim()).context("arc status JSON")?;
    const GROUPS: [&str; 3] = ["changed", "staged", "untracked"];
    let container = root.get("status");
    let scope = container.unwrap_or(&root);
    let recognized = container.is_some() || GROUPS.iter().any(|g| scope.get(g).is_some());
    if !recognized {
        bail!(
            "unrecognized `arc status --json` shape (no `status` object nor \
             changed/staged/untracked groups) — this `arc`'s output does not \
             match the field-verified shape. Refusing to report an empty change \
             set from an unrecognized shape; see docs/VCS-BACKENDS.md §7a."
        );
    }
    let mut out = Vec::new();
    for group in GROUPS {
        if let Some(arr) = scope.get(group).and_then(Value::as_array) {
            for entry in arr {
                if let Some(p) = entry.get("path").and_then(Value::as_str) {
                    out.push(p.to_string());
                }
            }
        }
    }
    Ok(out)
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
        // Recognized envelope (a `status` object) with empty groups → a genuine
        // "nothing changed" empty set, NOT an error.
        let empty = r#"{ "status": {} }"#;
        assert!(parse_arc_status_json(empty).unwrap().is_empty());
    }

    /// Graceful-degradation guard: a valid-JSON but *unrecognized* shape (what a
    /// real `arc` whose output differs from our assumption would produce) must
    /// FAIL LOUD, never silently return an empty change set (delete-safety).
    #[test]
    fn parse_arc_status_json_unrecognized_shape_fails_loud() {
        // No `status` object and none of the expected groups → error.
        let err = parse_arc_status_json(r#"{ "files": ["a.rs"], "revision": 42 }"#)
            .expect_err("unrecognized shape must error, not yield empty");
        assert!(
            format!("{err:#}").contains("unrecognized"),
            "error must name the unrecognized-shape cause, got: {err:#}"
        );
        // A bare object with no keys is also unrecognized (ambiguous vs. a real
        // clean-tree shape, which the research says carries a `status` object).
        assert!(parse_arc_status_json("{}").is_err());
        // But a top-level group without the `status` wrapper is still accepted
        // (defensive: some arc versions may flatten).
        assert_eq!(
            parse_arc_status_json(r#"{ "changed": [{"path": "flat.rs"}] }"#).unwrap(),
            vec!["flat.rs"]
        );
    }

    #[test]
    fn reject_flaglike_rev_blocks_leading_dash() {
        // A leading-`-` rev would be parsed as an arc option (no `--` guard).
        let err = reject_flaglike_rev("-rf").expect_err("leading-dash rev must be rejected");
        assert!(format!("{err:#}").contains("begins with `-`"));
        // Ordinary revisions pass untouched.
        assert!(reject_flaglike_rev("HEAD").is_ok());
        assert!(reject_flaglike_rev("trunk~3").is_ok());
        assert!(reject_flaglike_rev("a7f4ba4f5a422cc03c45343d5db3c6d032f3baa4").is_ok());
    }

    #[test]
    fn split_lines_drops_blanks_and_trailing_newline() {
        assert_eq!(split_lines(b"a.rs\nb.rs\n"), vec!["a.rs", "b.rs"]);
        assert_eq!(split_lines(b""), Vec::<String>::new());
    }
}
