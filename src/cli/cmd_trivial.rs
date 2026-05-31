//! Trivial standalone handlers: `vex completions`, `vex init`,
//! `vex capabilities`. Each is short enough that the import overhead of
//! a dedicated file would outweigh the value — collected here so
//! `cli/mod.rs` stays free of the boilerplate.

use anyhow::{bail, Context, Result};
use clap::CommandFactory;

use super::args::Cli;
use crate::util::config;

/// `vex completions <shell>` — emit a shell-completion script.
pub(crate) fn completions(shell: clap_complete::Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_owned();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
    Ok(())
}

/// `vex init` — drop a default `.vex.toml` into the current directory.
pub(crate) fn init() -> Result<()> {
    let path = std::env::current_dir()
        .context("get working directory")?
        .join(".vex.toml");
    if path.exists() {
        bail!(".vex.toml already exists at {}", path.display());
    }
    std::fs::write(&path, config::DEFAULT_CONFIG)
        .with_context(|| format!("write {}", path.display()))?;
    println!("Created {}", path.display());
    Ok(())
}

/// `vex capabilities` — pretty-print the v1 protocol envelope so MCP
/// clients (and humans doing capability negotiation by hand) can read
/// it directly. Keep the shape stable: a top-level `protocol_version`
/// and a `capabilities` block — see `src/protocol/mod.rs`.
pub(crate) fn capabilities() -> Result<()> {
    let body = serde_json::json!({
        "protocol_version": crate::protocol::PROTOCOL_VERSION,
        "capabilities": crate::protocol::capabilities::current(),
    });
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}
