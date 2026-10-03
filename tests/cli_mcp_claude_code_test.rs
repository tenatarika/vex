//! `vex mcp install|uninstall|list --agent claude-code` delegates to the
//! `claude` CLI (`claude mcp add|remove|get`) instead of writing a config
//! file. Claude Code keeps user-scope MCP servers in `~/.claude.json`
//! alongside unrelated state and its docs recommend `claude mcp add`
//! over hand-editing that file; the pre-1.27.2 handler wrote
//! `~/.claude/claude_desktop_config.json`, which Claude Code never reads.
//!
//! Every test drives the real `vex` binary against a FAKE `claude`
//! (a shell script injected via `VEX_CLAUDE_BIN`) that appends its argv
//! to a log file. `HOME` / `USERPROFILE` point at a tempdir, so neither
//! the real `claude` nor the real home directory is ever touched.
//!
//! Unix-only: the fake is a `#!/bin/sh` script.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

const VEX_MCP: &str = "/opt/vex/bin/vex-mcp";
const ROOT: &str = "/work/my project";

/// The exact `claude mcp add` argv vex must produce for the default
/// server name, `VEX_MCP` and `ROOT`. Each element is one argv entry.
fn expected_add() -> Vec<String> {
    [
        "mcp",
        "add",
        "--scope",
        "user",
        "--transport",
        "stdio",
        "vex",
        "--env",
        &format!("VEX_ROOT={ROOT}"),
        "--",
        VEX_MCP,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Behaviour knobs for the fake `claude`. Refusal wording mirrors Claude
/// Code 2.1.285 (see the module docs of `src/integrations/mcp_claude_code.rs`).
#[derive(Clone, Copy)]
struct Fake {
    /// `get`: "user" / "local" (found, with that Scope: line), "missing",
    /// or "fail" (unrelated error).
    get: &'static str,
    /// `add`: "ok", "exists" (already in user config), or "fail".
    add: &'static str,
    /// `remove --scope user`: "ok", "missing" (no such server), or "fail".
    remove: &'static str,
}

impl Default for Fake {
    fn default() -> Self {
        Fake {
            get: "missing",
            add: "ok",
            remove: "missing",
        }
    }
}

struct Env {
    tmp: TempDir,
    fake: Fake,
}

const FAKE_CLAUDE: &str = r#"#!/bin/sh
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$FAKE_CLAUDE_LOG"
case "$1 $2" in
  "mcp get")
    case "$FAKE_CLAUDE_GET" in
      user)  printf '%s:\n  Scope: User config (available in all your projects)\n  Status: ok\n' "$3"; exit 0 ;;
      local) printf '%s:\n  Scope: Local config (private to you in this project)\n' "$3"; exit 0 ;;
      missing) echo "No MCP server named \"$3\". Configured servers: other"; exit 1 ;;
      *) echo "fake-claude: get exploded" >&2; exit 1 ;;
    esac ;;
  "mcp add")
    case "$FAKE_CLAUDE_ADD" in
      ok) echo "Added stdio MCP server $7 to user config"; exit 0 ;;
      exists) echo "MCP server $7 already exists in user config" >&2; exit 1 ;;
      *) echo "fake-claude: add exploded" >&2; exit 1 ;;
    esac ;;
  "mcp remove")
    case "$FAKE_CLAUDE_REMOVE" in
      ok) echo "Removed MCP server $5 from user config"; exit 0 ;;
      missing) echo "No MCP server named \"$5\" in user scope" >&2; exit 1 ;;
      *) echo "fake-claude: remove exploded" >&2; exit 1 ;;
    esac ;;
esac
echo "fake-claude: unexpected argv $*" >&2
exit 97
"#;

/// Path of the shared fake `claude`, (re)written atomically only when
/// missing or stale so concurrent test processes never exec a
/// half-written file.
fn shared_fake_claude() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join("fake-claude-v2");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(FAKE_CLAUDE) {
        let tmp = dir.join(format!("fake-claude-v2.{}.tmp", std::process::id()));
        std::fs::write(&tmp, FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
    }
    path
}

