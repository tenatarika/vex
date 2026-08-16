//! Non-blocking refresh of a stale index.
//!
//! `handle_staleness` normally rebuilds **before** answering, so the first query
//! after an edit pays the whole update — on a large corpus, orders of magnitude
//! more than the query itself. That wait turns out to buy nothing: readers take
//! no lock and a live mmap survives the index's atomic rename, so queries issued
//! *while* a rebuild runs cost what any other query costs. The index can be
//! refreshed behind the query instead of in front of it. (The measurements
//! behind this are in the CHANGELOG entry that introduced the flag.)
//!
//! That is what this module does. With `--async-update` (or `async_update` in
//! `.vex.toml`) a stale index is refreshed by a detached `vex update` child
//! while the current index answers now, and the response carries
//! `_meta.vex.dev/stale` with a reason so a caller that needs freshness can see
//! that it did not get it.
//!
//! Four rules keep it honest:
//!
//! * **Only when an index already exists.** A missing index is bootstrapped
//!   synchronously — there is nothing to answer from.
//! * **Only after the embedder-mismatch guard.** That guard's "refuse and serve
//!   stale" outcome must win, or a background child would re-embed with a
//!   different model and mix embedding spaces on disk.
//! * **The child gets `--no-wait`.** Several queries can notice the same stale
//!   index at once (parallel agents do this constantly); the first child takes
//!   the build lock and the rest exit immediately instead of queueing.
//! * **One attempt per cooldown, and its stderr is kept.** `--no-wait` bounds
//!   the *work*, not the process count — a burst of simultaneous queries forks a
//!   child each, and every one pays a binary load, a config load and a stat pass
//!   before losing the lock. A short-TTL attempt marker collapses
//!   that to one, and the child's stderr goes to a log beside the index so a
//!   refresh that keeps failing is diagnosable instead of silently retried.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use crate::util::config;

/// How long one background attempt suppresses the next. Long enough that a
/// burst of parallel queries forks once, short enough that a genuinely failed
/// refresh is retried while the user is still working.
const COOLDOWN: Duration = Duration::from_secs(30);

/// Whether this invocation may refresh in the background. Installed once at
/// dispatch from the global `--async-update` flag, the same way `--vcs`
/// installs its override, so the ~17 call sites that reach `handle_staleness`
/// do not each have to thread a boolean through.
static FLAG: OnceLock<bool> = OnceLock::new();

/// Install the CLI flag. Later calls are ignored (the first wins), matching
/// `vcs::install_override`.
pub(crate) fn install(flag: bool) {
    let _ = FLAG.set(flag);
}

/// Resolve the policy: CLI flag, else `.vex.toml`, else off.
pub(crate) fn enabled(cfg: &config::VexConfig) -> bool {
    *FLAG.get().unwrap_or(&false) || cfg.async_update.unwrap_or(false)
}

/// Marker touched when an attempt starts. Its mtime is the whole state: within
/// [`COOLDOWN`] of it, another query does not fork.
fn marker_path(root: &Path) -> PathBuf {
    config::index_dir(root).join("async_update.attempt")
}

/// Where the child's stderr goes, so a failing refresh leaves evidence.
fn log_path(root: &Path) -> PathBuf {
    config::index_dir(root).join("async_update.log")
}

/// True when a background attempt started recently enough that forking another
/// would only add process churn.
fn attempted_recently(root: &Path) -> bool {
    std::fs::metadata(marker_path(root))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < COOLDOWN)
}

/// Whatever the last child wrote to stderr, trimmed for a one-line reason.
/// Present and non-empty means the previous refresh failed — worth telling the
/// caller, since otherwise "still stale" is indistinguishable from "still
/// running".
fn last_failure(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log_path(root)).ok()?;
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    Some(line.trim().chars().take(200).collect())
}

/// The command line for the background refresh.
///
/// Split out from the spawn so it can be asserted in a unit test: this is the
/// part with decisions in it, and a wrong argv would silently refresh the wrong
/// thing (or refresh it with the wrong sections).
///
/// `--path` is explicit because the child inherits no working directory
/// guarantee, and `--no-wait` is what makes concurrent notices cheap. Section
/// composition is deliberately *not* passed: `vex update` inherits it from the
/// manifest, which is the same source the blocking path reads.
pub(crate) fn argv(exe: &Path, root: &Path, cfg: &config::VexConfig) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec![
        exe.into(),
        "update".into(),
        "--path".into(),
        root.into(),
        "--no-wait".into(),
    ];
    // Semantic is the one option `vex update` cannot infer from the manifest
    // alone: the manifest records that vectors exist, but building them is opt-in
    // per invocation. Mirror what the blocking path passes.
    if cfg.semantic.unwrap_or(false) {
        out.push("--semantic".into());
    }
    out
}

