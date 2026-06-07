//! `vex mcp install` core — agent-handler trait + shared JSON/TOML
//! merge primitives + the seven concrete handlers vex ships (Claude
//! Code / Cursor / Codex CLI / Windsurf / Cline / Continue.dev / Zed).
//!
//! The configurator is **idempotent** and **format-respectful**: every
//! handler reads the agent's existing config (if any), merges a single
//! `vex` server entry without disturbing siblings, and writes back
//! atomically (`.tmp` + rename). When the entry already matches the
//! intended shape the handler returns [`InstallOutcome::AlreadyExists`]
//! instead of touching the file at all.
//!
//! Comments and exotic formatting in the user's existing config are
//! NOT preserved across the round-trip — `serde_json` / `toml::Value`
//! both canonicalise. Continue.dev avoids the issue by writing a
//! dedicated `.continue/mcpServers/vex.yaml` rather than merging.
//!
//! Future handlers slot in by implementing [`McpAgentHandler`] and
//! adding the constructor to [`known_agents`].

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Default name vex registers itself as in every agent's MCP server
/// table. Users override with `--server-name`; multiple `VEX_ROOT`s in
/// the same agent config need distinct names (`vex-api` / `vex-client`
/// is the documented multi-repo pattern — see
/// [`docs/COOKBOOK.md`](../../docs/COOKBOOK.md) Recipe 5).
pub const DEFAULT_SERVER_NAME: &str = "vex";

/// Inputs to [`McpAgentHandler::install`]. Owned so handlers can move
/// fields into the JSON/TOML they construct without surprise clones.
#[derive(Debug, Clone)]
pub struct InstallContext {
    /// Name to register the server under (`"vex"` by default).
    pub server_name: String,
    /// Absolute path to the `vex-mcp` binary. Falls back to bare
    /// `vex-mcp` (rely on PATH) if the resolver can't find a binary.
    pub binary_path: PathBuf,
    /// Project root passed to the server via `VEX_ROOT`. Almost
    /// always the current working dir at install time.
    pub project_root: PathBuf,
    /// Don't touch any files — only report what would happen.
    pub dry_run: bool,
    /// Overwrite an existing entry with the same name. Without this,
    /// install on an existing entry returns
    /// [`InstallOutcome::AlreadyExists`] without modifying the file.
    pub force: bool,
}

/// Result of an install attempt. The CLI surface prints one of these
/// per (agent, server) pair; the variants distinguish actual writes
/// from idempotent skips from dry-run previews so the caller can
/// summarise correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// File was created or modified to add the entry.
    Installed { config_path: PathBuf },
    /// Entry was already present and matched the intended shape.
    /// Idempotent — re-running the command is safe and a no-op.
    AlreadyExists { config_path: PathBuf },
    /// `dry_run=true` was set; this is what *would* have been written.
    /// `preview` is the post-merge serialized form so the user can
    /// diff against their existing config before committing.
    WouldInstall {
        config_path: PathBuf,
        preview: String,
    },
}

impl InstallOutcome {
    /// Path the outcome refers to, regardless of variant. Public helper
    /// for callers that want to summarise across a heterogeneous
    /// `Vec<InstallOutcome>` (`--agent all` output) without matching.
    #[allow(dead_code)]
    pub fn config_path(&self) -> &Path {
        match self {
            InstallOutcome::Installed { config_path }
            | InstallOutcome::AlreadyExists { config_path }
            | InstallOutcome::WouldInstall { config_path, .. } => config_path,
        }
    }
}

/// Result of an uninstall attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// Entry was found and removed.
    Removed { config_path: PathBuf },
    /// Entry was not present — uninstall is idempotent.
    NotFound { config_path: PathBuf },
}

/// Handler for one agent's MCP config. Implementations are stateless;
/// all configuration lives on [`InstallContext`]. `Debug` is required
/// so callers can `assert_eq!` / `dbg!` slices of handlers in tests
/// without case-by-case downcasting.
pub trait McpAgentHandler: Send + Sync + std::fmt::Debug {
    /// Stable identifier used on the CLI (`--agent <id>`). Kebab-case;
    /// must be unique across [`known_agents`].
    fn id(&self) -> &'static str;
    /// Human-readable name for status output.
    fn display_name(&self) -> &'static str;
    /// Resolved path to the config file this handler reads / writes.
    /// May not exist yet; install creates it.
    fn config_path(&self) -> Result<PathBuf>;
    /// Add a `vex` entry to the agent's MCP config, creating the file
    /// if absent.
    fn install(&self, ctx: &InstallContext) -> Result<InstallOutcome>;
    /// Remove the named entry. Returns `NotFound` if no such entry —
    /// the operation is idempotent.
    fn uninstall(&self, server_name: &str) -> Result<UninstallOutcome>;
    /// List server names currently registered in this agent's config.
    /// Returns an empty vec if the config file doesn't exist.
    fn list_servers(&self) -> Result<Vec<String>>;
}