impl Env {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        std::fs::create_dir_all(tmp.path().join("project")).unwrap();
        std::fs::create_dir_all(tmp.path().join("emptybin")).unwrap();
        Env {
            tmp,
            fake: Fake::default(),
        }
    }
    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }
    fn project(&self) -> PathBuf {
        self.tmp.path().join("project")
    }
    fn log(&self) -> PathBuf {
        self.tmp.path().join("claude-argv.log")
    }

    /// Install the fake `claude` and return its path. Its behaviour
    /// comes from env vars set on the `vex` child (and inherited by the
    /// `claude` grandchild), so the script body is constant: it lives at
    /// one stable path under `CARGO_TARGET_TMPDIR` and is rewritten only
    /// when its content changes. Some endpoint-security agents stall the
    /// FIRST exec of every new executable for tens of seconds; a fresh
    /// script per test would pay that on every test.
    ///
    /// Each invocation appends one line to `$FAKE_CLAUDE_LOG`: every argv
    /// entry wrapped in `[...]`, so an arg with a space survives intact.
    fn write_fake(&mut self, fake: Fake) -> PathBuf {
        self.fake = fake;
        shared_fake_claude()
    }

    /// `vex` with HOME isolated, cwd in the project dir, and `claude`
    /// resolved from the fake (when `fake` is Some) or from a PATH that
    /// contains no `claude` at all (when None).
    fn vex(&self, fake: Option<&Path>) -> Command {
        let mut cmd = Command::cargo_bin("vex").unwrap();
        cmd.current_dir(self.project())
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("VEX_CACHE_DIR", self.tmp.path().join("cache"))
            .env_remove("VEX_CLAUDE_BIN")
            .env("FAKE_CLAUDE_LOG", self.log())
            .env("FAKE_CLAUDE_GET", self.fake.get)
            .env("FAKE_CLAUDE_ADD", self.fake.add)
            .env("FAKE_CLAUDE_REMOVE", self.fake.remove);
        match fake {
            Some(p) => {
                cmd.env("VEX_CLAUDE_BIN", p);
            }
            None => {
                cmd.env("PATH", self.tmp.path().join("emptybin"));
            }
        }
        cmd
    }

    /// Every recorded `claude` invocation, one argv vector per call.
    fn calls(&self) -> Vec<Vec<String>> {
        let Ok(raw) = std::fs::read_to_string(self.log()) else {
            return Vec::new();
        };
        raw.lines()
            .map(|line| {
                line.strip_prefix('[')
                    .and_then(|l| l.strip_suffix(']'))
                    .map(|inner| inner.split("][").map(String::from).collect())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Claude Code must never get a file written by vex — neither the
    /// old (wrong) Desktop path nor `~/.claude.json` itself.
    fn assert_no_claude_files(&self) {
        let home = self.home();
        assert!(
            !home
                .join(".claude")
                .join("claude_desktop_config.json")
                .exists(),
            "vex must not write ~/.claude/claude_desktop_config.json"
        );
        assert!(
            !home.join(".claude.json").exists(),
            "vex must not write ~/.claude.json directly"
        );
    }
}

fn install_args() -> Vec<&'static str> {
    vec![
        "mcp",
        "install",
        "--agent",
        "claude-code",
        "--binary-path",
        VEX_MCP,
        "--project-root",
        ROOT,
    ]
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn remove_argv() -> Vec<String> {
    argv(&["mcp", "remove", "--scope", "user", "vex"])
}

fn run_ok(env: &Env, fake: Option<&Path>, args: &[&str]) -> (String, String) {
    let out = env
        .vex(fake)
        .args(args)
        .assert()
        .success()
        .get_output()
        .clone();
    (stdout_of(&out), stderr_of(&out))
}

fn run_code(env: &Env, fake: Option<&Path>, args: &[&str], code: i32) -> (String, String) {
    let out = env
        .vex(fake)
        .args(args)
        .assert()
        .code(code)
        .get_output()
        .clone();
    (stdout_of(&out), stderr_of(&out))
}

const ALL_ARGS: &[&str] = &[
    "mcp",
    "install",
    "--agent",
    "all",
    "--binary-path",
    VEX_MCP,
    "--project-root",
    ROOT,
];

#[test]
fn install_runs_only_claude_mcp_add_with_exact_argv() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake::default());

    let (stdout, _) = run_ok(&env, Some(&fake), &install_args());

    assert_eq!(
        env.calls(),
        vec![expected_add()],
        "install must not probe with the scope-blind `get`: {stdout}"
    );
    assert!(stdout.contains("Claude Code: registered `vex`"), "{stdout}");
    env.assert_no_claude_files();
}

