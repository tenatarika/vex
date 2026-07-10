//! Subversion (**svn**) backend (diff-scoping).
//!
//! Field-verified 2026-07-10 against a real `svn` 1.14 working copy (a local
//! `svnadmin`-created repo). Command shapes below are confirmed from live
//! captures — see `docs/VCS-BACKENDS.md` §6 (Phase 4). The `svn::tests`
//! module carries the captured XML as fixtures plus a live end-to-end test.
//!
//! Key svn facts driving the design:
//! - Detection / pre-flight is `svn info`: exit 0 inside a working copy,
//!   non-zero (`E155007: … is not a working copy`) outside one.
//! - svn branches are directory copies with **no merge-base**, so
//!   `DiffScope::SinceBranched` is a capability svn **declines**
//!   (`VcsError::Unsupported`) — a clear error, never a silently-wrong answer.
//! - Machine output is XML (`--xml`), svn's stable contract — the same reason
//!   the Arc backend prefers `--json` over porcelain. Parsing the porcelain
//!   columns would force brittle fixed-offset slicing that breaks on paths
//!   containing spaces (field-verified: `src/with space.rs`).
//! - `Since(rev)` → `svn diff --summarize --xml -r <rev>:HEAD` diffs committed
//!   revisions (server-contacting for a remote repo — inherent to svn's
//!   centralized model). `ChangedOnly` → `svn status --xml`, which lists both
//!   local modifications and unversioned files in one offline call.
//! - Because the `Since` call can hit the network, every `svn` invocation runs
//!   under a bounded wall-clock timeout (shared `proc::wait_capturing` /
//!   `vcs_timeout`, default 60s, override `VEX_VCS_TIMEOUT_SECS`) so an
//!   unreachable/hung server can't hang `vex`.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::proc::{vcs_timeout, wait_capturing};
use super::{DiffScope, Vcs, VcsCapabilities, VcsError, VcsKind, VcsResult};

/// Message for the declined `SinceBranched` scope. svn has no merge-base, so
/// the flag stays accepted at the CLI but resolves to `Unsupported` with an
/// actionable redirect to `--since`.
const SINCE_BRANCHED_MSG: &str = "--since-branched requires merge-base, which svn does not \
     support (svn branches are directory copies with no common ancestor). Use `--since <rev>` \
     with an svn revision (e.g. `--since 42`) instead.";

/// Subversion VCS backend. Fieldless — every op shells out to `svn` in `root`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SvnVcs;

impl Vcs for SvnVcs {
    fn kind(&self) -> VcsKind {
        VcsKind::Svn
    }

    fn capabilities(&self) -> VcsCapabilities {
        // svn branches are directory copies with no merge-base → SinceBranched
        // is declined (see `SINCE_BRANCHED_MSG`).
        // svn has no content-addressed blob store → declines
        // `tracked_content_ids` (parse cache falls back to xxh3/mtime).
        VcsCapabilities {
            merge_base: false,
            content_addressed: false,
        }
    }

    fn ensure_repo(&self, root: &Path) -> VcsResult<()> {
        // `svn info` exits non-zero outside a working copy (`E155007`) —
        // the pre-flight guard (H3). The diff-scope path fails loud (H2).
        match svn(root, &["info"]) {
            Ok(_) => Ok(()),
            Err(e) => Err(VcsError::Failed(e.context(format!(
                "not an svn working copy at {} (or `svn` unavailable): \
                 --since/--changed-only require an svn checkout",
                root.display()
            )))),
        }
    }

    fn changed_paths(&self, root: &Path, scope: DiffScope) -> VcsResult<Vec<String>> {
        match scope {
            DiffScope::Since(rev) => svn_since(root, rev).map_err(VcsError::Failed),
            // Declined capability — NOT a failure. H2: distinct from `Failed`.
            DiffScope::SinceBranched => Err(VcsError::Unsupported(SINCE_BRANCHED_MSG.to_string())),
            DiffScope::ChangedOnly => svn_changed_only(root).map_err(VcsError::Failed),
        }
    }
}

/// Run `svn` with `args` in `root`, returning stdout on success.
///
/// A spawn failure (svn not installed) and a non-zero exit both surface as
/// `Err` with actionable context — never a silent empty set (H2).
fn svn(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let label = format!("svn {}", args.join(" "));
    let child = Command::new("svn")
        .args(args)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!("failed to invoke `{label}` — is Subversion installed and on PATH?")
        })?;
    let output = wait_capturing(child, vcs_timeout(), &label)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{label} failed: {}", stderr.trim());
    }
    Ok(output.stdout)
}

