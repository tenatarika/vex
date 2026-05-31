//! `vex self-update` — pull a newer release binary from GitHub.
//! Extracted from `cli/mod.rs` in S1 Group B together with its embedded
//! ed25519 release pubkey and the compile-time length assertion (S5).

use anyhow::{Context, Result};

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
/// self_update crate handles platform detection (target triple), archive
/// download, ed25519 signature verification, atomic file replacement,
/// and Windows-specific in-use-binary swap via a temp rename.
pub(crate) fn cmd_self_update(check_only: bool, no_confirm: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    // SAFETY of the `try_into` below: the byte count is asserted at
    // compile time by the `const _: () = assert!(...)` above. The
    // `unwrap()` here is provably unreachable — if the slice were the
    // wrong length, the crate wouldn't compile.
    let pubkey: [u8; 32] = VEX_RELEASE_PUBKEY
        .try_into()
        .expect("checked at compile time");
    let status = self_update::backends::github::Update::configure()
        .repo_owner("tenatarika")
        .repo_name("vex")
        .bin_name("vex")
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

    let result = status.update().context("apply self-update")?;
    match result {
        self_update::Status::UpToDate(v) => println!("vex is already up to date ({v})."),
        self_update::Status::Updated(v) => println!("Updated to vex {v}. Restart any open shells."),
    }
    Ok(())
}
