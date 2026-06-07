# vex MCP — Editor / Agent Integrations

Ready-to-paste MCP-server configs for every editor or coding agent that speaks the [Model Context Protocol](https://modelcontextprotocol.io/). All clients use the same `vex-mcp` binary (prebuilt since v1.11.2 — see the project [`README.md` Integration section](../README.md#claude-code-mcp-server) for download / build instructions and the full 23-tool catalog). Only the config file location and serialization format differ.

## Quick start

The fastest path (v1.15.0+) is `vex mcp install --agent <id>` — it reads your existing agent config, merges a `vex` server entry without disturbing siblings, and writes back atomically:

```bash
vex mcp install --agent cursor               # or: claude-code, codex-cli, windsurf, cline, continue, zed
vex mcp install --agent all                  # configure every supported agent
vex mcp install --agent cursor --dry-run     # preview without writing
vex mcp uninstall --agent cursor             # remove the entry
vex mcp list                                 # show current entries per agent
```

The snippets in this folder are still useful for **manual edits**, **agents the auto-installer doesn't know yet**, and **inspection** (the install command writes the same shapes you see here). To use them by hand:

1. Install the `vex-mcp` binary (download from a release, or `cargo build --release -p vex-mcp`).
2. Pick your agent's folder below.
3. Copy the contents into the listed config path.
4. Replace `/path/to/vex-mcp` with the absolute path to the binary (or just `vex-mcp` if it's on `PATH`).
5. Replace `/path/to/your/project` with the absolute path to the repo you want vex to index (or remove the `VEX_ROOT` env — vex falls back to the current working directory).
6. Restart the agent.

## Clients

| Agent              | Snippet                                                            | Target file on disk                                                  |
| ------------------ | ------------------------------------------------------------------ | -------------------------------------------------------------------- |
| Claude Code        | [`claude-code/claude_desktop_config.json`](claude-code/claude_desktop_config.json) | `~/.claude/claude_desktop_config.json`                               |
| Cursor             | [`cursor/mcp.json`](cursor/mcp.json)                               | `~/.cursor/mcp.json` *or* `<project>/.cursor/mcp.json`               |
| Codex CLI (OpenAI) | [`codex-cli/config.toml`](codex-cli/config.toml)                   | `~/.codex/config.toml` *or* `<project>/.codex/config.toml`           |
| Windsurf (Codeium) | [`windsurf/mcp_config.json`](windsurf/mcp_config.json)             | `~/.codeium/windsurf/mcp_config.json`                                |
| Cline (VS Code)    | [`cline/mcp.json`](cline/mcp.json)                                 | Cline panel → MCP Servers → Configure tab (or `~/.cline/mcp.json` for the CLI variant) |
| Continue.dev       | [`continue/vex.yaml`](continue/vex.yaml)                           | `<project>/.continue/mcpServers/vex.yaml`                            |
| Zed                | [`zed/settings.json`](zed/settings.json)                           | `~/.config/zed/settings.json` (merge into your existing settings)    |

## Per-agent caveats

- **Cursor** — project-scoped config (`<project>/.cursor/mcp.json`) wins over global (`~/.cursor/mcp.json`) when both are present. Restart Cursor after editing.
- **Codex CLI** — process env is inherited, so `VEX_ROOT` can also come from your shell. Bump `startup_timeout_sec` / `tool_timeout_sec` (commented in the snippet) if you index a very large repo on first call.
- **Cline** — `disabled` and `autoApprove` are Cline-specific. `autoApprove: ["search", "show"]` whitelists read-only tools to skip per-call confirmation; the default is prompt-every-time.
- **Continue.dev** — MCP servers are only active in **Agent** mode (not Chat / Edit / Autocomplete).
- **Zed** — MCP tools surface as **context servers** in the Agent Panel. Verify with the status indicator (green dot = active).

## See also

- [`README.md` → Integration](../README.md#integration) — full setup walkthrough including the prebuilt-binary download URLs and the Claude Code variant.
- [`docs/COOKBOOK.md`](../docs/COOKBOOK.md) — recipes for chaining vex's MCP tools end-to-end (refactor, PR-impact, code archaeology, dead-code cleanup, multi-repo orchestration).
- [`docs/MCP-SCHEMA.md`](../docs/MCP-SCHEMA.md) — canonical MCP parameter vocabulary (v1.7+).
- [`CONTRIBUTING.md` → Adding an MCP tool](../CONTRIBUTING.md#adding-an-mcp-tool) — for contributors extending the server.
