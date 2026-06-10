//! `vex self-update` — pull a newer release binary from GitHub.
//! Extracted from `cli/mod.rs` in S1 Group B together with its embedded
//! ed25519 release pubkey and the compile-time length assertion (S5).

use anyhow::{Context, Result};

use super::self_update_flow;

/// ed25519 public key used to verify release archives signed in CI via
/// `zipsign`. Anyone modifying this MUST also rotate the corresponding
/// private key stored in the `ZIPSIGN_PRIVATE_KEY` GitHub Secret —
/// otherwise every subsequent release will fail to verify on update.
///
/// Generation: `zipsign gen-key vex.priv vex.pub`. The 32 bytes below
/// are the raw contents of `vex.pub`.
const VEX_RELEASE_PUBKEY: &[u8] = &[
    0x03, 0x9e, 0x75, 0x96, 0xbe, 0x60, 0xaf, 0x61, 0xdf, 0xdf, 0xb7, 0x93, 0x07, 0xc3, 0x2e, 0x95,
    0x38, 0xc9, 0x35, 0xc0, 0xe2, 0x05, 0xcc, 0x9d, 0x0e, 0x31, 0xf9, 0x66, 0x7d, 0xa6, 0x49, 0x51,
];

// S5 — compile-time guard. ed25519 public keys are exactly 32 bytes; if
// someone edits `VEX_RELEASE_PUBKEY` above and the byte count drifts,
// the build fails with this message instead of panicking at runtime
// inside `cmd_self_update`. Replaces the previous
// `.expect("VEX_RELEASE_PUBKEY must be 32 bytes")` runtime check.
const _: () = assert!(
    VEX_RELEASE_PUBKEY.len() == 32,
    "VEX_RELEASE_PUBKEY must be exactly 32 bytes (ed25519 public key)"
);