/// Every handler vex ships with. Order is the documented order in
/// `integrations/README.md` so `vex mcp install --agent all` produces
/// deterministic output across runs.
pub fn known_agents() -> Vec<Box<dyn McpAgentHandler>> {
    vec![Box::new(ClaudeCodeHandler), Box::new(CursorHandler)]
}

/// Look up an agent by its `--agent <id>` value.
pub fn find_agent(id: &str) -> Option<Box<dyn McpAgentHandler>> {
    known_agents().into_iter().find(|h| h.id() == id)
}

// ────────────────────────────────────────────────────────────────────
// Path helpers
// ────────────────────────────────────────────────────────────────────

/// Resolve `$HOME` (Unix) / `%USERPROFILE%` (Windows). Mirrors the
/// helper inside `src/util/config.rs` rather than depending on the
/// `dirs` crate — vex's stance is "minimise dependencies, roll our own
/// for trivially small surface."
pub(crate) fn home_dir() -> Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(var)
        .map(PathBuf::from)
        .with_context(|| format!("environment variable {var} not set"))
}

/// Write `contents` to `dest` atomically: first to a sibling `.tmp`
/// file, fsync, then rename. Mirrors the convention in
/// `store::writer` and `embed::cache::EmbedCache::save` — a crash
/// mid-write can never leave a half-rendered config behind.
pub(crate) fn atomic_write(dest: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir for {}", dest.display()))?;
    }
    let tmp = dest.with_extension({
        let mut s = dest
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_string();
        if !s.is_empty() {
            s.push('.');
        }
        s.push_str("tmp");
        s
    });
    {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, dest)
        .with_context(|| format!("rename {} → {}", tmp.display(), dest.display()))?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// JSON merge primitives (shared across Claude Code / Cursor / Cline /
// Windsurf / Zed)
// ────────────────────────────────────────────────────────────────────

/// Per-agent quirks for the JSON merge path. Each JSON-format handler
/// supplies one of these and the shared [`install_json`] helper does
/// the read/merge/write.
pub(crate) struct JsonProfile {
    /// Top-level key under which the server entries live. Most agents
    /// use `mcpServers`; Zed uses `context_servers`.
    pub root_key: &'static str,
    /// Cursor requires `"type": "stdio"` on each entry; others infer.
    pub emit_type_stdio: bool,
    /// Cline tacks on `disabled: false` + `autoApprove: []` per entry.
    pub emit_cline_extras: bool,
}

pub(crate) fn install_json(
    profile: &JsonProfile,
    config_path: &Path,
    ctx: &InstallContext,
) -> Result<InstallOutcome> {
    let mut root = read_or_empty_json_object(config_path)?;

    // Locate / create the root key holding server entries.
    let servers = root
        .as_object_mut()
        .context("config root is not a JSON object")?
        .entry(profile.root_key.to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .with_context(|| format!("`{}` is not a JSON object", profile.root_key))?;

    let desired = build_json_entry(profile, ctx);

    if let Some(existing) = servers.get(&ctx.server_name) {
        if existing == &desired && !ctx.force {
            return Ok(InstallOutcome::AlreadyExists {
                config_path: config_path.to_path_buf(),
            });
        }
        // Differs (or --force) → overwrite below.
    }

    servers.insert(ctx.server_name.clone(), desired);

    let rendered = serde_json::to_string_pretty(&root)? + "\n";

    if ctx.dry_run {
        return Ok(InstallOutcome::WouldInstall {
            config_path: config_path.to_path_buf(),
            preview: rendered,
        });
    }

    atomic_write(config_path, &rendered)?;
    Ok(InstallOutcome::Installed {
        config_path: config_path.to_path_buf(),
    })
}

pub(crate) fn uninstall_json(
    profile: &JsonProfile,
    config_path: &Path,
    server_name: &str,
) -> Result<UninstallOutcome> {
    if !config_path.exists() {
        return Ok(UninstallOutcome::NotFound {
            config_path: config_path.to_path_buf(),
        });
    }
    let mut root = read_or_empty_json_object(config_path)?;
    let removed = root
        .as_object_mut()
        .and_then(|o| o.get_mut(profile.root_key))
        .and_then(|v| v.as_object_mut())
        .and_then(|o| o.remove(server_name))
        .is_some();
    if !removed {
        return Ok(UninstallOutcome::NotFound {
            config_path: config_path.to_path_buf(),
        });
    }
    let rendered = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write(config_path, &rendered)?;
    Ok(UninstallOutcome::Removed {
        config_path: config_path.to_path_buf(),
    })
}

pub(crate) fn list_json(profile: &JsonProfile, config_path: &Path) -> Result<Vec<String>> {
    if !config_path.exists() {
        return Ok(Vec::new());
    }
    let root = read_or_empty_json_object(config_path)?;
    Ok(root
        .get(profile.root_key)
        .and_then(|v| v.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default())
}

fn read_or_empty_json_object(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }
    let parsed: serde_json::Value = serde_json::from_str(trimmed)
        .with_context(|| format!("parse {} as JSON", path.display()))?;
    if !parsed.is_object() {
        bail!("{} must be a JSON object at the top level", path.display());
    }
    Ok(parsed)
}

fn build_json_entry(profile: &JsonProfile, ctx: &InstallContext) -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    if profile.emit_type_stdio {
        entry.insert("type".into(), serde_json::json!("stdio"));
    }
    entry.insert(
        "command".into(),
        serde_json::json!(ctx.binary_path.to_string_lossy()),
    );
    entry.insert(
        "env".into(),
        serde_json::json!({
            "VEX_ROOT": ctx.project_root.to_string_lossy(),
        }),
    );
    if profile.emit_cline_extras {
        entry.insert("disabled".into(), serde_json::json!(false));
        entry.insert("autoApprove".into(), serde_json::json!([]));
    }
    serde_json::Value::Object(entry)
}

