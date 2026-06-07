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
    vec![
        Box::new(ClaudeCodeHandler),
        Box::new(CursorHandler),
        Box::new(CodexCliHandler),
        Box::new(WindsurfHandler),
        Box::new(ClineHandler),
        Box::new(ContinueDevHandler),
        Box::new(ZedHandler),
    ]
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
// TOML merge primitives (Codex CLI)
// ────────────────────────────────────────────────────────────────────

/// Build the `[mcp_servers.<name>]` table value: `command` + `env`
/// (matching the documented Codex CLI MCP schema). Other optional
/// fields (timeouts, enabled_tools) are left to the user — vex
/// install seeds only the minimum.
fn build_toml_entry(ctx: &InstallContext) -> toml::Value {
    let mut entry = toml::map::Map::new();
    entry.insert(
        "command".to_string(),
        toml::Value::String(ctx.binary_path.to_string_lossy().to_string()),
    );
    let mut env = toml::map::Map::new();
    env.insert(
        "VEX_ROOT".to_string(),
        toml::Value::String(ctx.project_root.to_string_lossy().to_string()),
    );
    entry.insert("env".to_string(), toml::Value::Table(env));
    toml::Value::Table(entry)
}

pub(crate) fn install_toml(config_path: &Path, ctx: &InstallContext) -> Result<InstallOutcome> {
    let mut doc = read_or_empty_toml_table(config_path)?;
    let mcp_servers = doc
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("`mcp_servers` is not a TOML table")?;

    let desired = build_toml_entry(ctx);

    if let Some(existing) = mcp_servers.get(&ctx.server_name) {
        if existing == &desired && !ctx.force {
            return Ok(InstallOutcome::AlreadyExists {
                config_path: config_path.to_path_buf(),
            });
        }
    }

    mcp_servers.insert(ctx.server_name.clone(), desired);

    let rendered = toml::to_string_pretty(&toml::Value::Table(doc))
        .context("serialize Codex config to TOML")?;

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

pub(crate) fn uninstall_toml(config_path: &Path, server_name: &str) -> Result<UninstallOutcome> {
    if !config_path.exists() {
        return Ok(UninstallOutcome::NotFound {
            config_path: config_path.to_path_buf(),
        });
    }
    let mut doc = read_or_empty_toml_table(config_path)?;
    let removed = doc
        .get_mut("mcp_servers")
        .and_then(|v| v.as_table_mut())
        .and_then(|t| t.remove(server_name))
        .is_some();
    if !removed {
        return Ok(UninstallOutcome::NotFound {
            config_path: config_path.to_path_buf(),
        });
    }
    let rendered = toml::to_string_pretty(&toml::Value::Table(doc))?;
    atomic_write(config_path, &rendered)?;
    Ok(UninstallOutcome::Removed {
        config_path: config_path.to_path_buf(),
    })
}

pub(crate) fn list_toml(config_path: &Path) -> Result<Vec<String>> {
    if !config_path.exists() {
        return Ok(Vec::new());
    }
    let doc = read_or_empty_toml_table(config_path)?;
    Ok(doc
        .get("mcp_servers")
        .and_then(|v| v.as_table())
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default())
}

fn read_or_empty_toml_table(path: &Path) -> Result<toml::map::Map<String, toml::Value>> {
    if !path.exists() {
        return Ok(toml::map::Map::new());
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(toml::map::Map::new());
    }
    let value: toml::Value =
        toml::from_str(trimmed).with_context(|| format!("parse {} as TOML", path.display()))?;
    match value {
        toml::Value::Table(t) => Ok(t),
        _ => bail!("{} must be a TOML table at the top level", path.display()),
    }
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

/// Windsurf (Codeium) — `~/.codeium/windsurf/mcp_config.json`. Same
/// canonical JSON profile as Claude Code; only the config path differs.
#[derive(Debug)]
pub struct WindsurfHandler;

const WINDSURF_PROFILE: JsonProfile = JsonProfile {
    root_key: "mcpServers",
    emit_type_stdio: false,
    emit_cline_extras: false,
};

impl McpAgentHandler for WindsurfHandler {
    fn id(&self) -> &'static str {
        "windsurf"
    }
    fn display_name(&self) -> &'static str {
        "Windsurf"
    }
    fn config_path(&self) -> Result<PathBuf> {
        Ok(home_dir()?
            .join(".codeium")
            .join("windsurf")
            .join("mcp_config.json"))
    }
    fn install(&self, ctx: &InstallContext) -> Result<InstallOutcome> {
        install_json(&WINDSURF_PROFILE, &self.config_path()?, ctx)
    }
    fn uninstall(&self, server_name: &str) -> Result<UninstallOutcome> {
        uninstall_json(&WINDSURF_PROFILE, &self.config_path()?, server_name)
    }
    fn list_servers(&self) -> Result<Vec<String>> {
        list_json(&WINDSURF_PROFILE, &self.config_path()?)
    }
}

