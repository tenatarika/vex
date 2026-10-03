//! Editor / agent integration support (v1.15.0+).
//!
//! This module groups the moving parts of "make `vex` first-class in
//! any coding agent". Today it ships:
//!
//! - [`agents_md`] — the AGENTS.md template emitted by `vex init
//!   --agents-md`, a community-convention instruction file readable
//!   by Cursor / Codex CLI / Aider / Cline / Windsurf and friends as
//!   a fallback to their own per-tool config formats.
//!
//! Future additions land here too:
//! - `mcp/` (v1.15.0): `vex mcp install --agent <X>` auto-configurator
//!   for the seven MCP clients vex ships ready-to-paste snippets for
//!   under [`integrations/`](../../integrations/). Claude Code is the
//!   one handler that delegates to the agent's own CLI
//!   (`claude mcp add`) instead of writing a file — see
//!   [`mcp_claude_code`].

pub mod agents_md;
pub mod mcp;
pub mod mcp_claude_code;
