//! `vex mcp install / uninstall / list` — CLI surface for the
//! [`crate::integrations::mcp`] module. Routes the user's `--agent
//! <id>` choice to the right [`McpAgentHandler`] and renders the
//! [`InstallOutcome`] / [`UninstallOutcome`] in plain text.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::integrations::mcp::{
    self, find_agent, known_agents, InstallContext, InstallOutcome, McpAgentHandler,
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

    let handlers = resolve_agents(agent)?;
    for h in &handlers {
        let outcome = h
            .install(&ctx)
            .with_context(|| format!("install vex-mcp into {}", h.display_name()))?;
        render_install(h.as_ref(), &outcome);
    }
    Ok(())
}

/// `vex mcp uninstall`.
pub(crate) fn uninstall(agent: &str, server_name: Option<String>) -> Result<()> {
    let server_name = server_name.unwrap_or_else(|| DEFAULT_SERVER_NAME.to_string());
    let handlers = resolve_agents(agent)?;
    for h in &handlers {
        let outcome = h
            .uninstall(&server_name)
            .with_context(|| format!("uninstall {} from {}", server_name, h.display_name()))?;
        render_uninstall(h.as_ref(), &outcome, &server_name);
    }
    Ok(())
}

/// `vex mcp list`. Without `--agent`, enumerates every known agent and
/// prints its server names; with `--agent`, narrows to one.
pub(crate) fn list(agent: Option<&str>) -> Result<()> {
    let handlers: Vec<Box<dyn McpAgentHandler>> = match agent {
        Some(id) => resolve_agents(id)?,
        None => known_agents(),
    };
    for h in &handlers {
        let path = h.config_path()?;
        let entries = h
            .list_servers()
            .with_context(|| format!("list servers from {}", h.display_name()))?;
        if entries.is_empty() {
            println!(
                "{}: (no MCP servers configured at {})",
                h.display_name(),
                path.display()
            );
        } else {
            println!("{} ({}):", h.display_name(), path.display());
            for name in entries {
                println!("  - {name}");
            }
        }
    }
    Ok(())
}

fn resolve_agents(agent: &str) -> Result<Vec<Box<dyn McpAgentHandler>>> {
    if agent == "all" {
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

fn render_install(handler: &dyn McpAgentHandler, outcome: &InstallOutcome) {
    match outcome {
        InstallOutcome::Installed { config_path } => {
            println!(
                "{}: installed `vex` MCP server in {}",
                handler.display_name(),
                config_path.display()
            );
        }
        InstallOutcome::AlreadyExists { config_path } => {
            println!(
                "{}: already configured at {} (use --force to overwrite)",
                handler.display_name(),
                config_path.display()
            );
        }
        InstallOutcome::WouldInstall {
            config_path,
            preview,
        } => {
            println!(
                "{}: --dry-run — would write {}:",
                handler.display_name(),
                config_path.display()
            );
            // Indent the preview so it's visually distinct in batch
            // output (`--agent all`).
            for line in preview.lines() {
                println!("  {line}");
            }
        }
    }
}

fn render_uninstall(handler: &dyn McpAgentHandler, outcome: &UninstallOutcome, server_name: &str) {
    match outcome {
        UninstallOutcome::Removed { config_path } => {
            println!(
                "{}: removed `{}` from {}",
                handler.display_name(),
                server_name,
                config_path.display()
            );
        }
        UninstallOutcome::NotFound { config_path } => {
            println!(
                "{}: no `{}` entry in {} (nothing to do)",
                handler.display_name(),
                server_name,
                config_path.display()
            );
        }
    }
}

// Re-export so the trait method dispatch in `install` / `uninstall`
// type-checks without leaking the import into every call site.
#[allow(unused_imports)]
use mcp as _mcp;

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
