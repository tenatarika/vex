//! Claude Code handler for `vex mcp install|uninstall|list`.
//!
//! Unlike every other agent, Claude Code is configured by **shelling
//! out to its own CLI** (`claude mcp add|remove|get`) instead of
//! merging a config file:
//!
//! - Claude Code keeps user-scope MCP servers in `~/.claude.json`, a
//!   file that also holds unrelated Claude Code state (projects,
//!   history, OAuth bookkeeping). Its docs recommend `claude mcp add`
//!   over hand-editing it; a vex-side JSON round-trip would race a
//!   running Claude Code session writing the same file.
//! - The pre-fix handler wrote `~/.claude/claude_desktop_config.json`,
//!   which Claude Code never reads (that name is Claude Desktop's), so
//!   `vex mcp install --agent claude-code` silently did nothing useful.
//!
//! vex never reads or writes any Claude file here. When `claude` is not
//! on `PATH`, install/uninstall print the exact command for the user to
//! run and report the agent as needing manual action.
//!
//! The `claude` binary is resolved from `VEX_CLAUDE_BIN` (tests inject a
//! fake through it) and otherwise from `PATH`.
//!
//! **Scope.** vex only ever manages the USER-scope entry. `claude mcp get`
//! searches local, project and user scope alike, so it is never used to
//! decide anything: install runs `add --scope user` and reads claude's
//! "already exists" refusal; uninstall runs `remove --scope user` and
//! reads its "no such server" refusal. A same-named project or local
//! entry is therefore invisible to install/uninstall, as it should be.
//!
//! **Message matching.** Claude Code has no machine-readable exit codes
//! for these refusals, so the two outcomes are recognised by
//! case-insensitive substrings of its output (see [`ALREADY_EXISTS`] /
//! [`NOT_FOUND`]). Wording verified against Claude Code 2.1.285:
//! - `add`, entry present: `MCP server vex already exists in user config`
//! - `remove`, entry absent: `No MCP server named "vex" in user scope`
//! - `get`, entry absent: `No MCP server named "vex". Configured servers: …`
//!
//! Any other non-zero exit is a real error carrying claude's output.

use anyhow::{bail, Context, Result};
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::mcp::{
    home_dir, InstallContext, InstallOutcome, McpAgentHandler, UninstallOutcome,
    DEFAULT_SERVER_NAME,
};

/// Environment override for the `claude` executable. When set, it is
/// used verbatim (no PATH search); a path that does not exist counts as
/// "claude not installed".
pub const CLAUDE_BIN_ENV: &str = "VEX_CLAUDE_BIN";

/// Scope vex registers into: user scope is the only one that matches
/// "available in every project", which is what the file-based handlers
/// give the other agents.
const SCOPE: &str = "user";

/// Explanation printed when `claude` cannot be found.
const MISSING_REASON: &str = "`claude` CLI not found on PATH (or via VEX_CLAUDE_BIN); \
     nothing was written — run the command below once Claude Code is installed";

/// Claude Code — registers vex through `claude mcp add --scope user`.
/// See the module docs for why this is not a file-merging handler.
#[derive(Debug, Default)]
pub struct ClaudeCodeHandler;

impl McpAgentHandler for ClaudeCodeHandler {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
    /// Where Claude Code itself stores user-scope servers. Informational
    /// only — vex never opens this file.
    fn config_path(&self) -> Result<PathBuf> {
        Ok(home_dir()?.join(".claude.json"))
    }
    fn list_source(&self) -> Result<String> {
        Ok(format!(
            "checked `{DEFAULT_SERVER_NAME}` in every scope via \
             `claude mcp get {DEFAULT_SERVER_NAME}`; `claude mcp list` shows every server"
        ))
    }
    fn install(&self, ctx: &InstallContext) -> Result<InstallOutcome> {
        install_via(resolve_claude_bin().as_deref(), ctx)
    }
    fn uninstall(&self, server_name: &str) -> Result<UninstallOutcome> {
        uninstall_via(resolve_claude_bin().as_deref(), server_name)
    }
    fn list_servers(&self) -> Result<Vec<String>> {
        list_via(resolve_claude_bin().as_deref(), DEFAULT_SERVER_NAME)
    }
}

/// Raised by [`ClaudeCodeHandler::list_servers`] when `claude` is absent,
/// so the CLI can render "needs manual action" instead of a hard error.
#[derive(Debug)]
pub struct ClaudeCliMissing;

impl std::fmt::Display for ClaudeCliMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("`claude` CLI not found on PATH (or via VEX_CLAUDE_BIN)")
    }
}

impl std::error::Error for ClaudeCliMissing {}

// ────────────────────────────────────────────────────────────────────
// argv builders (pure)
// ────────────────────────────────────────────────────────────────────

/// Substring (lower-cased) of claude's refusal to `add` a name that is
/// already registered in the target scope.
const ALREADY_EXISTS: &[&str] = &["already exists"];

