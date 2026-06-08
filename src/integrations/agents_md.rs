//! AGENTS.md template — the community-convention instruction file that
//! Cursor / Codex CLI / Aider / Cline / Windsurf and most non-Claude
//! agents read as a fallback to their own per-tool config files.
//!
//! Emitted by `vex init --agents-md`. The template is intentionally
//! short — only the load-bearing "use vex for code lookup" rules and a
//! pointer to vex's documentation. Agents that need richer per-project
//! conventions append to the file; vex never overwrites an existing
//! AGENTS.md (matches the same refuse-if-exists behaviour as `.vex.toml`).
//!
//! Mirrors [`.claude/skills/vex/SKILL.md`](../../.claude/skills/vex/SKILL.md)
//! in spirit (six load-bearing rules + lazy-load pointer) but is
//! self-contained — it does not require the Claude Code skill loader.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// Project-root AGENTS.md template. Kept generic — references
/// `integrations/` and `vex mcp install` for MCP setup but otherwise
/// makes no assumption about the host project's language or layout.
pub const DEFAULT_AGENTS_MD: &str = r#"# AGENTS.md

Conventions for AI coding agents (Cursor / Codex CLI / Claude Code / Cline / Windsurf / Continue.dev / Zed / Aider / ...) working in this repository.

## Code Search — use `vex`

This repository is indexed by [vex](https://github.com/tenatarika/vex). Prefer `vex` over `grep` / `Read` for code lookup — it is faster (~4ms typical), scope-aware, and avoids loading whole files into context.

| Task                                | Reach for                          |
|-------------------------------------|------------------------------------|
| Locate a specific symbol by name    | `vex check <Symbol>`               |
| Extract a specific function/class   | `vex show <Symbol>`                |
| Find all references                 | `vex usages <Symbol> --strict`     |
| Who calls / who do I call           | `vex callers <Name>` / `callees`   |
| Fuzzy / multi-word keyword search   | `vex search "<phrase>"`            |
| Regex content search                | `vex grep <pattern>`               |
| AST pattern match                   | `vex pattern '<pat>' --lang <X>`   |
| Symbol-level diff vs base           | `vex diff --base origin/main`      |
| Near-duplicate / similar symbols    | `vex similar <Symbol>`             |

**`vex check` vs `vex search`** is the most common mistake. `vex check <Symbol>` is the exact-name probe — fastest, returns a clean hit/miss with `path:line` and bypasses the ranker. `vex search <query>` is a ranked blend (FST + BM25 + semantic) that returns NEIGHBORS (callers / imports) when no symbol literally matches the query — great for "find me something about retries", wrong for "does `Foo` exist". v1.15.0 prints a stderr hint when an identifier-shaped `vex search` returns 0 FST hits.

**`--strict`** is the load-bearing flag for refactor work on `vex usages` — it reads the persistent scope-binder reference edges and drops string-literal / comment / wrong-scope false hits. Cross-file imports resolved for Rust, TypeScript, Python, C#, C++ (other languages fall back to text-scan and signal it in the response).

## Re-indexing

- `vex update` — incremental, run after edits
- `vex index` — full rebuild, run after a `vex` binary upgrade that bumps the index format
- `vex status` — health check (symbol count, embeddings, body-tokens marker, etc.)

## MCP-capable agents

For Claude Code / Cursor / Codex CLI / Windsurf / Cline / Continue.dev / Zed, the same surface is available via the `vex-mcp` server. One-line setup:

```bash
vex mcp install --agent cursor       # or any of: claude-code, codex-cli, windsurf, cline, continue, zed
vex mcp install --agent all          # configure every supported agent
```

`vex mcp install` is idempotent — re-running is a no-op skip. Use `--dry-run` to preview, `--force` to overwrite an existing entry. Manual config-file snippets live in [`integrations/`](https://github.com/tenatarika/vex/tree/main/integrations) for agents the auto-installer doesn't yet know.

## Further reading

- [vex README](https://github.com/tenatarika/vex#readme) — full setup + command catalog
- [Agent cookbook](https://github.com/tenatarika/vex/blob/main/docs/COOKBOOK.md) — MCP-tool composition recipes (refactor, PR-impact, code archaeology)
- [Known limitations](https://github.com/tenatarika/vex/blob/main/docs/LIMITATIONS.md) — what `vex usages --strict` can't see (dynamic dispatch, reflection, wildcard imports)
"#;

/// Write [`DEFAULT_AGENTS_MD`] to `<dir>/AGENTS.md`, refusing to
/// overwrite an existing file (same behaviour as `.vex.toml` creation
/// in [`crate::cli::cmd_trivial::init`]).
///
/// Returns the path that was written.
pub fn write_template(dir: &Path) -> Result<std::path::PathBuf> {
    let path = dir.join("AGENTS.md");
    if path.exists() {
        bail!("AGENTS.md already exists at {}", path.display());
    }
    std::fs::write(&path, DEFAULT_AGENTS_MD)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn template_creates_file_with_expected_marker() {
        let tmp = TempDir::new().unwrap();
        let path = write_template(tmp.path()).unwrap();
        assert!(path.exists(), "AGENTS.md must exist after write_template");
        let contents = std::fs::read_to_string(&path).unwrap();
        // First-line marker — used by downstream linters / agent loaders
        // to detect the canonical template vs a hand-edited file. If
        // this changes, downstream tooling needs a heads-up.
        assert!(
            contents.starts_with("# AGENTS.md\n"),
            "template must start with `# AGENTS.md`"
        );
    }

    #[test]
    fn template_mentions_vex_for_agent_discovery() {
        // Tightest possible guard against accidentally publishing a
        // template that doesn't actually advertise vex. If somebody
        // refactors the template and drops the "use vex" framing, an
        // AGENTS.md sitting in a downstream repo silently stops
        // pointing agents at the tool.
        let contents = DEFAULT_AGENTS_MD;
        assert!(contents.contains("vex"), "template must mention vex");
        assert!(
            contents.contains("--strict"),
            "template must mention the load-bearing --strict flag"
        );
        assert!(
            contents.contains("vex show"),
            "template must mention `vex show` as the Read-replacement"
        );
        // v1.15.1: pin the `vex check` recommendation. Pre-fix the
        // template recommended `vex search <Symbol>` for exact-name
        // lookup, which surfaces ranked NEIGHBORS instead of the
        // symbol itself when no local definition exists. Future
        // template refactors must keep the `check` first-class.
        assert!(
            contents.contains("vex check"),
            "template must recommend `vex check` for exact-name lookup"
        );
    }

    #[test]
    fn refuses_to_overwrite_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("AGENTS.md");
        std::fs::write(&path, "user-customised content").unwrap();

        let err = write_template(tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("AGENTS.md already exists"),
            "error must mention the conflict explicitly: got {msg:?}"
        );

        // User content must be preserved on the refuse path.
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "user-customised content");
    }

    #[test]
    fn write_returns_the_path_it_wrote() {
        let tmp = TempDir::new().unwrap();
        let path = write_template(tmp.path()).unwrap();
        assert_eq!(path, tmp.path().join("AGENTS.md"));
    }
}