// ────────────────────────────────────────────────────────────────────
// Concrete handlers
// ────────────────────────────────────────────────────────────────────

/// Claude Code — `~/.claude/claude_desktop_config.json`. No special
/// quirks; the canonical JSON profile.
#[derive(Debug)]
pub struct ClaudeCodeHandler;

const CLAUDE_CODE_PROFILE: JsonProfile = JsonProfile {
    root_key: "mcpServers",
    emit_type_stdio: false,
    emit_cline_extras: false,
};

impl McpAgentHandler for ClaudeCodeHandler {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
    fn config_path(&self) -> Result<PathBuf> {
        Ok(home_dir()?
            .join(".claude")
            .join("claude_desktop_config.json"))
    }
    fn install(&self, ctx: &InstallContext) -> Result<InstallOutcome> {
        install_json(&CLAUDE_CODE_PROFILE, &self.config_path()?, ctx)
    }
    fn uninstall(&self, server_name: &str) -> Result<UninstallOutcome> {
        uninstall_json(&CLAUDE_CODE_PROFILE, &self.config_path()?, server_name)
    }
    fn list_servers(&self) -> Result<Vec<String>> {
        list_json(&CLAUDE_CODE_PROFILE, &self.config_path()?)
    }
}

/// Cursor — `~/.cursor/mcp.json`. Requires `"type": "stdio"` per
/// entry; otherwise same JSON shape.
#[derive(Debug)]
pub struct CursorHandler;

const CURSOR_PROFILE: JsonProfile = JsonProfile {
    root_key: "mcpServers",
    emit_type_stdio: true,
    emit_cline_extras: false,
};

