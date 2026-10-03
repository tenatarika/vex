# Claude Code

Register `vex-mcp` with Claude Code through its own CLI — it stores
user-scope servers in `~/.claude.json` next to unrelated Claude Code
state, so don't hand-edit that file.

## Recommended: user scope (every project)

```bash
vex mcp install --agent claude-code
```

This runs, and prints, the equivalent of:

```bash
claude mcp add --scope user --transport stdio vex \
  --env VEX_ROOT=/path/to/your/project -- /path/to/vex-mcp
```

The `--` is required: everything after it is the server command. The
server name (`vex`) must come before `--env`. You can run the command
yourself instead; if `claude` is not on `PATH`, `vex mcp install` prints
it for you and writes nothing.

- Preview without running anything: `vex mcp install --agent claude-code --dry-run`
- Replace an existing entry: `vex mcp install --agent claude-code --force`
  (runs `claude mcp remove --scope user vex`, then `claude mcp add …`)
- Check: `claude mcp get vex` (or `vex mcp list --agent claude-code`)
- Remove: `vex mcp uninstall --agent claude-code`
  (runs `claude mcp remove --scope user vex`)

## Alternative: project scope (shared with your team)

Commit [`mcp.json`](mcp.json) to the repository root as **`.mcp.json`**,
with `command` set to your `vex-mcp` path, or generate it with:

```bash
claude mcp add --scope project --transport stdio vex \
  --env VEX_ROOT=/path/to/your/project -- /path/to/vex-mcp
```

Claude Code asks each user to approve project-scoped servers the first
time it sees them.

## Not `~/.claude/claude_desktop_config.json`

That file name is **Claude Desktop's** config. Claude Code never reads it.
vex releases before this fix wrote the Claude Code entry there; such an
entry has no effect in Claude Code. vex leaves it alone, because Claude
Desktop may use it.