#[test]
fn install_honours_custom_server_name() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake::default());
    let mut args = install_args();
    args.extend(["--server-name", "vex-api"]);

    run_ok(&env, Some(&fake), &args);

    let calls = env.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0][6], "vex-api",
        "server name is the positional <name>"
    );
}

#[test]
fn install_maps_already_exists_refusal_to_already_registered() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        add: "exists",
        ..Fake::default()
    });

    let (stdout, _) = run_ok(&env, Some(&fake), &install_args());

    assert_eq!(env.calls(), vec![expected_add()]);
    assert!(stdout.contains("already registered"), "{stdout}");
    assert!(
        stdout.contains("--force"),
        "must point at --force: {stdout}"
    );
    assert!(
        stdout.contains("already exists in user config"),
        "claude's own wording is shown: {stdout}"
    );
}

#[test]
fn install_ignores_same_named_entry_in_other_scope() {
    // `get` would find a local-scope `vex`; install must not consult it
    // and must register the user-scope entry anyway.
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        get: "local",
        ..Fake::default()
    });

    let (stdout, _) = run_ok(&env, Some(&fake), &install_args());

    assert_eq!(env.calls(), vec![expected_add()]);
    assert!(stdout.contains("registered"), "{stdout}");
}

#[test]
fn install_force_removes_then_adds() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        remove: "ok",
        ..Fake::default()
    });
    let mut args = install_args();
    args.push("--force");

    let (stdout, _) = run_ok(&env, Some(&fake), &args);

    assert_eq!(env.calls(), vec![remove_argv(), expected_add()]);
    assert!(
        stdout.contains("claude mcp remove --scope user vex"),
        "{stdout}"
    );
    env.assert_no_claude_files();
}

#[test]
fn install_force_on_absent_user_entry_still_adds() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake::default()); // remove → "No MCP server named"
    let mut args = install_args();
    args.push("--force");

    let (stdout, _) = run_ok(&env, Some(&fake), &args);

    assert_eq!(env.calls(), vec![remove_argv(), expected_add()]);
    assert!(
        !stdout.contains("claude mcp remove"),
        "a no-op remove must not be reported as run: {stdout}"
    );
}

#[test]
fn install_force_remove_error_stops_before_add() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        remove: "fail",
        ..Fake::default()
    });
    let mut args = install_args();
    args.push("--force");

    let (_, stderr) = run_code(&env, Some(&fake), &args, 2);

    assert_eq!(env.calls(), vec![remove_argv()]);
    assert!(stderr.contains("fake-claude: remove exploded"), "{stderr}");
}

#[test]
fn install_force_partial_failure_says_entry_was_removed_and_prints_retry() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        remove: "ok",
        add: "fail",
        ..Fake::default()
    });
    let mut args = install_args();
    args.push("--force");

    let (_, stderr) = run_code(&env, Some(&fake), &args, 2);

    assert_eq!(env.calls(), vec![remove_argv(), expected_add()]);
    assert!(stderr.contains("removed the previous"), "{stderr}");
    assert!(stderr.contains("NOT registered"), "{stderr}");
    assert!(stderr.contains("fake-claude: add exploded"), "{stderr}");
    assert!(
        stderr.contains(
            "claude mcp add --scope user --transport stdio vex \
             --env 'VEX_ROOT=/work/my project' -- /opt/vex/bin/vex-mcp"
        ),
        "the exact retry command must be printed: {stderr}"
    );
}

#[test]
fn install_dry_run_prints_command_and_runs_nothing() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake::default());
    let mut args = install_args();
    args.push("--dry-run");

    let (stdout, _) = run_ok(&env, Some(&fake), &args);

    assert!(env.calls().is_empty(), "--dry-run must not invoke claude");
    assert!(
        stdout.contains(
            "claude mcp add --scope user --transport stdio vex \
             --env 'VEX_ROOT=/work/my project' -- /opt/vex/bin/vex-mcp"
        ),
        "dry-run must print the exact, shell-quoted command, got: {stdout}"
    );
    assert!(!stdout.contains("claude mcp remove"));
    env.assert_no_claude_files();
}

#[test]
fn install_dry_run_with_force_also_prints_remove() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake::default());
    let mut args = install_args();
    args.extend(["--dry-run", "--force"]);

    let (stdout, _) = run_ok(&env, Some(&fake), &args);

    assert!(env.calls().is_empty());
    let rm = stdout
        .find("claude mcp remove --scope user vex")
        .expect("remove line");
    let add = stdout
        .find("claude mcp add --scope user")
        .expect("add line");
    assert!(rm < add, "remove must be listed before add: {stdout}");
}