impl McpAgentHandler for CursorHandler {
    fn id(&self) -> &'static str {
        "cursor"
    }
    fn display_name(&self) -> &'static str {
        "Cursor"
    }
    fn config_path(&self) -> Result<PathBuf> {
        Ok(home_dir()?.join(".cursor").join("mcp.json"))
    }
    fn install(&self, ctx: &InstallContext) -> Result<InstallOutcome> {
        install_json(&CURSOR_PROFILE, &self.config_path()?, ctx)
    }
    fn uninstall(&self, server_name: &str) -> Result<UninstallOutcome> {
        uninstall_json(&CURSOR_PROFILE, &self.config_path()?, server_name)
    }
    fn list_servers(&self) -> Result<Vec<String>> {
        list_json(&CURSOR_PROFILE, &self.config_path()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_ctx(server_name: &str, root: &Path) -> InstallContext {
        InstallContext {
            server_name: server_name.into(),
            binary_path: PathBuf::from("/path/to/vex-mcp"),
            project_root: root.to_path_buf(),
            dry_run: false,
            force: false,
        }
    }

    #[test]
    fn install_json_creates_file_when_absent() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("mcp.json");
        let ctx = make_ctx("vex", tmp.path());

        let out = install_json(&CLAUDE_CODE_PROFILE, &cfg, &ctx).unwrap();
        assert!(matches!(out, InstallOutcome::Installed { .. }));
        assert!(cfg.exists());

        let body = std::fs::read_to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["mcpServers"]["vex"]["command"],
            serde_json::json!("/path/to/vex-mcp")
        );
        assert_eq!(
            v["mcpServers"]["vex"]["env"]["VEX_ROOT"],
            serde_json::json!(tmp.path().to_string_lossy())
        );
    }

    #[test]
    fn install_json_preserves_existing_unrelated_servers() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("mcp.json");
        let initial = serde_json::json!({
            "mcpServers": {
                "other-server": {
                    "command": "/some/other/binary",
                    "env": { "FOO": "bar" }
                }
            },
            "unrelated_top_level": "must-survive"
        });
        std::fs::write(&cfg, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

        let ctx = make_ctx("vex", tmp.path());
        install_json(&CLAUDE_CODE_PROFILE, &cfg, &ctx).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        // Unrelated entries must survive the merge — install must be
        // surgical, not a wholesale rewrite.
        assert_eq!(
            v["mcpServers"]["other-server"]["command"],
            serde_json::json!("/some/other/binary")
        );
        assert_eq!(v["unrelated_top_level"], serde_json::json!("must-survive"));
        assert_eq!(
            v["mcpServers"]["vex"]["command"],
            serde_json::json!("/path/to/vex-mcp")
        );
    }

    #[test]
    fn install_json_is_idempotent_when_entry_matches() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("mcp.json");
        let ctx = make_ctx("vex", tmp.path());

        let first = install_json(&CLAUDE_CODE_PROFILE, &cfg, &ctx).unwrap();
        assert!(matches!(first, InstallOutcome::Installed { .. }));

        let second = install_json(&CLAUDE_CODE_PROFILE, &cfg, &ctx).unwrap();
        assert!(
            matches!(second, InstallOutcome::AlreadyExists { .. }),
            "second install must be a no-op skip, got {second:?}"
        );
    }

    #[test]
    fn install_json_overwrites_when_force_is_set() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("mcp.json");
        let mut ctx = make_ctx("vex", tmp.path());

        install_json(&CLAUDE_CODE_PROFILE, &cfg, &ctx).unwrap();

        // Change the binary path; with force=true the entry must be
        // updated even though `server_name` is the same.
        ctx.binary_path = PathBuf::from("/new/vex-mcp");
        ctx.force = true;

        let out = install_json(&CLAUDE_CODE_PROFILE, &cfg, &ctx).unwrap();
        assert!(matches!(out, InstallOutcome::Installed { .. }));

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["vex"]["command"],
            serde_json::json!("/new/vex-mcp")
        );
    }

    #[test]
    fn install_json_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("mcp.json");
        let mut ctx = make_ctx("vex", tmp.path());
        ctx.dry_run = true;

        let out = install_json(&CLAUDE_CODE_PROFILE, &cfg, &ctx).unwrap();
        match out {
            InstallOutcome::WouldInstall { preview, .. } => {
                assert!(preview.contains("\"vex\""));
                assert!(preview.contains("/path/to/vex-mcp"));
            }
            other => panic!("expected WouldInstall, got {other:?}"),
        }
        assert!(!cfg.exists(), "dry_run must not create the file");
    }

    #[test]
    fn cursor_profile_emits_type_stdio() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("mcp.json");
        let ctx = make_ctx("vex", tmp.path());

        install_json(&CURSOR_PROFILE, &cfg, &ctx).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["vex"]["type"],
            serde_json::json!("stdio"),
            "Cursor profile must emit `type: stdio` — it is the only client that requires it"
        );
    }

    #[test]
    fn uninstall_json_removes_only_the_named_entry() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("mcp.json");
        let initial = serde_json::json!({
            "mcpServers": {
                "vex": {"command": "/vex-mcp", "env": {}},
                "other": {"command": "/other", "env": {}}
            }
        });
        std::fs::write(&cfg, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

        let out = uninstall_json(&CLAUDE_CODE_PROFILE, &cfg, "vex").unwrap();
        assert!(matches!(out, UninstallOutcome::Removed { .. }));

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v["mcpServers"]["vex"].is_null());
        assert_eq!(
            v["mcpServers"]["other"]["command"],
            serde_json::json!("/other"),
            "uninstall must surgically remove only the named entry"
        );
    }

    #[test]
    fn uninstall_json_on_missing_file_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("never-existed.json");
        let out = uninstall_json(&CLAUDE_CODE_PROFILE, &cfg, "vex").unwrap();
        assert!(matches!(out, UninstallOutcome::NotFound { .. }));
        assert!(!cfg.exists(), "uninstall must not create the file");
    }

    #[test]
    fn list_json_returns_empty_when_file_absent() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("never-existed.json");
        assert!(list_json(&CLAUDE_CODE_PROFILE, &cfg).unwrap().is_empty());
    }

    #[test]
    fn known_agents_have_unique_ids() {
        let agents = known_agents();
        let ids: Vec<&'static str> = agents.iter().map(|a| a.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ids.len(),
            "every handler must have a unique --agent <id>"
        );
    }
}