/// `Since(rev)` → `svn diff --summarize --xml -r <rev>:HEAD` (field-verified).
/// Lists paths changed between committed revision `<rev>` and repository HEAD.
fn svn_since(root: &Path, rev: &str) -> Result<Vec<String>> {
    reject_flaglike_rev(rev)?;
    let range = format!("{rev}:HEAD");
    let stdout = svn(root, &["diff", "--summarize", "--xml", "-r", &range])?;
    let text = String::from_utf8_lossy(&stdout);
    parse_svn_diff_summarize_xml(&text)
        .with_context(|| format!("could not parse `svn diff --summarize --xml`: {text:.200}"))
}

/// `ChangedOnly` → `svn status --xml` (field-verified). One offline call lists
/// local modifications *and* unversioned (untracked) files.
fn svn_changed_only(root: &Path) -> Result<Vec<String>> {
    let stdout = svn(root, &["status", "--xml"])?;
    let text = String::from_utf8_lossy(&stdout);
    parse_svn_status_xml(&text)
        .with_context(|| format!("could not parse `svn status --xml`: {text:.200}"))
}

/// Reject a revision that `svn` would choke on as a flag-like token. svn's
/// `-r` rejects a leading-`-` range with a raw `E205000` syntax error; a
/// vex-authored message up front is clearer, and closes the same
/// programmatic/MCP flag-injection surface the arc backend guards.
fn reject_flaglike_rev(rev: &str) -> Result<()> {
    if rev.starts_with('-') {
        bail!(
            "invalid svn revision {rev:?}: begins with `-`. Use a plain svn \
             revision such as `42`, `BASE`, or `{{2026-01-01}}`."
        );
    }
    Ok(())
}

/// Statuses worth reporting as a "changed" path. svn only emits deviating
/// entries, but an entry can be content-`normal` with a prop-only change (or
/// be `ignored`/`external`), which we exclude — a code-search scope should key
/// on content, matching git's content-only `--name-only`. Everything else
/// (`modified`, `added`, `deleted`, `replaced`, `unversioned`, `conflicted`,
/// `missing`, …) is reported. Unknown statuses are reported rather than
/// silently dropped (fail-safe toward over-inclusion, never under).
fn is_reportable_status(item: &str) -> bool {
    !matches!(item, "normal" | "none" | "ignored" | "external")
}