/// What [`spawn`] did, so the caller can tell the user the truth: a reason
/// string for `_meta.vex.dev/stale_reason`, and whether a refresh is actually
/// running. Announcing a background refresh that never started would be worse
/// than saying nothing.
pub(crate) struct Attempt {
    pub(crate) started: bool,
    pub(crate) reason: String,
}

impl Attempt {
    fn started(reason: String) -> Self {
        Self {
            started: true,
            reason,
        }
    }

    fn not_started(reason: String) -> Self {
        Self {
            started: false,
            reason,
        }
    }
}

/// Start the refresh and return immediately. Errors are reported through the
/// returned reason rather than propagated: failing to *start* a background
/// refresh must not fail the query that triggered it.
pub(crate) fn spawn(root: &Path, cfg: &config::VexConfig, changed_count: Option<usize>) -> Attempt {
    // Mirrors the wording the blocking path uses: the deep check reports a
    // count, the cheap HEAD-only check does not.
    let what = match changed_count {
        Some(n) => format!("{n} changed file(s)"),
        None => "HEAD changed".to_string(),
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return Attempt::not_started(format!(
                "index is stale ({what}); could not start a background refresh (cannot locate \
                 the vex executable: {e}) — run `vex update`"
            ))
        }
    };
    // A recent attempt is either still running or just failed; either way,
    // forking again now adds nothing but process churn.
    if attempted_recently(root) {
        return Attempt::not_started(match last_failure(root) {
            Some(err) => format!(
                "index is stale ({what}); the last background refresh reported an error, so the \
                 index has not moved — see {} ({err})",
                log_path(root).display()
            ),
            None => format!(
                "index is stale ({what}); answered from the existing index while a refresh \
                 started moments ago is still running"
            ),
        });
    }

    let previous_failure = last_failure(root);

    // Opening the log is also the writability probe for the index directory,
    // and it has to be: the log and the attempt marker both live there, so an
    // unwritable index dir would otherwise disable the very diagnostics meant to
    // explain it — silently, forever, once per query. If this fails, a refresh
    // cannot write the index either, so say that instead of forking a child
    // whose complaint would go nowhere. (Truncating per attempt also keeps the
    // log to the newest failure rather than growing without bound.)
    let log = match std::fs::File::create(log_path(root)) {
        Ok(f) => f,
        Err(e) => {
            return Attempt::not_started(format!(
                "index is stale ({what}); the index directory {} is not writable ({e}), so no \
                 refresh can succeed — fix permissions and run `vex update`",
                config::index_dir(root).display()
            ))
        }
    };

    let args = argv(&exe, root, cfg);
    let mut cmd = std::process::Command::new(&args[0]);
    cmd.args(&args[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log));

    match cmd.spawn() {
        Ok(child) => {
            // Deliberately not waited on: the child is reparented and reaped by
            // init once this short-lived CLI exits.
            tracing::debug!(pid = child.id(), "started background index refresh");
            let _ = std::fs::write(marker_path(root), child.id().to_string());
            Attempt::started(match previous_failure {
                Some(err) => format!(
                    "index is stale ({what}); answered from the existing index and retried a \
                     refresh that previously failed — see {} ({err})",
                    log_path(root).display()
                ),
                None => format!(
                    "index is stale ({what}); answered from the existing index while a refresh \
                     runs in the background"
                ),
            })
        }
        Err(e) => Attempt::not_started(format!(
            "index is stale ({what}); could not start a background refresh ({e}) — run \
             `vex update`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(semantic: Option<bool>, async_update: Option<bool>) -> config::VexConfig {
        config::VexConfig {
            semantic,
            async_update,
            ..Default::default()
        }
    }

    #[test]
    fn argv_targets_the_root_explicitly_and_never_waits() {
        let got = argv(
            Path::new("/usr/local/bin/vex"),
            Path::new("/repo"),
            &cfg_with(None, None),
        );
        let as_str: Vec<String> = got
            .iter()
            .map(|o| o.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            as_str,
            vec![
                "/usr/local/bin/vex",
                "update",
                "--path",
                "/repo",
                "--no-wait"
            ]
        );
    }

    /// A project that indexes with embeddings must keep them on a background
    /// refresh; dropping them would silently disable semantic search until the
    /// next explicit `vex index --semantic`.
    #[test]
    fn argv_carries_semantic_when_the_project_uses_it() {
        let got = argv(
            Path::new("vex"),
            Path::new("/repo"),
            &cfg_with(Some(true), None),
        );
        assert!(got.iter().any(|a| a == "--semantic"));
    }

    #[test]
    fn config_enables_it_without_the_flag() {
        // `install` is a process-wide OnceLock, so this asserts the config half
        // only — the flag half is covered by the CLI integration test.
        assert!(enabled(&cfg_with(None, Some(true))));
        assert!(!enabled(&cfg_with(None, Some(false))));
        assert!(!enabled(&cfg_with(None, None)));
    }
}