/// Cline — `~/.cline/mcp.json` (the standalone CLI variant; VS Code
/// extension users normally edit via the panel UI). Same `mcpServers`
/// root as the other JSON agents but tacks on `disabled: false` +
/// `autoApprove: []` per entry.
#[derive(Debug)]
pub struct ClineHandler;

const CLINE_PROFILE: JsonProfile = JsonProfile {
    root_key: "mcpServers",
    emit_type_stdio: false,
    emit_cline_extras: true,
};

impl McpAgentHandler for ClineHandler {
    fn id(&self) -> &'static str {
        "cline"
    }
    fn display_name(&self) -> &'static str {
        "Cline"
    }
    fn config_path(&self) -> Result<PathBuf> {
        Ok(home_dir()?.join(".cline").join("mcp.json"))
    }
    fn install(&self, ctx: &InstallContext) -> Result<InstallOutcome> {
        install_json(&CLINE_PROFILE, &self.config_path()?, ctx)
    }
    fn uninstall(&self, server_name: &str) -> Result<UninstallOutcome> {
        uninstall_json(&CLINE_PROFILE, &self.config_path()?, server_name)
    }
    fn list_servers(&self) -> Result<Vec<String>> {
        list_json(&CLINE_PROFILE, &self.config_path()?)
    }
}

/// Codex CLI (OpenAI) — `~/.codex/config.toml`. TOML format
/// (`[mcp_servers.<name>]` table). Uses the dedicated install_toml /
/// uninstall_toml / list_toml primitives instead of the JSON path.
#[derive(Debug)]
pub struct CodexCliHandler;

impl McpAgentHandler for CodexCliHandler {
    fn id(&self) -> &'static str {
        "codex-cli"
    }
    fn display_name(&self) -> &'static str {
        "Codex CLI"
    }
    fn config_path(&self) -> Result<PathBuf> {
        Ok(home_dir()?.join(".codex").join("config.toml"))
    }
    fn install(&self, ctx: &InstallContext) -> Result<InstallOutcome> {
        install_toml(&self.config_path()?, ctx)
    }
    fn uninstall(&self, server_name: &str) -> Result<UninstallOutcome> {
        uninstall_toml(&self.config_path()?, server_name)
    }
    fn list_servers(&self) -> Result<Vec<String>> {
        list_toml(&self.config_path()?)
    }
}

/// Continue.dev — drops a per-server YAML file at
/// `<project>/.continue/mcpServers/<server_name>.yaml` rather than
/// merging into a shared file. This matches Continue's documented
/// "one server per file in the mcpServers/ directory" convention and
/// neatly side-steps needing a YAML library — the file is small
/// enough to render from a format string.
///
/// `config_path()` returns the *directory*, not a file — install
/// resolves the per-server filename internally. Uninstall + list
/// operate on the same directory.
#[derive(Debug)]
pub struct ContinueDevHandler;

impl ContinueDevHandler {
    /// Project-scoped — Continue looks for the directory relative to
    /// the workspace root, which at `vex mcp install` time is the
    /// current working directory.
    fn dir(&self) -> Result<PathBuf> {
        Ok(std::env::current_dir()
            .context("get working directory")?
            .join(".continue")
            .join("mcpServers"))
    }
}

