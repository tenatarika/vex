use super::*;
use tempfile::TempDir;

fn ctx(force: bool, dry_run: bool) -> InstallContext {
    InstallContext {
        server_name: "vex".into(),
        binary_path: PathBuf::from("/opt/vex-mcp"),
        project_root: PathBuf::from("/work/proj"),
        dry_run,
        force,
    }
}

fn strs(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn add_args_put_name_before_variadic_env_and_use_separator() {
    assert_eq!(
        strs(&add_args(&ctx(false, false))),
        [
            "mcp",
            "add",
            "--scope",
            "user",
            "--transport",
            "stdio",
            "vex",
            "--env",
            "VEX_ROOT=/work/proj",
            "--",
            "/opt/vex-mcp",
        ]
    );
}

#[test]
fn remove_and_get_args_are_exact() {
    assert_eq!(
        strs(&remove_args("vex-api")),
        ["mcp", "remove", "--scope", "user", "vex-api"]
    );
    assert_eq!(strs(&get_args("vex-api")), ["mcp", "get", "vex-api"]);
}

#[test]
fn quote_posix_leaves_safe_words_and_quotes_the_rest() {
    assert_eq!(quote_posix("VEX_ROOT=/a/b-c.d"), "VEX_ROOT=/a/b-c.d");
    assert_eq!(quote_posix("/my dir"), "'/my dir'");
    assert_eq!(quote_posix("it's"), r"'it'\''s'");
    assert_eq!(quote_posix(""), "''");
    assert_eq!(quote_posix("$HOME"), "'$HOME'");
    assert_eq!(quote_posix("50%"), "'50%'");
    assert_eq!(quote_posix(r"C:\x"), r"'C:\x'");
}

#[test]
fn quote_windows_uses_double_quotes_and_keeps_backslash_paths_bare() {
    assert_eq!(
        quote_windows(r"C:\tools\vex-mcp.exe"),
        r"C:\tools\vex-mcp.exe"
    );
    assert_eq!(
        quote_windows(r"VEX_ROOT=C:\my proj"),
        r#""VEX_ROOT=C:\my proj""#
    );
    assert_eq!(quote_windows(r#"a"b"#), r#""a\"b""#);
    assert_eq!(quote_windows(""), r#""""#);
}

#[test]
fn shell_quote_matches_host_shell() {
    let expected = if cfg!(windows) {
        quote_windows("/my dir")
    } else {
        quote_posix("/my dir")
    };
    assert_eq!(shell_quote("/my dir"), expected);
}

#[test]
fn ran_matches_refusals_case_insensitively() {
    let exists = Ran {
        success: false,
        output: "MCP server vex already exists in user config".into(),
    };
    assert!(exists.says_any(ALREADY_EXISTS));
    assert!(!exists.says_any(NOT_FOUND));
    let missing = Ran {
        success: false,
        output: r#"No MCP server named "vex" in user scope"#.into(),
    };
    assert!(missing.says_any(NOT_FOUND));
    assert!(!missing.says_any(ALREADY_EXISTS));
    let other = Ran {
        success: false,
        output: "EACCES: permission denied, open '/home/u/.claude.json'".into(),
    };
    assert!(!other.says_any(NOT_FOUND) && !other.says_any(ALREADY_EXISTS));
}

#[cfg(windows)]
#[test]
fn find_on_path_prefers_exe_over_earlier_cmd_shim() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    std::fs::write(first.path().join("claude.cmd"), b"").unwrap();
    std::fs::write(second.path().join("claude.exe"), b"").unwrap();
    let path_var = std::env::join_paths([first.path(), second.path()]).unwrap();
    assert_eq!(
        find_on_path(&path_var),
        Some(second.path().join("claude.exe"))
    );
}

#[test]
fn display_command_prefixes_claude() {
    assert_eq!(
        display_command(&remove_args("vex")),
        "claude mcp remove --scope user vex"
    );
}

#[test]
fn dry_run_plans_without_running_even_without_claude() {
    let out = install_via(None, &ctx(true, true)).unwrap();
    assert_eq!(
        out,
        InstallOutcome::WouldRun {
            commands: vec![
                "claude mcp remove --scope user vex".into(),
                "claude mcp add --scope user --transport stdio vex --env \
                 VEX_ROOT=/work/proj -- /opt/vex-mcp"
                    .into(),
            ]
        }
    );
}

#[test]
fn missing_claude_install_needs_action_with_add_command() {
    match install_via(None, &ctx(false, false)).unwrap() {
        InstallOutcome::NeedsAction { reason, commands } => {
            assert!(reason.contains("not found"));
            assert_eq!(
                commands,
                ["claude mcp add --scope user --transport stdio vex --env \
                  VEX_ROOT=/work/proj -- /opt/vex-mcp"]
            );
        }
        other => panic!("expected NeedsAction, got {other:?}"),
    }
}

#[test]
fn missing_claude_uninstall_needs_action_with_remove_command() {
    match uninstall_via(None, "vex").unwrap() {
        UninstallOutcome::NeedsAction { commands, .. } => {
            assert_eq!(commands, ["claude mcp remove --scope user vex"]);
        }
        other => panic!("expected NeedsAction, got {other:?}"),
    }
}

#[test]
fn missing_claude_list_is_a_typed_error() {
    let err = list_via(None, "vex").unwrap_err();
    assert!(err.downcast_ref::<ClaudeCliMissing>().is_some());
}

#[test]
fn find_on_path_locates_claude_and_skips_dirs_without_it() {
    let empty = TempDir::new().unwrap();
    let with = TempDir::new().unwrap();
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    std::fs::write(with.path().join(name), b"").unwrap();

    let path_var = std::env::join_paths([empty.path(), with.path()]).unwrap();
    assert_eq!(find_on_path(&path_var), Some(with.path().join(name)));

    let only_empty = std::env::join_paths([empty.path()]).unwrap();
    assert_eq!(find_on_path(&only_empty), None);
}

#[test]
fn find_on_path_ignores_a_directory_named_claude() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join("claude")).unwrap();
    let path_var = std::env::join_paths([tmp.path()]).unwrap();
    if !cfg!(windows) {
        assert_eq!(find_on_path(&path_var), None);
    }
}
