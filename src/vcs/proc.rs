//! Shared subprocess helper for VCS backends: run a child under a bounded
//! wall-clock timeout, capturing output.
//!
//! Both the svn and arc backends shell out to a CLI that can block — svn `diff
//! -r<rev>:HEAD` contacts the server for a remote repo, arc reads a FUSE/VFS
//! mount that can stall. `std::process::Command` has no built-in timeout, so an
//! unresponsive backend would hang the whole `vex` call. `wait_capturing`
//! bounds every such invocation and kills+reaps the child if it overruns.

use std::io::Read;
use std::process::{Child, Output};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Bounded wall-clock timeout for a single VCS subprocess. Override with
/// `VEX_VCS_TIMEOUT_SECS`; a missing/zero/invalid value uses the default.
pub(super) fn vcs_timeout() -> Duration {
    const DEFAULT_SECS: u64 = 60;
    let secs = std::env::var("VEX_VCS_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_SECS);
    Duration::from_secs(secs)
}

/// Wait for an already-spawned `child` (whose stdout/stderr MUST be piped),
/// capturing both streams, killing it if it runs longer than `timeout`.
///
/// Both pipes are drained on their own threads first: a child that fills a pipe
/// buffer blocks on the write, indistinguishable from a hang if the parent
/// isn't reading — so we read concurrently, not after `wait`. On timeout the
/// child is killed and reaped and an actionable error returned (never a silent
/// empty set — H2). `label` names the command in errors.
pub(super) fn wait_capturing(mut child: Child, timeout: Duration, label: &str) -> Result<Output> {
    let mut out = child.stdout.take().context("child stdout was not piped")?;
    let mut err = child.stderr.take().context("child stderr was not piped")?;
    let out_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let err_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("waiting on `{label}`"))?
        {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{label}` timed out after {}s — unresponsive backend? \
                 Set VEX_VCS_TIMEOUT_SECS to adjust.",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    // The child has exited; its pipes are closed, so the reader threads have
    // finished (or will imminently) — join to collect the full output.
    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    // The mechanism is OS-agnostic; the tests need a sleepy binary, so they are
    // unix-gated (`sleep`/`printf` aren't on Windows).

    #[cfg(unix)]
    #[test]
    fn wait_capturing_times_out_and_kills_a_hung_child() {
        let child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let err = wait_capturing(child, Duration::from_millis(150), "sleep 30")
            .expect_err("a 30s child under a 150ms budget must time out");
        assert!(
            format!("{err:#}").contains("timed out"),
            "error must name the timeout, got: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wait_capturing_collects_output_of_a_fast_child() {
        let child = Command::new("printf")
            .arg("hello")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn printf");
        let out = wait_capturing(child, Duration::from_secs(10), "printf hello").unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    }
}
