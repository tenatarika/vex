//! `vex self-update` — pull a newer release binary from GitHub.
//! Extracted from `cli/mod.rs` in S1 Group B together with its embedded
//! ed25519 release pubkey and the compile-time length assertion (S5).

use std::path::{Path, PathBuf};

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

/// The binary name, used for the release-asset identifier, the
/// archive-entry match, and the final swap. A literal, NOT
/// `env!("CARGO_PKG_NAME")`: the crates.io package is `vex-search` (the
/// `vex` name is taken there) while the binary and the release assets
/// (`vex-<target>.tar.gz`, literals in release.yml) stay `vex`. Deriving
/// it from the package name would make self-update look for
/// `vex-search-<target>` and miss every asset. `CARGO_BIN_NAME` is not an
/// option either — this module is also compiled into the lib target,
/// where it is unset. The `bin_name_matches_release_asset_contract` test
/// pins the value. (The GitHub `repo_name` below stays a literal too.)
const BIN_NAME: &str = "vex";

/// Update the running binary from the latest GitHub release. The
/// self_update crate handles platform detection (target triple) and the
/// release/asset lookup; the apply path lives in `self_update_flow`, which
/// downloads + verifies the archive once and installs EVERY file in it —
/// the bundled DirectML.dll sidecar included, which the crate's built-in
/// `update()` silently dropped (it extracts only the named binary).
pub(crate) fn cmd_self_update(check_only: bool, no_confirm: bool) -> Result<()> {
    let bundle = current_mcpb_bundle();
    if !check_only {
        if let Some(bundle) = &bundle {
            anyhow::bail!("{}", bundle.refusal());
        }
    }
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
    let identifier = format!("{BIN_NAME}-{target}");
    let status = self_update::backends::github::Update::configure()
        .repo_owner("tenatarika")
        .repo_name("vex")
        .bin_name(BIN_NAME)
        .identifier(&identifier)
        .current_version(current)
        .show_download_progress(true)
        .no_confirm(no_confirm)
        .verifying_keys([pubkey])
        .build()
        .context("configure self-update client")?;

    // ONE release fetch and ONE version gate, shared by --check and apply —
    // the two paths previously fetched and compared independently, which
    // invited them to drift apart under refactoring.
    //
    // DELIBERATE semantics change vs `status.update()`: the gate is
    // `get_latest_release()` + `bump_is_greater` (strictly newer, majors
    // included, prereleases excluded by GitHub's /releases/latest), where
    // the crate used `get_latest_releases()` + `bump_is_compatible` (major-
    // pinned, prereleases visible). For a single-binary CLI, stopping at a
    // major boundary just strands users on an unmaintained line — and
    // `--check` has always used exactly these newer semantics, so check and
    // apply agree. Downgrades remain impossible either way (`latest <
    // current` never passes the gate). Major-version crossings warn loudly
    // below, including under --no-confirm and --check.
    let release = status
        .get_latest_release()
        .context("fetch latest release from GitHub (offline or rate-limited?)")?;
    let latest = release.version.as_str();
    let decision = update_decision(current, latest)?;

    if check_only {
        match decision {
            UpdateDecision::UpToDate => println!("vex is up to date ({current})."),
            UpdateDecision::Newer => {
                warn_if_crossing_major(current, latest);
                println!(
                    "Update available: {current} → {latest} ({}).\nRun `vex self-update` (omit --check) to install.",
                    release.name
                );
            }
            UpdateDecision::LocalAhead => {
                // Local build ahead of GitHub (e.g. a dev branch). Don't
                // pretend an update is needed — just report what's out there.
                println!(
                    "Latest release: {latest} (current: {current} — newer, no action needed)."
                );
            }
        }
        if let Some(bundle) = &bundle {
            println!("{}", bundle.check_note());
        }
        return Ok(());
    }

    // Apply path. `status.update()` is deliberately NOT used: it extracts
    // only the named binary from the archive, so Windows self-updates
    // dropped the DirectML.dll sidecar and degraded GPU embedding to CPU
    // until the next manual reinstall. The custom flow keeps the crate's
    // confirmation UX, then installs the whole archive.
    if decision != UpdateDecision::Newer {
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

    warn_if_crossing_major(current, latest);

    if !no_confirm {
        println!("\nvex release status:");
        println!("  * Current version: {current}");
        println!("  * New release: {} ({})", latest, asset.name);
        println!("\nThe new release will be downloaded/extracted and the existing binary will be replaced.");
        self_update_flow::confirm("Do you want to continue? [Y/n] ")?;
    }

    self_update_flow::apply_update(&asset.name, &asset.download_url, pubkey, BIN_NAME)
        .context("apply self-update")?;
    println!("Updated to vex {latest}. Restart any open shells.");
    Ok(())
}

/// Upper bound on the `manifest.json` size [`is_vex_mcpb_manifest`] will
/// read. Real MCPB manifests are a few KiB; anything bigger is not ours.
const MCPB_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

/// A running `vex` that ships inside an unpacked MCP Bundle (.mcpb).
///
/// The host app (Claude Desktop & co.) owns that install: it unpacked a
/// bundle whose manifest and registry-pinned SHA-256 describe exactly these
/// binaries, and it updates by replacing the whole bundle. Swapping `vex` in
/// place would leave `vex-mcp` and the manifest on the old version and be
/// silently undone by the host's next bundle update — so the apply path
/// refuses, and `--check` adds a note.
#[derive(Debug)]
struct McpbInstall {
    exe: PathBuf,
    manifest: PathBuf,
}

impl McpbInstall {
    fn refusal(&self) -> String {
        format!(
            "this vex ({}) is part of an MCP Bundle (manifest: {}); its host app manages \
             updates. Update the vex bundle from the host app instead, or install the \
             standalone CLI (brew, cargo, or the GitHub release tarball) to use `vex self-update`.",
            self.exe.display(),
            self.manifest.display()
        )
    }

    fn check_note(&self) -> String {
        format!(
            "Note: this vex is part of an MCP Bundle (manifest: {}); updates come from \
             the host app that installed it, not from `vex self-update`.",
            self.manifest.display()
        )
    }
}

/// The MCPB install the running exe belongs to, if any. Best-effort: when
/// the exe path can't be determined the answer is "not a bundle" — the
/// apply path resolves `current_exe` again and fails loudly there.
fn current_mcpb_bundle() -> Option<McpbInstall> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let manifest = mcpb_bundle_manifest(&exe)?;
    Some(McpbInstall { exe, manifest })
}

