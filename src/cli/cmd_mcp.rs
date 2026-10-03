//! `vex mcp install / uninstall / list` — CLI surface for the
//! [`crate::integrations::mcp`] module. Routes the user's `--agent
//! <id>` choice to the right [`McpAgentHandler`] and renders the
//! [`InstallOutcome`] / [`UninstallOutcome`] in plain text.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::integrations::mcp::{
    find_agent, known_agents, ClaudeCliMissing, InstallContext, InstallOutcome, McpAgentHandler,
    UninstallOutcome, DEFAULT_SERVER_NAME,
};

/// `vex mcp install`. `agent` is the `--agent <id>` value, with `"all"`
/// fanning out across [`known_agents`].
pub(crate) fn install(
    agent: &str,
    server_name: Option<String>,
    binary_path: Option<PathBuf>,
    project_root: Option<PathBuf>,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let server_name = server_name.unwrap_or_else(|| DEFAULT_SERVER_NAME.to_string());
    let binary_path = match binary_path {
        Some(p) => p,
        None => resolve_default_binary_path()?,
    };
    let project_root = match project_root {
        Some(p) => p,
        None => std::env::current_dir().context("get working directory")?,
    };

    let ctx = InstallContext {
        server_name,
        binary_path,
        project_root,
        dry_run,
        force,
    };

    let fan_out = agent == ALL;
    let mut tally = Tally::default();
    for h in &resolve_agents(agent)? {
        let result = h
            .install(&ctx)
            .with_context(|| format!("install vex-mcp into {}", h.display_name()));
        if let Some(outcome) = tally.settle(h.as_ref(), result, fan_out)? {
            if matches!(outcome, InstallOutcome::NeedsAction { .. }) {
                tally.needs_action.push(h.display_name());
            }
            render_install(h.as_ref(), &outcome, &ctx);
        }
    }
    tally.finish(fan_out)
}

/// `vex mcp uninstall`.
pub(crate) fn uninstall(agent: &str, server_name: Option<String>) -> Result<()> {
    let server_name = server_name.unwrap_or_else(|| DEFAULT_SERVER_NAME.to_string());
    let fan_out = agent == ALL;
    let mut tally = Tally::default();
    for h in &resolve_agents(agent)? {
        let result = h
            .uninstall(&server_name)
            .with_context(|| format!("uninstall {} from {}", server_name, h.display_name()));
        if let Some(outcome) = tally.settle(h.as_ref(), result, fan_out)? {
            if matches!(outcome, UninstallOutcome::NeedsAction { .. }) {
                tally.needs_action.push(h.display_name());
            }
            render_uninstall(h.as_ref(), &outcome, &server_name);
        }
    }
    tally.finish(fan_out)
}

/// `vex mcp list`. Without `--agent`, enumerates every known agent and
/// prints its server names; with `--agent`, narrows to one.
pub(crate) fn list(agent: Option<&str>) -> Result<()> {
    let (handlers, fan_out): (Vec<Box<dyn McpAgentHandler>>, bool) = match agent {
        Some(id) => (resolve_agents(id)?, id == ALL),
        None => (known_agents(), true),
    };
    let mut tally = Tally::default();
    for h in &handlers {
        let result = h
            .list_servers()
            .with_context(|| format!("list servers from {}", h.display_name()));
        let entries = match result {
            Err(e) if e.downcast_ref::<ClaudeCliMissing>().is_some() => {
                println!(
                    "{}: skipped — {}; run `claude mcp list` once it is installed",
                    h.display_name(),
                    ClaudeCliMissing
                );
                tally.needs_action.push(h.display_name());
                continue;
            }
            other => match tally.settle(h.as_ref(), other, fan_out)? {
                Some(entries) => entries,
                None => continue,
            },
        };
        let Some(source) = tally.settle(h.as_ref(), h.list_source(), fan_out)? else {
            continue;
        };
        if entries.is_empty() {
            println!("{}: no MCP servers configured ({source})", h.display_name());
        } else {
            println!("{} ({source}):", h.display_name());
            for name in entries {
                println!("  - {name}");
            }
        }
    }
    tally.finish(fan_out)
}

/// `--agent` value that fans out across every known agent.
const ALL: &str = "all";

/// Per-command bookkeeping so that under `--agent all` one agent's error
/// or missing CLI never stops the remaining agents.
///
/// Exit-code policy (the `mcp` contract is 0 = success / 2 = error; `1`
/// is reserved for empty query results, see docs/EXIT-CODES.md):
/// - any hard failure (I/O, parse, a failing `claude` call) → exit 2,
///   after every other agent has been processed;
/// - an agent whose CLI is missing (Claude Code without `claude`) → exit
///   2 when it was requested explicitly (`--agent claude-code`: the one
///   thing asked for was not done), but only a closing note under
///   `--agent all` — the file-based handlers also write configs for
///   agents that may not be installed, so a missing agent is a skip.
#[derive(Default)]
struct Tally {
    failed: Vec<&'static str>,
    needs_action: Vec<&'static str>,
}

impl Tally {
    /// Single agent: propagate the error unchanged (same message as
    /// before fan-out tolerance existed). Fan-out: report it on stderr,
    /// record the failure, and let the caller move on.
    fn settle<T>(
        &mut self,
        handler: &dyn McpAgentHandler,
        result: Result<T>,
        fan_out: bool,
    ) -> Result<Option<T>> {
        match result {
            Ok(v) => Ok(Some(v)),
            Err(e) if !fan_out => Err(e),
            Err(e) => {
                eprintln!("Error: {e:#}");
                self.failed.push(handler.display_name());
                Ok(None)
            }
        }
    }