impl McpAgentHandler for ContinueDevHandler {
    fn id(&self) -> &'static str {
        "continue"
    }
    fn display_name(&self) -> &'static str {
        "Continue.dev"
    }
    fn config_path(&self) -> Result<PathBuf> {
        self.dir()
    }
    fn install(&self, ctx: &InstallContext) -> Result<InstallOutcome> {
        let file = self.dir()?.join(format!("{}.yaml", ctx.server_name));
        let yaml = render_continue_yaml(ctx);

        if file.exists() && !ctx.force {
            let existing = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            if existing == yaml {
                return Ok(InstallOutcome::AlreadyExists { config_path: file });
            }
            // Differs but no --force — surface AlreadyExists so the
            // user gets the "use --force" hint instead of a surprise
            // overwrite of a hand-edited file.
            return Ok(InstallOutcome::AlreadyExists { config_path: file });
        }

        if ctx.dry_run {
            return Ok(InstallOutcome::WouldInstall {
                config_path: file,
                preview: yaml,
            });
        }
        atomic_write(&file, &yaml)?;
        Ok(InstallOutcome::Installed { config_path: file })
    }
    fn uninstall(&self, server_name: &str) -> Result<UninstallOutcome> {
        let file = self.dir()?.join(format!("{server_name}.yaml"));
        if !file.exists() {
            return Ok(UninstallOutcome::NotFound { config_path: file });
        }
        std::fs::remove_file(&file).with_context(|| format!("remove {}", file.display()))?;
        Ok(UninstallOutcome::Removed { config_path: file })
    }
    fn list_servers(&self) -> Result<Vec<String>> {
        let dir = self.dir()?;
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .with_context(|| format!("read dir {}", dir.display()))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("yaml"))
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(String::from)
            })
            .collect();
        names.sort();
        Ok(names)
    }
}

/// Render the Continue per-server YAML body. Hand-formatted (no YAML
/// dep) — the file is small enough that string formatting is faster,
/// smaller, and easier to audit than a serde_yaml round-trip.
fn render_continue_yaml(ctx: &InstallContext) -> String {
    format!(
        "mcpServers:\n  - name: {}\n    type: stdio\n    command: {}\n    env:\n      VEX_ROOT: {}\n",
        ctx.server_name,
        ctx.binary_path.to_string_lossy(),
        ctx.project_root.to_string_lossy(),
    )
}

/// Zed — `~/.config/zed/settings.json`. Differs from the others in
/// the root key (`context_servers` not `mcpServers`); otherwise the
/// JSON shape is the same.
#[derive(Debug)]
pub struct ZedHandler;

const ZED_PROFILE: JsonProfile = JsonProfile {
    root_key: "context_servers",
    emit_type_stdio: false,
    emit_cline_extras: false,
};