/// The MCPB manifest that owns `exe`, if any. A vex bundle lays out
/// `<root>/manifest.json` + `<root>/server/vex` (packaging/mcpb), so the
/// manifest is looked for in the exe's parent's parent, and — in case a
/// host flattens the layout — in the exe's own directory.
fn mcpb_bundle_manifest(exe: &Path) -> Option<PathBuf> {
    let exe_dir = exe.parent()?;
    [Some(exe_dir), exe_dir.parent()]
        .into_iter()
        .flatten()
        .map(|dir| dir.join("manifest.json"))
        .find(|manifest| is_vex_mcpb_manifest(manifest))
}

/// True when `path` is an MCPB manifest for vex: `name == "vex"` and
/// `server.type == "binary"`. Any read or parse problem reads as "not a
/// bundle" — the guard must never block a plain install because of an
/// unrelated `manifest.json` sitting nearby.
fn is_vex_mcpb_manifest(path: &Path) -> bool {
    let small_file = std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() <= MCPB_MANIFEST_MAX_BYTES)
        .unwrap_or(false);
    if !small_file {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    json.get("name").and_then(|v| v.as_str()) == Some("vex")
        && json.pointer("/server/type").and_then(|v| v.as_str()) == Some("binary")
}

/// Outcome of the shared `--check`/apply version gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateDecision {
    /// `latest == current` — nothing to do.
    UpToDate,
    /// Strictly newer than current: majors included, prereleases never
    /// reach here (GitHub's `/releases/latest` excludes them).
    Newer,
    /// Local build ahead of the newest GitHub release (e.g. a dev branch).
    LocalAhead,
}

/// Pure decision behind the version gate, extracted from the
/// network-coupled `cmd_self_update` so the direction logic is
/// unit-testable — an argument swap in `bump_is_greater` (gating
/// DOWNGRADES instead of upgrades) would otherwise survive the whole
/// suite. Errors when the versions don't compare as semver: surfaced
/// rather than silently mis-reporting direction, since a release tagged
/// with an unexpected prefix would otherwise read as "no action needed".
fn update_decision(current: &str, latest: &str) -> Result<UpdateDecision> {
    if latest == current {
        return Ok(UpdateDecision::UpToDate);
    }
    let newer = self_update::version::bump_is_greater(current, latest)
        .with_context(|| format!("could not compare versions {current:?} and {latest:?}"))?;
    Ok(if newer {
        UpdateDecision::Newer
    } else {
        UpdateDecision::LocalAhead
    })
}