/// Pure parser for `svn status --xml`, unit-testable without an `svn` binary.
/// Collects the `path` of each `<entry>` whose `<wc-status item>` is
/// reportable (see [`is_reportable_status`]).
///
/// **Graceful-degradation guard (fail loud, never silently empty):** a future
/// `svn` whose XML shape differs would otherwise parse to an empty change set
/// and silently report "nothing changed" — dropping every `--changed-only`
/// result (the delete-safety footgun H2 misses for a *successful* call with an
/// unexpected shape). So we require a recognizable `<status>` root and `bail!`
/// otherwise; an empty set is returned only from a recognized, genuinely-empty
/// document.
fn parse_svn_status_xml(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut saw_root = false;
    let mut current_path: Option<String> = None;
    let mut out = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|e| anyhow::anyhow!("svn status --xml parse error: {e}"))?
        {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => match e.name().as_ref() {
                b"status" => saw_root = true,
                b"entry" => current_path = attr_value(&e, b"path")?,
                b"wc-status" => {
                    let reportable = attr_value(&e, b"item")?
                        .as_deref()
                        .map(is_reportable_status)
                        .unwrap_or(false);
                    match current_path.take() {
                        Some(p) if reportable => out.push(p),
                        _ => {}
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    if !saw_root {
        bail!(
            "unrecognized `svn status --xml` output (no `<status>` root) — this \
             `svn`'s output does not match the field-verified shape. Refusing to \
             report an empty change set from an unrecognized shape."
        );
    }
    Ok(out)
}

/// Pure parser for `svn diff --summarize --xml`. Collects the text of each
/// `<path>` whose `item` is reportable. Same fail-loud guard as
/// [`parse_svn_status_xml`]: requires a recognizable `<diff>`/`<paths>` root.
fn parse_svn_diff_summarize_xml(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut saw_root = false;
    let mut current_item: Option<String> = None;
    let mut in_path = false;
    let mut out = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|e| anyhow::anyhow!("svn diff --summarize --xml parse error: {e}"))?
        {
            Event::Eof => break,
            // svn emits `<path …>TEXT</path>` — always a Start with text
            // content, never a self-closing `<path/>` (field-verified across
            // add/delete/modify/prop-only/rename). So matching only `Start`
            // here is intentional; a path carries its text, and there is no
            // empty-path form to lose. (The status parser handles `Start |
            // Empty` because `<wc-status/>` genuinely can be self-closing.)
            Event::Start(e) => match e.name().as_ref() {
                b"diff" | b"paths" => saw_root = true,
                b"path" => {
                    in_path = true;
                    current_item = attr_value(&e, b"item")?;
                }
                _ => {}
            },
            Event::Text(e) if in_path => {
                let p = e
                    .unescape()
                    .map_err(|err| anyhow::anyhow!("svn diff path decode error: {err}"))?
                    .into_owned();
                let reportable = current_item
                    .as_deref()
                    .map(is_reportable_status)
                    .unwrap_or(true);
                if !p.is_empty() && reportable {
                    out.push(p);
                }
            }
            Event::End(e) if e.name().as_ref() == b"path" => {
                in_path = false;
                current_item = None;
            }
            _ => {}
        }
    }
    if !saw_root {
        bail!(
            "unrecognized `svn diff --summarize --xml` output (no `<diff>` root) \
             — this `svn`'s output does not match the field-verified shape. \
             Refusing to report an empty change set from an unrecognized shape."
        );
    }
    Ok(out)
}

/// Read an attribute value by name from a start/empty tag, XML-unescaped.
///
/// `Ok(None)` means the attribute is genuinely absent; a **malformed** attribute
/// (bad attribute syntax, or an undecodable entity escape) is `Err`, NOT `None`.
/// Collapsing "malformed" into "absent" would let a corrupt `item`/`path` attr
/// silently drop an entry from the change set — the same error→empty footgun
/// the document-shape guard (H2) exists to prevent, one level down.
fn attr_value(e: &BytesStart, key: &[u8]) -> Result<Option<String>> {
    for attr in e.attributes() {
        let attr = attr.context("malformed attribute in svn XML")?;
        if attr.key.as_ref() == key {
            let value = attr.unescape_value().with_context(|| {
                format!(
                    "undecodable svn XML attribute {:?}",
                    String::from_utf8_lossy(key)
                )
            })?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real capture: `svn status --xml` from a 1.14 working copy with a
    /// modified, an added, an unversioned, and an added path *containing a
    /// space* — the case that breaks porcelain fixed-offset slicing.
    const STATUS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<status>
<target path=".">
<entry path="src/b.rs">
<wc-status item="modified" revision="2" props="none">
<commit revision="1"><author>furcas</author><date>2026-07-10T18:46:19.072993Z</date></commit>
</wc-status>
</entry>
<entry path="src/d.rs">
<wc-status item="added" revision="-1" props="none"></wc-status>
</entry>
<entry path="src/e.rs">
<wc-status item="unversioned" props="none"></wc-status>
</entry>
<entry path="src/with space.rs">
<wc-status props="none" item="added" revision="-1"></wc-status>
</entry>
</target>
</status>"#;

    /// Real capture: `svn diff --summarize --xml -r 1:HEAD`.
    const DIFF_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<diff>
<paths>
<path props="none" kind="file" item="modified">src/a.rs</path>
<path props="none" kind="file" item="added">src/c.rs</path>
</paths>
</diff>"#;

    #[test]
    fn parse_status_collects_reportable_paths_incl_spaces() {
        let paths = parse_svn_status_xml(STATUS_XML).unwrap();
        assert_eq!(
            paths,
            vec!["src/b.rs", "src/d.rs", "src/e.rs", "src/with space.rs"]
        );
    }

    #[test]
    fn parse_diff_summarize_collects_paths() {
        let paths = parse_svn_diff_summarize_xml(DIFF_XML).unwrap();
        assert_eq!(paths, vec!["src/a.rs", "src/c.rs"]);
    }

    #[test]
    fn parse_status_excludes_normal_and_ignored() {
        let xml = r#"<status><target path=".">
            <entry path="keep.rs"><wc-status item="modified"></wc-status></entry>
            <entry path="prop_only.rs"><wc-status item="normal" props="modified"></wc-status></entry>
            <entry path="ignored.rs"><wc-status item="ignored"></wc-status></entry>
        </target></status>"#;
        assert_eq!(parse_svn_status_xml(xml).unwrap(), vec!["keep.rs"]);
    }

    #[test]
    fn parse_status_recognized_but_empty_is_ok() {
        // A recognized `<status>` root with no changed entries → genuine
        // "nothing changed", NOT an error.
        let xml = r#"<status><target path="."></target></status>"#;
        assert!(parse_svn_status_xml(xml).unwrap().is_empty());
    }

    #[test]
    fn parse_status_unrecognized_shape_fails_loud() {
        let err = parse_svn_status_xml(r#"<foo><bar/></foo>"#)
            .expect_err("unrecognized shape must error, not yield empty");
        assert!(format!("{err:#}").contains("unrecognized"));
    }

    #[test]
    fn parse_diff_unrecognized_shape_fails_loud() {
        assert!(parse_svn_diff_summarize_xml(r#"<foo/>"#).is_err());
    }

    #[test]
    fn parse_status_malformed_attribute_fails_loud() {
        // An unquoted attribute value is malformed XML on the inclusion-driving
        // `path` attr → must Err, not silently drop the entry (H2, one level
        // below the document-shape guard).
        let xml = r#"<status><target path=".">
            <entry path=unquoted><wc-status item="modified"></wc-status></entry>
        </target></status>"#;
        assert!(parse_svn_status_xml(xml).is_err());
    }

    #[test]
    fn reject_flaglike_rev_blocks_leading_dash() {
        assert!(reject_flaglike_rev("-5").is_err());
        assert!(reject_flaglike_rev("42").is_ok());
        assert!(reject_flaglike_rev("BASE").is_ok());
        assert!(reject_flaglike_rev("{2026-01-01}").is_ok());
    }

    // ---- end-to-end against a real `svn` (skipped when svn is absent) ----

    use tempfile::TempDir;

    fn svn_available() -> bool {
        ["svn", "svnadmin"].iter().all(|bin| {
            Command::new(bin)
                .arg("--version")
                .arg("--quiet")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
    }

    fn run_in(dir: &Path, program: &str, args: &[&str]) {
        let status = Command::new(program)
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
        assert!(status.success(), "{program} {args:?} failed");
    }

    /// Exercises the real `SvnVcs` trait impl against a live `svnadmin` repo:
    /// `ensure_repo` (inside/outside), all three `DiffScope` arms. This is the
    /// field-verify guard the arc backend could not have (arc is unavailable),
    /// so it runs unconditionally *when svn is installed* and skips otherwise.
    #[test]
    fn end_to_end_against_real_svn() {
        if !svn_available() {
            eprintln!("skipping end_to_end_against_real_svn: svn/svnadmin not on PATH");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let wc = tmp.path().join("wc");
        run_in(tmp.path(), "svnadmin", &["create", repo.to_str().unwrap()]);
        let url = format!("file://{}", repo.display());
        run_in(tmp.path(), "svn", &["-q", "co", &url, wc.to_str().unwrap()]);

        // r1: two files.
        std::fs::create_dir(wc.join("src")).unwrap();
        std::fs::write(wc.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(wc.join("src/b.rs"), "fn b() {}\n").unwrap();
        run_in(&wc, "svn", &["-q", "add", "src"]);
        run_in(&wc, "svn", &["-q", "commit", "-m", "r1"]);
        run_in(&wc, "svn", &["-q", "up"]);
        // r2: modify a, add c.
        std::fs::write(wc.join("src/a.rs"), "fn a() {}\nfn a2() {}\n").unwrap();
        std::fs::write(wc.join("src/c.rs"), "fn c() {}\n").unwrap();
        run_in(&wc, "svn", &["-q", "add", "src/c.rs"]);
        run_in(&wc, "svn", &["-q", "commit", "-m", "r2"]);
        run_in(&wc, "svn", &["-q", "up"]);
        // Working-tree changes: modify b, add d, leave e untracked.
        std::fs::write(wc.join("src/b.rs"), "fn b() {}\nfn b2() {}\n").unwrap();
        std::fs::write(wc.join("src/d.rs"), "fn d() {}\n").unwrap();
        run_in(&wc, "svn", &["-q", "add", "src/d.rs"]);
        std::fs::write(wc.join("src/e.rs"), "fn e() {}\n").unwrap();

        // ensure_repo: ok inside the working copy, Failed outside it.
        assert!(SvnVcs.ensure_repo(&wc).is_ok());
        assert!(matches!(
            SvnVcs.ensure_repo(tmp.path()),
            Err(VcsError::Failed(_))
        ));

        // Since(1): committed r1->HEAD (=r2) → a modified, c added.
        let mut since = SvnVcs.changed_paths(&wc, DiffScope::Since("1")).unwrap();
        since.sort();
        assert_eq!(since, vec!["src/a.rs", "src/c.rs"]);

        // ChangedOnly: working-tree modifications + untracked.
        let mut changed = SvnVcs.changed_paths(&wc, DiffScope::ChangedOnly).unwrap();
        changed.sort();
        assert_eq!(changed, vec!["src/b.rs", "src/d.rs", "src/e.rs"]);

        // SinceBranched: declined (Unsupported), not Failed, not empty.
        match SvnVcs.changed_paths(&wc, DiffScope::SinceBranched) {
            Err(VcsError::Unsupported(msg)) => assert!(msg.contains("merge-base")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