    fn finish(self, fan_out: bool) -> Result<()> {
        if !self.failed.is_empty() {
            bail!(
                "{} agent(s) failed: {} (see errors above)",
                self.failed.len(),
                self.failed.join(", ")
            );
        }
        if self.needs_action.is_empty() {
            return Ok(());
        }
        let names = self.needs_action.join(", ");
        if fan_out {
            println!("note: skipped {names} — needs manual action (see above)");
            return Ok(());
        }
        bail!("{names}: nothing was changed — run the command printed above")
    }
}

fn resolve_agents(agent: &str) -> Result<Vec<Box<dyn McpAgentHandler>>> {
    if agent == ALL {
        return Ok(known_agents());
    }
    let handler = find_agent(agent).with_context(|| {
        let known: Vec<&str> = known_agents().iter().map(|h| h.id()).collect();
        format!(
            "unknown agent `{agent}` (known: {}; or `all` for every agent)",
            known.join(", ")
        )
    })?;
    Ok(vec![handler])
}

/// Locate the `vex-mcp` binary to register. Lookup order:
///   1. Sibling of the currently-running `vex` binary
///      (`std::env::current_exe()` → parent → `vex-mcp[.exe]`).
///   2. Bare `vex-mcp` — relying on the user's PATH.
///
/// Returns an absolute path when found via #1, a bare relative `vex-mcp`
/// otherwise (a deliberate hint to the user that PATH lookup is in
/// play, and the agent will fail to spawn the server if PATH is wrong).
fn resolve_default_binary_path() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sibling = parent.join(if cfg!(windows) {
                "vex-mcp.exe"
            } else {
                "vex-mcp"
            });
            if sibling.exists() {
                return Ok(sibling);
            }
        }
    }
    // Fallback: assume vex-mcp is on PATH. Agent spawn fails clearly
    // if not — better than refusing to install with a confusing error.
    Ok(PathBuf::from(if cfg!(windows) {
        "vex-mcp.exe"
    } else {
        "vex-mcp"
    }))
}

fn render_install(handler: &dyn McpAgentHandler, outcome: &InstallOutcome, ctx: &InstallContext) {
    let name = handler.display_name();
    let server = &ctx.server_name;
    match outcome {
        InstallOutcome::Installed { config_path } => {
            println!(
                "{name}: installed `{server}` MCP server in {}",
                config_path.display()
            );
        }
        InstallOutcome::AlreadyExists { config_path } => {
            println!(
                "{name}: already configured at {} (use --force to overwrite)",
                config_path.display()
            );
        }
        InstallOutcome::WouldInstall {
            config_path,
            preview,
        } => {
            println!("{name}: --dry-run — would write {}:", config_path.display());
            // Indent the preview so it's visually distinct in batch
            // output (`--agent all`).
            print_indented(preview.lines());
        }
        InstallOutcome::Registered { commands } => {
            println!("{name}: registered `{server}` MCP server (user scope) by running:");
            print_indented(commands);
        }
        InstallOutcome::AlreadyRegistered { message } => {
            println!(
                "{name}: `{server}` is already registered in user scope \
                 (use --force to replace it); claude said: {message}"
            );
        }
        InstallOutcome::WouldRun { commands } => {
            println!("{name}: --dry-run — would run:");
            print_indented(commands);
            if ctx.force {
                println!("  (the remove is a no-op if no user-scope `{server}` exists)");
            }
        }
        InstallOutcome::NeedsAction { reason, commands } => {
            println!("{name}: skipped — {reason}:");
            print_indented(commands);
        }
    }
}

fn render_uninstall(handler: &dyn McpAgentHandler, outcome: &UninstallOutcome, server_name: &str) {
    let name = handler.display_name();
    match outcome {
        UninstallOutcome::Removed { config_path } => {
            println!(
                "{name}: removed `{server_name}` from {}",
                config_path.display()
            );
        }
        UninstallOutcome::NotFound { config_path } => {
            println!(
                "{name}: no `{server_name}` entry in {} (nothing to do)",
                config_path.display()
            );
        }
        UninstallOutcome::Unregistered { command } => {
            println!("{name}: removed `{server_name}` by running:");
            print_indented([command]);
        }
        UninstallOutcome::NotRegistered { probe } => {
            println!(
                "{name}: no user-scope `{server_name}` entry (`{probe}` found none; nothing to do)"
            );
        }
        UninstallOutcome::NeedsAction { reason, commands } => {
            println!("{name}: skipped — {reason}:");
            print_indented(commands);
        }
    }
}

fn print_indented<I, S>(lines: I)
where
    I: IntoIterator<Item = S>,
    S: std::fmt::Display,
{
    for line in lines {
        println!("  {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_agents_unknown_id_lists_known_in_error() {
        let err = resolve_agents("definitely-not-real").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown agent"));
        // The error must surface the known list so users typing `--agent
        // codex-cli` (which won't exist until the next commit) get an
        // actionable hint without consulting docs.
        assert!(msg.contains("claude-code") || msg.contains("cursor"));
    }

    #[test]
    fn resolve_agents_all_expands_to_full_set() {
        let handlers = resolve_agents("all").unwrap();
        assert!(!handlers.is_empty());
        assert_eq!(handlers.len(), known_agents().len());
    }

    #[test]
    fn resolve_agents_named_returns_singleton() {
        let handlers = resolve_agents("claude-code").unwrap();
        assert_eq!(handlers.len(), 1);
        assert_eq!(handlers[0].id(), "claude-code");
    }
}
