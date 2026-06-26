//! Environment validation helpers: resolve and sanity-check `VEX_BIN` and
//! the JSON-RPC `project_root` argument before spawning the `vex` binary.
//!
//! Extracted from `main.rs` in the v1.21 split — see
//! `.claude/Task/v1.21-vex-mcp-split.md`.

use anyhow::Result;

/// v1.12.0 S8.3 — resolve the `vex` binary path and validate it before
/// `Command::spawn` so a typo'd `VEX_BIN` surfaces a human-readable error
/// instead of an opaque OS-level "No such file or directory" buried
/// inside the JSON-RPC tool-call response. When `VEX_BIN` is unset we
/// keep the existing behaviour: fall through to the literal string
/// `"vex"` and let the OS's PATH resolution find it (or fail loudly
/// later — that path is already user-controlled).
pub(crate) fn resolve_vex_bin() -> Result<String> {
    let Some(raw) = std::env::var_os("VEX_BIN") else {
        return Ok("vex".into());
    };
    let path = std::path::PathBuf::from(&raw);
    if !path.exists() {
        anyhow::bail!(
            "VEX_BIN points to `{}` but no such file exists; \
             unset VEX_BIN to fall back to PATH lookup of `vex`",
            path.display()
        );
    }
    if !path.is_file() {
        anyhow::bail!(
            "VEX_BIN points to `{}` but it is not a regular file \
             (likely a directory); unset VEX_BIN or point it at the \
             `vex` binary directly",
            path.display()
        );
    }
    // On Unix, additionally assert the binary is executable. Windows
    // associates executability by extension (.exe), so the `is_file`
    // check above is sufficient there.
    #[cfg(unix)]
    {
        use anyhow::Context;
        use std::os::unix::fs::PermissionsExt;
        let mode = path
            .metadata()
            .with_context(|| format!("stat VEX_BIN target `{}`", path.display()))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            anyhow::bail!(
                "VEX_BIN target `{}` is not executable (mode {:o}); \
                 run `chmod +x` on it or unset VEX_BIN",
                path.display(),
                mode & 0o777
            );
        }
    }
    Ok(path
        .into_os_string()
        .into_string()
        .unwrap_or_else(|os| os.to_string_lossy().into_owned()))
}

/// v1.12.0 S8.3 — validate the resolved project root before passing it to
/// `Command::current_dir`. Without this, a bogus `project_root` argument
/// (or a typo'd `VEX_ROOT`) yields the same opaque OS-level error as
/// VEX_BIN above. We keep `.` (the implicit default) un-canonicalized so
/// the spawn falls through to the MCP server's cwd unchanged.
pub(crate) fn validate_project_root(project_root: &str) -> Result<()> {
    if project_root == "." {
        return Ok(());
    }
    let path = std::path::Path::new(project_root);
    if !path.exists() {
        anyhow::bail!(
            "project_root `{}` does not exist (set via tool arg \
             `project_root` or env VEX_ROOT)",
            project_root
        );
    }
    if !path.is_dir() {
        anyhow::bail!("project_root `{}` is not a directory", project_root);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- S8.3 (v1.12.0): VEX_BIN + project_root validation ----------------

    /// All three VEX_BIN scenarios live in a single test because `cargo
    /// test` runs unit tests in parallel within the same binary and
    /// `std::env::set_var` is process-global — splitting them would race
    /// (one test's `set_var` clobbers another's snapshot). The scenarios
    /// are independent enough to assert sequentially in one body.
    #[test]
    fn resolve_vex_bin_validates_env_or_falls_back_to_path_literal() {
        let prior = std::env::var_os("VEX_BIN");

        // Scenario 1: VEX_BIN unset → falls back to literal "vex" so the
        // OS does PATH resolution at spawn time.
        // SAFETY: tests can race set_var/remove_var but we're inside one
        // test body that consolidates all three scenarios.
        unsafe {
            std::env::remove_var("VEX_BIN");
        }
        assert_eq!(resolve_vex_bin().expect("scenario 1 must succeed"), "vex");

        // Scenario 2: VEX_BIN points at a nonexistent file → clear error.
        unsafe {
            std::env::set_var("VEX_BIN", "/definitely/not/a/real/path/vex_xxx");
        }
        let err = resolve_vex_bin().expect_err("scenario 2 must fail");
        assert!(
            format!("{err}").contains("no such file"),
            "scenario 2 must mention 'no such file', got: {err}"
        );

        // Scenario 3: VEX_BIN points at a directory → clear error.
        // Use `env::temp_dir()` so the path exists on every platform —
        // hard-coding `/tmp` made Windows CI fall through to the
        // "no such file" branch instead of "not a regular file".
        let dir = std::env::temp_dir();
        unsafe {
            std::env::set_var("VEX_BIN", &dir);
        }
        let err = resolve_vex_bin().expect_err("scenario 3 must fail");
        assert!(
            format!("{err}").contains("not a regular file"),
            "scenario 3 must mention 'not a regular file', got: {err}"
        );

        // Restore prior state so neighbouring tests (and the broader
        // test binary) see no observable change.
        unsafe {
            match prior {
                Some(v) => std::env::set_var("VEX_BIN", v),
                None => std::env::remove_var("VEX_BIN"),
            }
        }
    }

    #[test]
    fn validate_project_root_passes_dot() {
        // "." is the implicit default; we don't canonicalize it so the
        // server's cwd wins. Validation must short-circuit on this case
        // so a server running in a non-existent cwd (impossible at the OS
        // level but possible inside containers with deleted dirs) is not
        // gratuitously rejected.
        validate_project_root(".").expect("'.' must pass");
    }

    #[test]
    fn validate_project_root_rejects_missing_path() {
        let err =
            validate_project_root("/definitely/not/a/real/directory/xxx").expect_err("must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("does not exist"),
            "error must mention 'does not exist', got: {msg}"
        );
    }

    #[test]
    fn validate_project_root_rejects_file_target() {
        // Cargo.toml exists in the crate root; safe target.
        let cargo_toml = std::env::current_dir().expect("cwd").join("Cargo.toml");
        let err = validate_project_root(cargo_toml.to_str().expect("utf-8"))
            .expect_err("must fail on a file path");
        let msg = format!("{err}");
        assert!(
            msg.contains("not a directory"),
            "error must mention 'not a directory', got: {msg}"
        );
    }
}
