//! Trivial standalone handlers: `vex completions`, `vex init`,
//! `vex capabilities`. Each is short enough that the import overhead of
//! a dedicated file would outweigh the value — collected here so
//! `cli/mod.rs` stays free of the boilerplate.

use anyhow::{bail, Context, Result};
use clap::CommandFactory;

use super::args::Cli;
use crate::integrations::agents_md;
use crate::util::config;

/// `vex completions <shell>` — emit a shell-completion script.
pub(crate) fn completions(shell: clap_complete::Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_owned();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
    Ok(())
}

/// `vex init` — drop a default `.vex.toml` into the current directory.
///
/// `agents_md` adds a generic AGENTS.md (community-convention agent
/// instruction file) next to the config. `agents_md_only` skips the
/// `.vex.toml` write — useful when the project already has one but
/// hasn't yet adopted the AGENTS.md convention. The flag combination
/// is enforced at the clap layer (`requires = "agents_md"`).
pub(crate) fn init(agents_md: bool, agents_md_only: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("get working directory")?;

    if !agents_md_only {
        let path = cwd.join(".vex.toml");
        if path.exists() {
            bail!(".vex.toml already exists at {}", path.display());
        }
        std::fs::write(&path, config::DEFAULT_CONFIG)
            .with_context(|| format!("write {}", path.display()))?;
        println!("Created {}", path.display());
    }

    if agents_md {
        let path = agents_md::write_template(&cwd)?;
        println!("Created {}", path.display());
    }

    Ok(())
}

/// `vex capabilities` — emit the v1 ResponseEnvelope so MCP clients (and
/// humans doing capability negotiation by hand) get the same shape every
/// other vex JSON command emits: `protocol_version`, `capabilities`,
/// `_meta`, and `results`. See `src/protocol/mod.rs`.
///
/// The matrix lives at `capabilities`; `results` is `null` because this
/// call is argument-free and has no per-query payload. Echoing the
/// matrix into `results` would be semantically misleading — agents would
/// see the capability block twice under two different keys.
///
/// Pre-fix this emitted only the top-level `protocol_version` +
/// `capabilities` pair. The MCP wrapper at
/// `crates/vex-mcp/src/main.rs` recognised the envelope but produced
/// `structuredContent: {}` because `envelope_results` was absent — what
/// the field-test report observed as "`capabilities` returns `{}`".
pub(crate) fn capabilities() -> Result<()> {
    // The envelope is built explicitly (rather than going through
    // `print_envelope`) so the `_meta` block stays empty (no project
    // root / manifest is available here) and the `T` payload type can
    // be `serde_json::Value` for an explicit `null`.
    let envelope: crate::protocol::ResponseEnvelope<serde_json::Value> =
        crate::protocol::ResponseEnvelope {
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            capabilities: crate::protocol::capabilities::current(),
            meta: crate::protocol::MetaEnvelope::default(),
            results: serde_json::Value::Null,
        };
    println!("{}", serde_json::to_string_pretty(&envelope)?);
    Ok(())
}