/// Update the running binary from the latest GitHub release. The
/// self_update crate handles platform detection (target triple) and the
/// release/asset lookup; the apply path lives in `self_update_flow`, which
/// downloads + verifies the archive once and installs EVERY file in it —
/// the bundled DirectML.dll sidecar included, which the crate's built-in
/// `update()` silently dropped (it extracts only the named binary).
pub(crate) fn cmd_self_update(check_only: bool, no_confirm: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    // SAFETY of the `try_into` below: the byte count is asserted at
    // compile time by the `const _: () = assert!(...)` above. The
    // `unwrap()` here is provably unreachable — if the slice were the
    // wrong length, the crate wouldn't compile.
    let pubkey: [u8; 32] = VEX_RELEASE_PUBKEY
        .try_into()
        .expect("checked at compile time");
    // U1 bug fix: self_update's default asset matcher does
    // `name.contains(target_triple)`, and the v1.12.0 release ships
    // TWO archives matching `x86_64-pc-windows-msvc` (and likewise on
    // every other platform):
    //
    //   vex-x86_64-pc-windows-msvc.tar.gz       (the CLI — what we want)
    //   vex-mcp-x86_64-pc-windows-msvc.tar.gz   (the MCP server — wrong)
    //
    // Without an `identifier`, `self_update::Release::asset_for`
    // returns the first matching asset in iteration order. The
    // GitHub API lists assets alphabetically and `vex-mcp-…` precedes
    // `vex-x86_64-…`, so the updater downloaded the MCP archive and
    // then failed to extract `vex.exe` from it.
    //
    // The fix narrows the match via `identifier = "vex-<target>"` —
    // `name.contains("vex-x86_64-pc-windows-msvc")` is true for the
    // CLI archive but false for `vex-mcp-x86_64-pc-windows-msvc.tar.gz`
    // because the `vex-` is followed by `mcp-`, not the target triple.
    // NB: `contains` is a substring check, not a prefix — narrow but
    // not anchored. A future release that adds e.g.
    // `vex-x86_64-pc-windows-msvc-debug.tar.gz` would also match this
    // identifier; if that happens the right fix is to assert the
    // archive entry path explicitly via `bin_path_in_archive`. The
    // regression tests below pin every CURRENT release-matrix triple
    // AND reproduce the original bug when identifier is absent, so a
    // future self_update semantics drift surfaces immediately.
    let target = self_update::get_target();
    let identifier = format!("vex-{target}");
    let status = self_update::backends::github::Update::configure()
        .repo_owner("tenatarika")
        .repo_name("vex")
        .bin_name("vex")
        .identifier(&identifier)
        .current_version(current)
        .show_download_progress(true)
        .no_confirm(no_confirm)
        .verifying_keys([pubkey])
        .build()
        .context("configure self-update client")?;

    if check_only {
        let release = status
            .get_latest_release()
            .context("fetch latest release from GitHub (offline or rate-limited?)")?;
        let latest = release.version.as_str();
        if latest == current {
            println!("vex is up to date ({current}).");
            return Ok(());
        }
        // Surface a semver parse failure as an error rather than silently
        // mis-reporting direction — a release tagged with an unexpected
        // prefix would otherwise produce a confusing "no action needed".
        let newer = self_update::version::bump_is_greater(current, latest)
            .with_context(|| format!("could not compare versions {current:?} and {latest:?}"))?;
        if newer {
            println!(
                "Update available: {current} → {latest} ({}).\nRun `vex self-update` (omit --check) to install.",
                release.name
            );
        } else {
            // Local build ahead of GitHub (e.g. a dev branch). Don't
            // pretend an update is needed — just report what's out there.
            println!("Latest release: {latest} (current: {current} — newer, no action needed).");
        }
        return Ok(());
    }

    // Apply path. `status.update()` is deliberately NOT used: it extracts
    // only the named binary from the archive, so Windows self-updates
    // dropped the DirectML.dll sidecar and degraded GPU embedding to CPU
    // until the next manual reinstall. The custom flow keeps the crate's
    // confirmation UX, then installs the whole archive.
    //
    // DELIBERATE semantics change vs `status.update()`: the gate below is
    // `get_latest_release()` + `bump_is_greater` (strictly newer, majors
    // included, prereleases excluded by GitHub's /releases/latest), where
    // the crate used `get_latest_releases()` + `bump_is_compatible` (major-
    // pinned, prereleases visible). For a single-binary CLI, stopping at a
    // major boundary just strands users on an unmaintained line — and the
    // `--check` branch above has always used exactly these newer semantics,
    // so check and apply now agree. Downgrades remain impossible either
    // way (`latest < current` never passes the gate).
    let release = status
        .get_latest_release()
        .context("fetch latest release from GitHub (offline or rate-limited?)")?;
    let latest = release.version.as_str();
    let newer = latest != current
        && self_update::version::bump_is_greater(current, latest)
            .with_context(|| format!("could not compare versions {current:?} and {latest:?}"))?;
    if !newer {
        // Mirrors the crate's `Status::UpToDate` arm (also covers a local
        // dev build that is ahead of the newest GitHub release).
        println!("vex is already up to date ({current}).");
        return Ok(());
    }

    let asset = release
        .asset_for(target, Some(&identifier))
        .ok_or_else(|| {
            anyhow::anyhow!("no release asset found for target `{target}` in vex {latest}")
        })?;

    if !no_confirm {
        println!("\nvex release status:");
        println!("  * Current version: {current}");
        println!("  * New release: {} ({})", latest, asset.name);
        println!("\nThe new release will be downloaded/extracted and the existing binary will be replaced.");
        self_update_flow::confirm("Do you want to continue? [Y/n] ")?;
    }

    self_update_flow::apply_update(&asset.name, &asset.download_url, pubkey, "vex")
        .context("apply self-update")?;
    println!("Updated to vex {latest}. Restart any open shells.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use self_update::update::{Release, ReleaseAsset};

    /// Construct a synthetic v1.12.0-shaped release with BOTH the CLI
    /// archive and the MCP archive matching the requested target — the
    /// exact configuration that bit Windows users in v1.12. The
    /// matcher pre-1.13.1 would pick the MCP archive (alphabetically
    /// first); the fix's `identifier = "vex-{target}"` anchors the
    /// CLI archive uniquely.
    fn synthetic_release(target: &str) -> Release {
        Release {
            name: format!("vex {target} test"),
            version: "1.12.0".to_string(),
            date: "2026-06-04".to_string(),
            body: None,
            assets: vec![
                // Lexical order matters — `vex-mcp-...` < `vex-x86_64-...`
                // alphabetically, so the unanchored matcher picked
                // this one first.
                ReleaseAsset {
                    name: format!("vex-mcp-{target}.tar.gz"),
                    download_url: format!("https://example.test/vex-mcp-{target}.tar.gz"),
                },
                ReleaseAsset {
                    name: format!("vex-{target}.tar.gz"),
                    download_url: format!("https://example.test/vex-{target}.tar.gz"),
                },
            ],
        }
    }

    fn assert_picks_cli(target: &str) {
        let release = synthetic_release(target);
        let identifier = format!("vex-{target}");
        let picked = release
            .asset_for(target, Some(&identifier))
            .expect("identifier must select an asset");
        assert_eq!(
            picked.name,
            format!("vex-{target}.tar.gz"),
            "identifier `vex-{{target}}` should anchor the CLI archive on {target}"
        );
        // Sanity: without the identifier, the MCP archive wins —
        // reproduces the original bug. If this stops failing, the
        // self_update crate changed semantics and we should re-audit.
        let bug = release
            .asset_for(target, None)
            .expect("unanchored matcher must still pick *something*");
        assert_eq!(
            bug.name,
            format!("vex-mcp-{target}.tar.gz"),
            "unanchored matcher should reproduce the v1.12 bug (picks MCP on {target})"
        );
    }

    #[test]
    fn identifier_picks_cli_archive_on_windows_x64() {
        assert_picks_cli("x86_64-pc-windows-msvc");
    }

    #[test]
    fn identifier_picks_cli_archive_on_linux_x64() {
        assert_picks_cli("x86_64-unknown-linux-gnu");
    }

    #[test]
    fn identifier_picks_cli_archive_on_apple_arm64() {
        assert_picks_cli("aarch64-apple-darwin");
    }

    #[test]
    fn identifier_picks_cli_archive_on_apple_x64() {
        assert_picks_cli("x86_64-apple-darwin");
    }
}