/// Substrings (lower-cased) of claude's refusal to `remove`/`get` a name
/// that is not registered. "not found" covers older/alternative wording.
const NOT_FOUND: &[&str] = &["no mcp server named", "no mcp server found", "not found"];

/// `claude mcp add --scope user --transport stdio <name> --env
/// VEX_ROOT=<root> -- <vex-mcp>`. `<name>` must precede `--env`: the
/// flag is variadic in Claude Code's parser and would swallow it. The
/// `--` separator is mandatory for stdio servers.
pub(crate) fn add_args(ctx: &InstallContext) -> Vec<OsString> {
    let mut env_kv = OsString::from("VEX_ROOT=");
    env_kv.push(ctx.project_root.as_os_str());
    vec![
        "mcp".into(),
        "add".into(),
        "--scope".into(),
        SCOPE.into(),
        "--transport".into(),
        "stdio".into(),
        ctx.server_name.clone().into(),
        "--env".into(),
        env_kv,
        "--".into(),
        ctx.binary_path.clone().into_os_string(),
    ]
}

pub(crate) fn remove_args(server_name: &str) -> Vec<OsString> {
    vec![
        "mcp".into(),
        "remove".into(),
        "--scope".into(),
        SCOPE.into(),
        server_name.into(),
    ]
}

pub(crate) fn get_args(server_name: &str) -> Vec<OsString> {
    vec!["mcp".into(), "get".into(), server_name.into()]
}

/// Render `claude <args>` as a copy-pasteable line for the host shell.
pub(crate) fn display_command(args: &[OsString]) -> String {
    std::iter::once("claude".to_string())
        .chain(
            args.iter()
                .map(|a| shell_quote(&a.to_string_lossy()).into_owned()),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote `s` for the host shell unless it consists solely of characters
/// that never need quoting.
pub(crate) fn shell_quote(s: &str) -> Cow<'_, str> {
    if cfg!(windows) {
        quote_windows(s)
    } else {
        quote_posix(s)
    }
}

/// `true` when `s` is non-empty and every char is alphanumeric or in
/// `extra` / the always-safe punctuation set.
fn is_shell_safe(s: &str, extra: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./=:,+@".contains(c) || extra.contains(c))
}

/// POSIX: single quotes, with `'` spelled `'\''`.
pub(crate) fn quote_posix(s: &str) -> Cow<'_, str> {
    if is_shell_safe(s, "") {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(format!("'{}'", s.replace('\'', r"'\''")))
    }
}

/// Windows (cmd.exe / PowerShell): double quotes, with `"` escaped as
/// `\"` (the MSVCRT argv convention claude.exe parses). `\` is a normal
/// path character there, so it is treated as safe.
pub(crate) fn quote_windows(s: &str) -> Cow<'_, str> {
    if is_shell_safe(s, "\\") {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(format!("\"{}\"", s.replace('"', "\\\"")))
    }
}

/// The commands an install would run, in order, for display (dry-run and
/// the missing-`claude` hint). `remove` is listed only under `--force`;
/// when there is nothing to remove it is a harmless no-op.
fn planned_install_commands(ctx: &InstallContext) -> Vec<String> {
    let mut cmds = Vec::with_capacity(2);
    if ctx.force {
        cmds.push(display_command(&remove_args(&ctx.server_name)));
    }
    cmds.push(display_command(&add_args(ctx)));
    cmds
}

// ────────────────────────────────────────────────────────────────────
// Resolution + execution
// ────────────────────────────────────────────────────────────────────

/// Resolve the `claude` executable: `VEX_CLAUDE_BIN` when set (an empty
/// value is ignored), else a `PATH` search. `None` = not installed.
fn resolve_claude_bin() -> Option<PathBuf> {
    match std::env::var_os(CLAUDE_BIN_ENV) {
        Some(v) if !v.is_empty() => {
            let p = PathBuf::from(v);
            p.is_file().then_some(p)
        }
        _ => std::env::var_os("PATH").and_then(|path| find_on_path(&path)),
    }
}

/// Search a `PATH`-style value for the `claude` executable. On Windows
/// the native `claude.exe` is preferred over the npm `claude.cmd` shim:
/// Rust runs `.cmd`/`.bat` through cmd.exe, which cannot carry every
/// argument safely (see [`run`]).
pub(crate) fn find_on_path(path_var: &OsStr) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd", "claude.bat"]
    } else {
        &["claude"]
    };
    // Name-major order so a `claude.exe` anywhere on PATH beats a
    // `claude.cmd` earlier on PATH.
    names.iter().find_map(|n| {
        std::env::split_paths(path_var)
            .map(|dir| dir.join(n))
            .find(|candidate| candidate.is_file())
    })
}

/// Outcome of one `claude` invocation.
struct Ran {
    success: bool,
    /// stderr and stdout, trimmed and joined — claude is not consistent
    /// about which stream carries a refusal.
    output: String,
}

impl Ran {
    fn says_any(&self, needles: &[&str]) -> bool {
        let lower = self.output.to_lowercase();
        needles.iter().any(|n| lower.contains(n))
    }
}