#[test]
fn install_surfaces_claude_output_on_unrecognised_failure() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        add: "fail",
        ..Fake::default()
    });

    let (stdout, stderr) = run_code(&env, Some(&fake), &install_args(), 2);

    assert!(
        stderr.contains("fake-claude: add exploded"),
        "claude's stderr must reach the user, got: {stderr}"
    );
    assert!(
        !stdout.contains("already registered"),
        "an unknown failure must not pass for success: {stdout}"
    );
}

#[test]
fn install_without_claude_prints_command_and_writes_nothing() {
    let env = Env::new();

    let (stdout, _) = run_code(&env, None, &install_args(), 2);

    assert!(
        stdout.contains(
            "claude mcp add --scope user --transport stdio vex \
             --env 'VEX_ROOT=/work/my project' -- /opt/vex/bin/vex-mcp"
        ),
        "missing claude must print the manual command, got: {stdout}"
    );
    assert!(stdout.contains("not found"), "must explain why: {stdout}");
    env.assert_no_claude_files();
    assert_eq!(
        std::fs::read_dir(env.home()).unwrap().count(),
        0,
        "nothing may be written under HOME"
    );
}

#[test]
fn install_with_missing_override_binary_is_treated_as_missing() {
    let env = Env::new();
    let ghost = env.tmp.path().join("does-not-exist");

    let (stdout, _) = run_code(&env, Some(&ghost), &install_args(), 2);

    assert!(stdout.contains("claude mcp add --scope user"));
    env.assert_no_claude_files();
}

#[test]
fn install_all_continues_past_missing_claude() {
    let env = Env::new();

    let (stdout, _) = run_ok(&env, None, ALL_ARGS);

    let home = env.home();
    assert!(home.join(".cursor").join("mcp.json").exists());
    assert!(home.join(".codex").join("config.toml").exists());
    assert!(home
        .join(".config")
        .join("zed")
        .join("settings.json")
        .exists());
    assert!(env
        .project()
        .join(".continue")
        .join("mcpServers")
        .join("vex.yaml")
        .exists());
    env.assert_no_claude_files();
    assert!(
        stdout.contains("claude mcp add --scope user"),
        "the skipped agent's manual command must still be printed: {stdout}"
    );
}

#[test]
fn install_all_continues_past_failing_claude_but_exits_2() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        add: "fail",
        ..Fake::default()
    });

    let (_, stderr) = run_code(&env, Some(&fake), ALL_ARGS, 2);

    assert!(
        env.home().join(".cursor").join("mcp.json").exists(),
        "agents after a failing one must still be installed"
    );
    assert!(stderr.contains("fake-claude: add exploded"));
}

#[test]
fn install_rejects_server_name_starting_with_dash() {
    let env = Env::new();
    let mut args = install_args();
    args.push("--server-name=-rf");

    let (_, stderr) = run_code(&env, None, &args, 2);

    assert!(stderr.contains("must not start with `-`"), "{stderr}");
    assert!(env.calls().is_empty());
}

#[test]
fn uninstall_rejects_server_name_starting_with_dash() {
    let env = Env::new();

    let (_, stderr) = run_code(
        &env,
        None,
        &[
            "mcp",
            "uninstall",
            "--agent",
            "claude-code",
            "--server-name=-x",
        ],
        2,
    );

    assert!(stderr.contains("must not start with `-`"), "{stderr}");
}

#[test]
fn uninstall_runs_only_claude_mcp_remove() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        remove: "ok",
        ..Fake::default()
    });

    let (stdout, _) = run_ok(
        &env,
        Some(&fake),
        &["mcp", "uninstall", "--agent", "claude-code"],
    );

    assert_eq!(env.calls(), vec![remove_argv()]);
    assert!(stdout.contains("removed `vex`"), "{stdout}");
}

#[test]
fn uninstall_maps_not_found_to_noop() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake::default());

    let (stdout, _) = run_ok(
        &env,
        Some(&fake),
        &["mcp", "uninstall", "--agent", "claude-code"],
    );

    assert_eq!(env.calls(), vec![remove_argv()]);
    assert!(stdout.contains("nothing to do"), "{stdout}");
}