/// True when `current → latest` crosses a major version boundary.
/// Unparseable inputs read as "not crossing" (stay quiet); both strings
/// are already semver-validated by [`update_decision`] on every reachable
/// path.
fn crosses_major(current: &str, latest: &str) -> bool {
    match (semver_major(current), semver_major(latest)) {
        (Some(cur), Some(new)) => cur != new,
        _ => false,
    }
}

/// A major-version crossing means likely breaking changes — warn loudly on
/// stderr. Fires on the apply path in BOTH interactive and `--no-confirm`
/// modes (the prompt lets an interactive user bail, but scripted CI must
/// not silently land on a new major) and on `--check`, so the preview
/// carries the same signal as the apply.
fn warn_if_crossing_major(current: &str, latest: &str) {
    if crosses_major(current, latest) {
        eprintln!(
            "WARNING: {current} → {latest} crosses a major version boundary — expect \
             breaking changes; review the release notes before relying on scripted updates."
        );
    }
}

/// First numeric component of a version string (`"1.16.0"` → `1`,
/// `"v2.0.0-rc.1"` → `2`). `None` when unparseable — callers treat that as
/// "can't tell, stay quiet"; the version gate has already validated both
/// strings via `bump_is_greater` before this is consulted.
fn semver_major(version: &str) -> Option<u64> {
    version
        .trim_start_matches(['v', 'V'])
        .split(['.', '-', '+'])
        .next()?
        .parse()
        .ok()
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

    #[test]
    fn semver_major_parses_plain_prefixed_and_prerelease_versions() {
        assert_eq!(super::semver_major("1.16.0"), Some(1));
        assert_eq!(super::semver_major("v2.0.0"), Some(2));
        assert_eq!(super::semver_major("10.1.3-rc.1"), Some(10));
        assert_eq!(super::semver_major("2-beta"), Some(2));
        assert_eq!(super::semver_major("garbage"), None);
        assert_eq!(super::semver_major(""), None);
    }

    #[test]
    fn update_decision_classifies_all_directions() {
        use super::UpdateDecision::*;
        // The gate that protects users from downgrades and spurious
        // updates — pinned so an argument swap in `bump_is_greater`
        // (which would gate downgrades instead of upgrades) fails here.
        assert_eq!(
            super::update_decision("1.16.0", "1.16.0").unwrap(),
            UpToDate
        );
        assert_eq!(super::update_decision("1.16.0", "1.16.1").unwrap(), Newer);
        assert_eq!(super::update_decision("1.16.0", "1.17.0").unwrap(), Newer);
        assert_eq!(
            super::update_decision("1.16.0", "2.0.0").unwrap(),
            Newer,
            "majors are included (deliberate bump_is_greater semantics)"
        );
        assert_eq!(
            super::update_decision("1.16.0", "1.15.9").unwrap(),
            LocalAhead,
            "downgrades never pass the gate"
        );
        assert_eq!(
            super::update_decision("2.0.0", "1.99.9").unwrap(),
            LocalAhead
        );
        assert!(super::update_decision("1.16.0", "not-semver").is_err());
    }

    #[test]
    fn crosses_major_detects_boundary_only() {
        assert!(super::crosses_major("1.16.0", "2.0.0"));
        assert!(super::crosses_major("1.16.0", "10.0.0"));
        assert!(!super::crosses_major("1.16.0", "1.17.0"));
        assert!(!super::crosses_major("1.16.0", "1.16.1"));
        assert!(
            !super::crosses_major("garbage", "2.0.0"),
            "unparseable input stays quiet rather than warning spuriously"
        );
    }

    /// Every target the release matrix (and its MCP bundle step) ships.
    const RELEASE_TARGETS: [&str; 3] = [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ];

    /// A v1.27.2-shaped release: CLI + MCP tarballs AND the MCP bundles
    /// for every target, in GitHub's alphabetical listing order.
    fn release_with_mcpb_assets() -> Release {
        let mut names: Vec<String> = RELEASE_TARGETS
            .iter()
            .flat_map(|t| {
                [
                    format!("vex-{t}.tar.gz"),
                    format!("vex-mcp-{t}.tar.gz"),
                    format!("vex-mcp-{t}.mcpb"),
                ]
            })
            .collect();
        names.sort();
        Release {
            name: "vex 1.27.2".to_string(),
            version: "1.27.2".to_string(),
            date: "2026-10-03".to_string(),
            body: None,
            assets: names
                .into_iter()
                .map(|name| ReleaseAsset {
                    download_url: format!("https://example.test/{name}"),
                    name,
                })
                .collect(),
        }
    }

    #[test]
    fn mcpb_bundle_never_matches_self_update_identifier() {
        // release.yml names bundles `vex-mcp-<target>.mcpb` precisely so
        // that the `vex-<target>` identifier (a substring match) can never
        // select one. `vex-<target>.mcpb` WOULD match — this pins the name.
        let release = release_with_mcpb_assets();
        for target in RELEASE_TARGETS {
            let identifier = format!("{}-{target}", super::BIN_NAME);
            for other in RELEASE_TARGETS {
                let bundle = format!("vex-mcp-{other}.mcpb");
                assert!(
                    !bundle.contains(&identifier),
                    "{bundle} must not match self-update identifier {identifier}"
                );
            }
            let picked = release
                .asset_for(target, Some(&identifier))
                .expect("CLI archive must still be selectable");
            assert_eq!(picked.name, format!("vex-{target}.tar.gz"));
        }
    }

    fn write_manifest(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("manifest.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    const VEX_MANIFEST: &str = r#"{"manifest_version":"0.3","name":"vex","version":"1.27.2",
        "server":{"type":"binary","entry_point":"server/vex-mcp"}}"#;

    /// `<root>/manifest.json` + `<root>/server/vex` — the packaging/mcpb layout.
    fn bundle_layout(manifest: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("server")).unwrap();
        write_manifest(root.path(), manifest);
        let exe = root.path().join("server").join("vex");
        std::fs::write(&exe, b"").unwrap();
        (root, exe)
    }

    #[test]
    fn mcpb_guard_detects_bundle_layout() {
        let (root, exe) = bundle_layout(VEX_MANIFEST);
        assert_eq!(
            super::mcpb_bundle_manifest(&exe),
            Some(root.path().join("manifest.json"))
        );
    }

    #[test]
    fn mcpb_guard_detects_flattened_layout() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_manifest(dir.path(), VEX_MANIFEST);
        let exe = dir.path().join("vex");
        assert_eq!(super::mcpb_bundle_manifest(&exe), Some(manifest));
    }

    #[test]
    fn mcpb_guard_ignores_plain_installs_and_foreign_manifests() {
        // No manifest at all (brew / cargo / tarball install).
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(super::mcpb_bundle_manifest(&dir.path().join("vex")), None);

        for foreign in [
            // Another bundle's manifest.
            r#"{"name":"other","server":{"type":"binary"}}"#,
            // A vex manifest that is not a binary server.
            r#"{"name":"vex","server":{"type":"node"}}"#,
            // Some unrelated project manifest.
            r#"{"name":"vex","version":"1.0.0"}"#,
            // Not JSON.
            "not json",
        ] {
            let (_root, exe) = bundle_layout(foreign);
            assert_eq!(
                super::mcpb_bundle_manifest(&exe),
                None,
                "must not treat {foreign:?} as a vex bundle"
            );
        }
    }

    #[test]
    fn mcpb_messages_name_the_manifest_and_the_host() {
        let install = super::McpbInstall {
            exe: "/b/server/vex".into(),
            manifest: "/b/manifest.json".into(),
        };
        let refusal = install.refusal();
        assert!(refusal.contains("/b/manifest.json") && refusal.contains("host app"));
        let note = install.check_note();
        assert!(note.contains("/b/manifest.json") && note.contains("host app"));
    }

    #[test]
    fn mcpb_guard_ignores_oversized_manifest() {
        let mut big = String::from(VEX_MANIFEST);
        big.push_str(&" ".repeat(super::MCPB_MANIFEST_MAX_BYTES as usize));
        let (_root, exe) = bundle_layout(&big);
        assert_eq!(super::mcpb_bundle_manifest(&exe), None);
    }

    #[test]
    fn bin_name_matches_release_asset_contract() {
        // release.yml packs assets under the LITERAL name
        // `vex-<target>.tar.gz` — it is not derived from the package name
        // (which is `vex-search` on crates.io). The installed binary must
        // carry the same name (`[[bin]] name = "vex"` in Cargo.toml).
        assert_eq!(super::BIN_NAME, "vex");
    }
}