/// Run `claude <args>`, capturing output. Spawn failures are errors; a
/// non-zero exit is returned for the caller to classify.
fn run(bin: &Path, args: &[OsString]) -> Result<Ran> {
    let out = match Command::new(bin).args(args).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            // Rust refuses to pass arguments cmd.exe cannot escape
            // (`%`, `"`, a trailing `\`, newlines) to a `.cmd`/`.bat`.
            bail!(
                "cannot pass these arguments safely to `{}` ({e}); install the native \
                 `claude.exe` (or point VEX_CLAUDE_BIN at it), or run this yourself:\n  {}",
                bin.display(),
                display_command(args)
            )
        }
        Err(e) => {
            return Err(e).with_context(|| format!("spawn `{}`", display_command(args)));
        }
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let output = [stderr.trim(), stdout.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Ran {
        success: out.status.success(),
        output,
    })
}

fn or_no_output(s: &str) -> &str {
    if s.is_empty() {
        "no output"
    } else {
        s
    }
}

/// Error for a non-zero exit that is not one of the recognised refusals.
fn failure(args: &[OsString], ran: &Ran) -> anyhow::Error {
    anyhow::anyhow!(
        "`{}` failed: {}",
        display_command(args),
        or_no_output(&ran.output)
    )
}

/// `claude mcp remove --scope user <name>`: `Ok(true)` removed,
/// `Ok(false)` nothing to remove, `Err` anything else.
fn remove_user_entry(bin: &Path, server_name: &str) -> Result<bool> {
    let args = remove_args(server_name);
    let ran = run(bin, &args)?;
    if ran.success {
        Ok(true)
    } else if ran.says_any(NOT_FOUND) {
        Ok(false)
    } else {
        Err(failure(&args, &ran))
    }
}

pub(crate) fn install_via(bin: Option<&Path>, ctx: &InstallContext) -> Result<InstallOutcome> {
    if ctx.dry_run {
        return Ok(InstallOutcome::WouldRun {
            commands: planned_install_commands(ctx),
        });
    }
    let Some(bin) = bin else {
        return Ok(InstallOutcome::NeedsAction {
            reason: MISSING_REASON.to_string(),
            commands: planned_install_commands(ctx),
        });
    };

    let mut ran_cmds = Vec::with_capacity(2);
    let removed = if ctx.force {
        let removed = remove_user_entry(bin, &ctx.server_name)?;
        if removed {
            ran_cmds.push(display_command(&remove_args(&ctx.server_name)));
        }
        removed
    } else {
        false
    };

    let add = add_args(ctx);
    let ran = run(bin, &add)?;
    if ran.success {
        ran_cmds.push(display_command(&add));
        return Ok(InstallOutcome::Registered { commands: ran_cmds });
    }
    if removed {
        // The old entry is gone and the new one did not land: say so
        // plainly and hand over the exact command to finish the job.
        bail!(
            "removed the previous user-scope `{name}` entry, but re-adding it failed: {err}\n\
             `{name}` is NOT registered now; retry with:\n  {cmd}",
            name = ctx.server_name,
            err = or_no_output(&ran.output),
            cmd = display_command(&add),
        );
    }
    if !ctx.force && ran.says_any(ALREADY_EXISTS) {
        return Ok(InstallOutcome::AlreadyRegistered {
            message: ran.output,
        });
    }
    Err(failure(&add, &ran))
}

pub(crate) fn uninstall_via(bin: Option<&Path>, server_name: &str) -> Result<UninstallOutcome> {
    let rm = remove_args(server_name);
    let Some(bin) = bin else {
        return Ok(UninstallOutcome::NeedsAction {
            reason: MISSING_REASON.to_string(),
            commands: vec![display_command(&rm)],
        });
    };
    if remove_user_entry(bin, server_name)? {
        Ok(UninstallOutcome::Unregistered {
            command: display_command(&rm),
        })
    } else {
        Ok(UninstallOutcome::NotRegistered {
            probe: display_command(&rm),
        })
    }
}

/// `claude mcp get <name>` searches every scope, so a hit is reported
/// with the scope claude names (its `Scope:` line), never assumed to be
/// the user-scope entry vex manages.
pub(crate) fn list_via(bin: Option<&Path>, server_name: &str) -> Result<Vec<String>> {
    let Some(bin) = bin else {
        return Err(ClaudeCliMissing.into());
    };
    let args = get_args(server_name);
    let ran = run(bin, &args)?;
    if !ran.success {
        return if ran.says_any(NOT_FOUND) {
            Ok(Vec::new())
        } else {
            Err(failure(&args, &ran))
        };
    }
    let scope = ran
        .output
        .lines()
        .find_map(|l| l.trim().strip_prefix("Scope:"))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Ok(vec![match scope {
        Some(scope) => format!("{server_name} — {scope}"),
        None => format!("{server_name} — scope not reported by `claude mcp get`"),
    }])
}

#[cfg(test)]
#[path = "mcp_claude_code_tests.rs"]
mod tests;