#[test]
fn uninstall_ignores_same_named_entry_in_other_scope() {
    // A local-scope `vex` exists (get would succeed) but user scope has
    // none: uninstall is a no-op, not an error.
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        get: "local",
        ..Fake::default()
    });

    let (stdout, _) = run_ok(
        &env,
        Some(&fake),
        &["mcp", "uninstall", "--agent", "claude-code"],
    );

    assert_eq!(env.calls(), vec![remove_argv()]);
    assert!(stdout.contains("nothing to do"), "{stdout}");
}

#[test]
fn uninstall_surfaces_unrecognised_failure() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        remove: "fail",
        ..Fake::default()
    });

    let (stdout, stderr) = run_code(
        &env,
        Some(&fake),
        &["mcp", "uninstall", "--agent", "claude-code"],
        2,
    );

    assert!(stderr.contains("fake-claude: remove exploded"), "{stderr}");
    assert!(!stdout.contains("nothing to do"), "{stdout}");
}

#[test]
fn uninstall_without_claude_prints_command() {
    let env = Env::new();

    let (stdout, _) = run_code(
        &env,
        None,
        &["mcp", "uninstall", "--agent", "claude-code"],
        2,
    );

    assert!(stdout.contains("claude mcp remove --scope user vex"));
}

#[test]
fn list_reports_presence_with_claudes_scope() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        get: "user",
        ..Fake::default()
    });

    let (stdout, _) = run_ok(
        &env,
        Some(&fake),
        &["mcp", "list", "--agent", "claude-code"],
    );

    assert_eq!(env.calls(), vec![argv(&["mcp", "get", "vex"])]);
    assert!(stdout.contains("- vex — User config"), "{stdout}");
}

#[test]
fn list_reports_other_scope_honestly() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        get: "local",
        ..Fake::default()
    });

    let (stdout, _) = run_ok(
        &env,
        Some(&fake),
        &["mcp", "list", "--agent", "claude-code"],
    );

    assert!(stdout.contains("- vex — Local config"), "{stdout}");
    assert!(!stdout.contains("User config"), "{stdout}");
}

#[test]
fn list_reports_absence() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake::default());

    let (stdout, _) = run_ok(
        &env,
        Some(&fake),
        &["mcp", "list", "--agent", "claude-code"],
    );

    assert!(!stdout.contains("- vex"), "{stdout}");
    assert!(stdout.contains("no MCP servers configured"), "{stdout}");
}

#[test]
fn list_surfaces_unrecognised_get_failure() {
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        get: "fail",
        ..Fake::default()
    });

    let (_, stderr) = run_code(
        &env,
        Some(&fake),
        &["mcp", "list", "--agent", "claude-code"],
        2,
    );

    assert!(stderr.contains("fake-claude: get exploded"), "{stderr}");
}

#[test]
fn list_all_continues_past_missing_claude() {
    let env = Env::new();

    let (stdout, _) = run_ok(&env, None, &["mcp", "list"]);

    assert!(stdout.contains("Cursor"), "{stdout}");
    assert!(stdout.contains("Zed"), "{stdout}");
}

#[test]
fn list_all_continues_when_home_is_unresolvable() {
    // `list_source()` needs HOME for the file-based agents; with it unset
    // each of those fails on its own instead of aborting the fan-out.
    let mut env = Env::new();
    let fake = env.write_fake(Fake {
        get: "user",
        ..Fake::default()
    });

    let out = env
        .vex(Some(&fake))
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .args(["mcp", "list"])
        .assert()
        .code(2)
        .get_output()
        .clone();

    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("- vex — User config"),
        "Claude Code must still be listed: {stdout}"
    );
    assert!(stdout_of(&out).contains("Continue.dev"), "{stdout}");
}

#[test]
fn install_help_lists_every_agent_id() {
    let out = Command::cargo_bin("vex")
        .unwrap()
        .args(["mcp", "install", "--help"])
        .assert()
        .success()
        .get_output()
        .clone();
    let help = stdout_of(&out);
    for id in [
        "claude-code",
        "cursor",
        "codex-cli",
        "windsurf",
        "cline",
        "continue",
        "zed",
        "all",
    ] {
        assert!(help.contains(id), "`--agent` help must list `{id}`: {help}");
    }
    assert!(
        !help.contains("See `vex mcp install --help`"),
        "help must not point at itself"
    );
}