impl McpAgentHandler for ZedHandler {
    fn id(&self) -> &'static str {
        "zed"
    }
    fn display_name(&self) -> &'static str {
        "Zed"
    }
    fn config_path(&self) -> Result<PathBuf> {
        Ok(home_dir()?
            .join(".config")
            .join("zed")
            .join("settings.json"))
    }
    fn install(&self, ctx: &InstallContext) -> Result<InstallOutcome> {
        install_json(&ZED_PROFILE, &self.config_path()?, ctx)
    }
    fn uninstall(&self, server_name: &str) -> Result<UninstallOutcome> {
        uninstall_json(&ZED_PROFILE, &self.config_path()?, server_name)
    }
    fn list_servers(&self) -> Result<Vec<String>> {
        list_json(&ZED_PROFILE, &self.config_path()?)
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
    fn cline_profile_emits_disabled_and_autoapprove() {
        // Cline-specific keys: `disabled: false` + `autoApprove: []`.
        // Without these the entry still parses but Cline's UI doesn't
        // recognise it as "enabled" — a silent broken-install class
        // of bug that's worth pinning.
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("mcp.json");
        let ctx = make_ctx("vex", tmp.path());

        install_json(&CLINE_PROFILE, &cfg, &ctx).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let entry = &v["mcpServers"]["vex"];
        assert_eq!(entry["disabled"], serde_json::json!(false));
        assert_eq!(entry["autoApprove"], serde_json::json!([]));
    }

    #[test]
    fn zed_profile_uses_context_servers_root_not_mcp_servers() {
        // Zed is the lone outlier on the root key. Pinning catches a
        // typo regression that would write `mcpServers` and have Zed
        // silently ignore the entry.
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("settings.json");
        let ctx = make_ctx("vex", tmp.path());

        install_json(&ZED_PROFILE, &cfg, &ctx).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(
            v.get("context_servers").is_some(),
            "Zed must write under `context_servers`, got: {v}"
        );
        assert!(
            v.get("mcpServers").is_none(),
            "Zed must NOT write under `mcpServers` — that key is for the other clients"
        );
    }

    #[test]
    fn toml_install_creates_mcp_servers_section() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        let ctx = make_ctx("vex", tmp.path());

        install_toml(&cfg, &ctx).unwrap();

        let raw = std::fs::read_to_string(&cfg).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["vex"]["command"],
            toml::Value::String("/path/to/vex-mcp".into())
        );
        assert_eq!(
            parsed["mcp_servers"]["vex"]["env"]["VEX_ROOT"],
            toml::Value::String(tmp.path().to_string_lossy().into_owned())
        );
    }

    #[test]
    fn toml_install_preserves_existing_top_level_keys() {
        // Codex's config.toml carries plenty of unrelated keys (model
        // selection, hook config, etc). Install must surgically add
        // the [mcp_servers.vex] entry without disturbing anything else.
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            "model = \"o1-pro\"\nallow_managed_hooks_only = true\n",
        )
        .unwrap();

        let ctx = make_ctx("vex", tmp.path());
        install_toml(&cfg, &ctx).unwrap();

        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(parsed["model"], toml::Value::String("o1-pro".into()));
        assert_eq!(
            parsed["allow_managed_hooks_only"],
            toml::Value::Boolean(true)
        );
        assert!(parsed["mcp_servers"]["vex"]["command"].is_str());
    }

    #[test]
    fn toml_install_is_idempotent_when_entry_matches() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        let ctx = make_ctx("vex", tmp.path());

        install_toml(&cfg, &ctx).unwrap();
        let second = install_toml(&cfg, &ctx).unwrap();
        assert!(matches!(second, InstallOutcome::AlreadyExists { .. }));
    }

    #[test]
    fn toml_uninstall_removes_only_target_entry() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            r#"
[mcp_servers.vex]
command = "/vex-mcp"
env = { VEX_ROOT = "/root" }

[mcp_servers.other]
command = "/other"
env = { FOO = "bar" }

[other_section]
key = "value"
"#,
        )
        .unwrap();

        let out = uninstall_toml(&cfg, "vex").unwrap();
        assert!(matches!(out, UninstallOutcome::Removed { .. }));

        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(parsed["mcp_servers"].get("vex").is_none());
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"],
            toml::Value::String("/other".into())
        );
        assert_eq!(
            parsed["other_section"]["key"],
            toml::Value::String("value".into())
        );
    }

    #[test]
    fn continue_dev_yaml_renders_with_expected_shape() {
        let tmp = TempDir::new().unwrap();
        let ctx = InstallContext {
            server_name: "vex".into(),
            binary_path: PathBuf::from("/opt/vex-mcp"),
            project_root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        };
        let yaml = render_continue_yaml(&ctx);
        // Pin the exact Continue.dev MCP server YAML shape — drift
        // here would silently produce files that Continue parses but
        // doesn't recognise as a server entry.
        assert!(yaml.starts_with("mcpServers:\n"));
        assert!(yaml.contains("name: vex"));
        assert!(yaml.contains("type: stdio"));
        assert!(yaml.contains("command: /opt/vex-mcp"));
        assert!(yaml.contains("VEX_ROOT:"));
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
