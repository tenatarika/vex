//! Custom apply flow for `vex self-update` — download once, verify the
//! zipsign signature, extract the WHOLE archive, install sidecar files
//! (DirectML.dll on Windows) beside the exe, then swap the binary last.
//!
//! Why not `self_update::Status::update()`: its internal flow extracts only
//! the named binary from the release archive (`Extract::extract_file`), so
//! every Windows self-update since v1.13 dropped the bundled DirectML.dll
//! and silently degraded GPU embedding to CPU. This module rebuilds the
//! apply path from the crate's public building blocks (`Download`,
//! `Extract::extract_into`, re-exported `self_replace`/`TempDir`) plus a
//! replicated ~20-line signature check, keeping a single download and a
//! single ed25519 verification for *all* files in the archive.
//!
//! Failure ordering: sidecars install BEFORE the exe swap. If a sidecar
//! write fails (e.g. unelevated under `C:\Program Files\vex\`), the update
//! aborts with the old exe + DLL pair intact — never exe↔DLL version skew.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

/// Outcome of the hash-compare gate for one sidecar file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidecarAction {
    /// Destination already has the identical bytes — leave it alone. The
    /// common case: DirectML.dll only changes when `ort` bumps its pin.
    Skip,
    /// Destination is missing (heals installs degraded by the old updater)
    /// or differs — install the new copy.
    Install,
}

/// Pure decision: skip a sidecar only when the destination exists AND is
/// byte-identical. `dest_hash = None` means the file is absent.
pub(crate) fn sidecar_action(src_hash: &[u8; 32], dest_hash: Option<&[u8; 32]>) -> SidecarAction {
    match dest_hash {
        Some(dest) if dest == src_hash => SidecarAction::Skip,
        _ => SidecarAction::Install,
    }
}

/// Pure: split extracted archive entries into the binary and its sidecars.
/// Matches `bin_name` and `bin_name.exe` case-insensitively (Windows
/// filesystems are case-preserving but insensitive). Everything else is a
/// sidecar — generic on purpose, so a future second redist ships without
/// touching this code. On Linux/macOS the archive holds only `vex`, the
/// sidecar list is empty, and the flow degrades to exactly today's behavior.
///
/// Errors on a SECOND binary-named entry (e.g. both `vex` and `vex.exe`):
/// silently reclassifying one as a sidecar would install a stray binary
/// next to the exe. Our release pipeline never produces this, so a
/// duplicate means a malformed archive — fail loudly instead.
pub(crate) fn classify_entries(
    extracted: &[PathBuf],
    bin_name: &str,
) -> Result<(Option<PathBuf>, Vec<PathBuf>)> {
    let bin_lower = bin_name.to_lowercase();
    let exe_lower = format!("{bin_lower}.exe");
    let mut binary: Option<PathBuf> = None;
    let mut sidecars = Vec::new();
    for path in extracted {
        let name_lower = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_lowercase);
        match name_lower {
            Some(n) if n == bin_lower || n == exe_lower => {
                if let Some(first) = &binary {
                    bail!(
                        "release archive contains more than one `{bin_name}` binary entry \
                         ({} and {}) — refusing to guess which to install",
                        first.display(),
                        path.display()
                    );
                }
                binary = Some(path.clone());
            }
            Some(_) => sidecars.push(path.clone()),
            None => {} // non-UTF-8 name — never produced by our release pipeline
        }
    }
    Ok((binary, sidecars))
}

/// SHA-256 of a file's contents, streamed in 64 KiB chunks (DirectML.dll
/// is ~18 MB — don't buffer it whole).
pub(crate) fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let file =
        fs::File::open(path).with_context(|| format!("open {} for hashing", path.display()))?;
    sha256_stream(file, path)
}

/// SHA-256 of a file that may legitimately be absent (the heal path: a
/// destination DLL missing after an old binary-only self-update). A single
/// `open` distinguishes missing from other errors — no separate `exists()`
/// probe, so there is no stat→open window for a concurrent process to race
/// and one fewer syscall on the path.
pub(crate) fn sha256_file_if_exists(path: &Path) -> Result<Option<[u8; 32]>> {
    match fs::File::open(path) {
        Ok(file) => sha256_stream(file, path).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!(e).context(format!("open {} for hashing", path.display()))),
    }
}

fn sha256_stream(mut file: fs::File, path: &Path) -> Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {} for hashing", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Verify the zipsign ed25519 signature embedded in a downloaded release
/// archive. Replicates `self_update`'s private `verify_signature` for the
/// tar.gz case (the only format vex ships — see release.yml): the signing
/// CONTEXT is the archive's file name, so `archive_path` MUST be named
/// exactly like the GitHub release asset or verification fails.
pub(crate) fn verify_signature(
    archive_path: &Path,
    keys: &[[u8; zipsign_api::PUBLIC_KEY_LENGTH]],
) -> Result<()> {
    let context = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::as_bytes)
        .ok_or_else(|| anyhow!("archive path has no UTF-8 file name: {archive_path:?}"))?;
    let keys = zipsign_api::verify::collect_keys(keys.iter().copied().map(Ok))
        .context("collect release verifying keys")?;
    let mut archive = fs::File::open(archive_path)
        .with_context(|| format!("open downloaded archive {}", archive_path.display()))?;
    zipsign_api::verify::verify_tar(&mut archive, &keys, Some(context))
        .context("verify release archive signature (zipsign ed25519)")?;
    Ok(())
}

/// Pure decision behind [`confirm`]: blank, `y`, or `yes` (any case)
/// proceeds, anything else aborts. Superset of `self_update`'s private
/// `confirm` (bare `y` only) — the prompt reads `[Y/n]` and people type
/// the full word, which must not read as an abort.
fn confirm_accepts(answer: &str) -> bool {
    let answer = answer.trim().to_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

/// Replicates `self_update`'s private `confirm`: the user's last gate
/// before the binary swap.
pub(crate) fn confirm(prompt: &str) -> Result<()> {
    print!("{prompt}");
    std::io::stdout().flush().context("flush confirm prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("read confirmation")?;
    if !confirm_accepts(&answer) {
        bail!("update aborted");
    }
    Ok(())
}

/// Install one sidecar next to the exe.
///
/// NOT `self_update::Move`: its `to_dest` is a bare `fs::rename`, which
/// fails when source (the OS temp dir, usually `C:`) and destination (the
/// install dir, possibly another volume) differ. Instead:
///   1. rename any existing destination aside WITHIN the same directory —
///      same-volume rename succeeds even while the DLL is mapped into
///      another running vex process (the same trick `self_replace` uses
///      for the exe itself);
///   2. the new file is STAGED into the destination directory first
///      (`<name>.self-update.new`, via copy — copy works across volumes)
///      and re-hashed against `src_hash` — `fs::copy` can report success
///      after a short write on some network filesystems, and the staged
///      copy is what actually lands beside the exe — and only then renamed
///      into place — same-volume, atomic, so `dest` is never observable in
///      a half-written state (a partial DLL beside the exe would fail
///      `LoadLibrary` harder than the missing-DLL case this module exists
///      to fix);
///   3. best-effort delete of the set-aside copy — if a process still maps
///      it the delete fails and the `.old` file is reaped on a later run.
///
/// Failure handling: a failed or corrupt staging copy aborts before `dest`
/// is ever touched; a failed final rename restores the set-aside original.
/// The install dir is never left without a working sidecar.
fn install_sidecar(src: &Path, dest: &Path, src_hash: &[u8; 32]) -> Result<()> {
    let dest_dir = dest
        .parent()
        .ok_or_else(|| anyhow!("sidecar destination {} has no parent dir", dest.display()))?;
    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("sidecar destination {} has no file name", dest.display()))?;
    let staged = dest_dir.join(format!("{file_name}.self-update.new"));
    let set_aside = dest_dir.join(format!("{file_name}.self-update.old"));

    // Reap leftovers from a previous update (a mapped DLL blocks the .old
    // delete; an interrupted run can leave a .new). Ignore failures — they
    // just stay for the next round.
    if set_aside.exists() {
        let _ = fs::remove_file(&set_aside);
    }
    if staged.exists() {
        let _ = fs::remove_file(&staged);
    }

    // Stage first: the cross-volume copy happens BEFORE the existing dest
    // is disturbed, so a permission error or short write aborts cleanly.
    if let Err(e) = fs::copy(src, &staged) {
        let _ = fs::remove_file(&staged);
        return Err(elevation_hint(e, dest));
    }

    // The staged copy is what actually lands beside the exe — verify it
    // against the source hash before the existing dest is disturbed.
    match sha256_file(&staged) {
        Ok(h) if h == *src_hash => {}
        Ok(_) => {
            let _ = fs::remove_file(&staged);
            bail!(
                "staged copy of {} is corrupt (hash mismatch after copy) — \
                 the existing install was left untouched; re-run `vex self-update`",
                dest.display()
            );
        }
        Err(e) => {
            let _ = fs::remove_file(&staged);
            return Err(e);
        }
    }

    let had_existing = dest.exists();
    if had_existing {
        if let Err(e) = fs::rename(dest, &set_aside) {
            let _ = fs::remove_file(&staged);
            return Err(elevation_hint(e, dest));
        }
    }
    if let Err(e) = fs::rename(&staged, dest) {
        if had_existing {
            // Roll the old file back so the install keeps working.
            let _ = fs::rename(&set_aside, dest);
        }
        let _ = fs::remove_file(&staged);
        return Err(elevation_hint(e, dest));
    }
    if had_existing {
        let _ = fs::remove_file(&set_aside);
    }
    Ok(())
}

/// Wrap a filesystem error on the install dir with an actionable message —
/// the common cause is vex installed under a write-protected directory
/// (e.g. `C:\Program Files\vex\`) and the shell not being elevated.
fn elevation_hint(e: std::io::Error, dest: &Path) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow!(e).context(format!(
            "cannot write {} — vex appears to be installed in a protected directory; \
             re-run `vex self-update` from an elevated shell",
            dest.display()
        ))
    } else {
        anyhow!(e).context(format!("install sidecar {}", dest.display()))
    }
}

/// List the files `Extract::extract_into` produced in the temp dir,
/// excluding the downloaded archive itself.
///
/// Release-contract guard: vex archives are FLAT (`tar czf … vex.exe
/// DirectML.dll` in release.yml). This scan is deliberately non-recursive,
/// so a directory entry means the archive shape changed — fail loudly
/// rather than silently dropping whatever is nested inside (a sidecar that
/// never reaches the install dir would reintroduce the exact degraded-GPU
/// state this module exists to fix). Entries are judged by their own file
/// type (`DirEntry::file_type`, which does NOT follow symlinks): the
/// contract is about what the archive contained, not what a link points
/// at, so symlinks and other non-regular entries are rejected too.
///
/// Invariant: `archive_name` must be the exact file name the downloaded
/// archive was written under inside `tmp_dir` — the string comparison
/// below is the only thing excluding the archive from the result, and a
/// mismatched name would classify the archive itself as a sidecar.
/// [`apply_update`] guarantees this by using one variable for both.
fn extracted_files(tmp_dir: &Path, archive_name: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(tmp_dir).context("list extracted archive contents")? {
        let entry = entry.context("read extracted dir entry")?;
        let path = entry.path();
        let file_type = entry.file_type().context("stat extracted dir entry")?;
        if file_type.is_dir() {
            bail!(
                "release archive {archive_name} unexpectedly contains a directory ({}) — \
                 vex release archives are flat; update self_update_flow.rs if the release \
                 layout changed",
                path.display()
            );
        }
        if !file_type.is_file() {
            bail!(
                "release archive {archive_name} unexpectedly contains a non-regular entry \
                 ({}) — vex release archives contain only plain files",
                path.display()
            );
        }
        if entry.file_name().to_string_lossy() != archive_name {
            files.push(path);
        }
    }
    files.sort(); // deterministic install order (and stable test output)
    Ok(files)
}

/// Install every sidecar (hash-gated) into `install_dir`, then return the
/// extracted binary's path for the final swap. Split from [`apply_update`]
/// so the whole post-download pipeline is testable without network or
/// `self_replace`.
///
/// Integrity boundary: the archive-level zipsign ed25519 signature
/// (verified before extraction) covers every byte of every file installed
/// here. The per-file SHA-256s below are NOT a second trust check — they
/// are a skip-gate (don't rewrite a byte-identical DLL) plus a staging
/// copy-integrity re-check inside [`install_sidecar`]. Don't add a pinned
/// per-file hash here (it would drift on every ort bump), and don't remove
/// the archive signature check thinking these hashes replace it.
pub(crate) fn install_from_extracted(
    tmp_dir: &Path,
    archive_name: &str,
    bin_name: &str,
    install_dir: &Path,
) -> Result<PathBuf> {
    let files = extracted_files(tmp_dir, archive_name)?;
    let (binary, sidecars) = classify_entries(&files, bin_name)?;
    let binary = binary.ok_or_else(|| {
        anyhow!("release archive {archive_name} did not contain the `{bin_name}` binary")
    })?;

    for sidecar in &sidecars {
        let name = sidecar
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("sidecar {} has no UTF-8 file name", sidecar.display()))?;
        let dest = install_dir.join(name);
        let src_hash = sha256_file(sidecar)?;
        let dest_hash = sha256_file_if_exists(&dest)?;
        match sidecar_action(&src_hash, dest_hash.as_ref()) {
            SidecarAction::Skip => println!("Sidecar {name} is up to date — skipping."),
            SidecarAction::Install => {
                install_sidecar(sidecar, &dest, &src_hash)?;
                println!("Installed sidecar {name}.");
            }
        }
    }
    Ok(binary)
}

/// The apply pipeline: download the release asset, verify its signature,
/// extract everything, install sidecars beside the running exe, and swap
/// the binary LAST. The caller has already compared versions and asked the
/// user for confirmation.
pub(crate) fn apply_update(
    asset_name: &str,
    download_url: &str,
    pubkey: [u8; 32],
    bin_name: &str,
) -> Result<()> {
    // GitHub asset names can't contain path separators, but the archive
    // name becomes a temp-dir path component below — fail closed anyway.
    if asset_name.contains(['/', '\\']) {
        bail!("refusing suspicious release asset name: {asset_name:?}");
    }

    let tmp_dir = self_update::TempDir::new().context("create temp dir for self-update")?;
    // MUST be named exactly like the asset: the zipsign verification
    // context is the file name (see `verify_signature`).
    let tmp_archive_path = tmp_dir.path().join(asset_name);
    let mut tmp_archive = fs::File::create(&tmp_archive_path)
        .with_context(|| format!("create {}", tmp_archive_path.display()))?;

    println!("Downloading...");
    let mut download = self_update::Download::from_url(download_url);
    // The asset URL is the GitHub API endpoint — without this Accept header
    // it returns JSON metadata instead of the archive bytes. (`download_to`
    // supplies a default User-Agent on its own.)
    download.set_header(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/octet-stream"),
    );
    download.show_progress(true);
    download
        .download_to(&mut tmp_archive)
        .context("download release archive")?;
    drop(tmp_archive); // close before reopening for verification on Windows

    println!("Verifying downloaded archive...");
    verify_signature(&tmp_archive_path, &[pubkey])?;

    println!("Extracting archive...");
    self_update::Extract::from_source(&tmp_archive_path)
        .extract_into(tmp_dir.path())
        .context("extract release archive")?;

    // `current_exe()` "may or may not" follow symlinks; canonicalize the exe
    // path (not just its parent — that wouldn't resolve a symlinked FILE like
    // `~/.local/bin/vex` → `/opt/vex/vex`) so sidecars land beside the REAL
    // binary, where the Windows loader actually resolves DLLs. Best-effort:
    // on canonicalization failure fall back to the reported path.
    let current_exe = std::env::current_exe().context("locate current vex binary")?;
    let current_exe = fs::canonicalize(&current_exe).unwrap_or(current_exe);
    let install_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("current exe {} has no parent dir", current_exe.display()))?;

    // Sidecars first — a failure here aborts BEFORE the exe is touched, so
    // the old exe + DLL pair stays intact (no version skew). `asset_name`
    // doubles as the exclude filter because the archive was written under
    // exactly that name above — keep the two in sync.
    let new_binary = install_from_extracted(tmp_dir.path(), asset_name, bin_name, install_dir)?;

    println!("Replacing binary...");
    self_update::self_replace::self_replace(&new_binary).context("replace vex binary")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ---- pure logic -----------------------------------------------------

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn classify_entries_splits_binary_and_dll_on_windows() {
        let (binary, sidecars) =
            classify_entries(&paths(&["DirectML.dll", "vex.exe"]), "vex").unwrap();
        assert_eq!(binary, Some(PathBuf::from("vex.exe")));
        assert_eq!(sidecars, paths(&["DirectML.dll"]));
    }

    #[test]
    fn classify_entries_no_sidecars_on_unix() {
        let (binary, sidecars) = classify_entries(&paths(&["vex"]), "vex").unwrap();
        assert_eq!(binary, Some(PathBuf::from("vex")));
        assert!(sidecars.is_empty());
    }

    #[test]
    fn classify_entries_rejects_duplicate_binary_entries() {
        // A malformed archive with both `vex` and `vex.exe` must fail
        // loudly, not silently install one of them as a sidecar.
        let err = classify_entries(&paths(&["vex", "vex.exe"]), "vex").unwrap_err();
        assert!(err.to_string().contains("more than one"), "{err}");
    }

    #[test]
    fn classify_entries_is_case_insensitive() {
        let (binary, sidecars) = classify_entries(&paths(&["VEX.EXE"]), "vex").unwrap();
        assert_eq!(binary, Some(PathBuf::from("VEX.EXE")));
        assert!(sidecars.is_empty());
    }

    #[test]
    fn classify_entries_reports_missing_binary() {
        let (binary, sidecars) = classify_entries(&paths(&["DirectML.dll"]), "vex").unwrap();
        assert_eq!(binary, None);
        assert_eq!(sidecars, paths(&["DirectML.dll"]));
    }

    #[test]
    fn confirm_accepts_blank_y_and_yes() {
        assert!(confirm_accepts(""));
        assert!(confirm_accepts("\n"));
        assert!(confirm_accepts("y\n"));
        assert!(confirm_accepts("Y\n"));
        assert!(confirm_accepts("yes\n")); // people type the word against [Y/n]
        assert!(confirm_accepts("YES\n"));
        // Windows console read_line delivers CRLF — the primary platform of
        // this whole fix. Production strips it via trim(); pin that, so a
        // refactor to e.g. trim_end_matches('\n') can't silently turn every
        // Windows interactive confirm into "update aborted".
        assert!(confirm_accepts("y\r\n"));
        assert!(confirm_accepts("yes\r\n"));
        assert!(!confirm_accepts("n\n"));
        assert!(!confirm_accepts("no\n"));
        assert!(!confirm_accepts("ye\n"));
        assert!(!confirm_accepts("q\n"));
    }

    #[test]
    fn elevation_hint_mentions_elevated_shell_on_permission_denied() {
        let err = elevation_hint(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            Path::new("C:/Program Files/vex/DirectML.dll"),
        );
        assert!(format!("{err:#}").contains("elevated shell"), "{err:#}");
    }

    #[test]
    fn elevation_hint_stays_generic_on_other_errors() {
        let err = elevation_hint(
            std::io::Error::from(std::io::ErrorKind::NotFound),
            Path::new("DirectML.dll"),
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("install sidecar"), "{msg}");
        assert!(!msg.contains("elevated shell"), "{msg}");
    }

    #[test]
    fn install_sidecar_failed_staging_leaves_dest_untouched() {
        // A staging-copy failure (src missing here; disk-full/EPERM in the
        // field) must abort BEFORE the existing dest is disturbed and must
        // not litter the install dir with .new/.old files.
        let install = tempfile::tempdir().unwrap();
        let dest = install.path().join("DirectML.dll");
        fs::write(&dest, b"old working dll").unwrap();

        let missing_src = install.path().join("nonexistent-src");
        let err = install_sidecar(&missing_src, &dest, &[0u8; 32]).unwrap_err();

        assert!(format!("{err:#}").contains("install sidecar"), "{err:#}");
        assert_eq!(fs::read(&dest).unwrap(), b"old working dll");
        assert_eq!(
            fs::read_dir(install.path()).unwrap().count(),
            1,
            "no .self-update.new/.old litter may remain"
        );
    }

    #[test]
    fn install_sidecar_rejects_corrupt_staged_copy() {
        // A short/corrupt staging copy (simulated here by passing a wrong
        // expected hash; in the field: `fs::copy` reporting success after a
        // partial flush on a network filesystem) must abort with the
        // existing dest untouched and no litter.
        let install = tempfile::tempdir().unwrap();
        let dest = install.path().join("DirectML.dll");
        fs::write(&dest, b"old working dll").unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("DirectML.dll");
        fs::write(&src, b"new dll bytes").unwrap();

        let err = install_sidecar(&src, &dest, &[0u8; 32]).unwrap_err();

        assert!(format!("{err:#}").contains("hash mismatch"), "{err:#}");
        assert_eq!(fs::read(&dest).unwrap(), b"old working dll");
        assert_eq!(
            fs::read_dir(install.path()).unwrap().count(),
            1,
            "no .self-update.new/.old litter may remain"
        );
    }

    #[test]
    fn sidecar_action_skips_when_hashes_match() {
        let h = [7u8; 32];
        assert_eq!(sidecar_action(&h, Some(&h)), SidecarAction::Skip);
    }

    #[test]
    fn sidecar_action_installs_when_dest_missing() {
        // The heal path: installs degraded by the old updater have no DLL.
        assert_eq!(sidecar_action(&[7u8; 32], None), SidecarAction::Install);
    }

    #[test]
    fn sidecar_action_installs_when_hashes_differ() {
        assert_eq!(
            sidecar_action(&[7u8; 32], Some(&[8u8; 32])),
            SidecarAction::Install
        );
    }

    #[test]
    fn sha256_file_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.txt");
        fs::write(&path, b"abc").unwrap();
        // SHA-256("abc") — FIPS 180-2 test vector.
        let expected: [u8; 32] = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(sha256_file(&path).unwrap(), expected);
    }

    #[test]
    fn sha256_file_if_exists_fails_closed_on_a_non_file() {
        // A non-file at the dest path must yield Err, never Ok(None) — a
        // future widening of the NotFound match (e.g. swallowing
        // PermissionDenied as "missing") must not read a directory as
        // absent. On Windows the open itself fails non-NotFound; on Unix
        // the open succeeds and the read fails inside sha256_stream.
        let dir = tempfile::tempdir().unwrap();
        assert!(sha256_file_if_exists(dir.path()).is_err());
    }

    #[test]
    fn sha256_file_if_exists_distinguishes_missing_from_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.txt");
        assert_eq!(sha256_file_if_exists(&path).unwrap(), None);
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file_if_exists(&path).unwrap(),
            Some(sha256_file(&path).unwrap())
        );
    }

    // ---- end-to-end pipeline (no network) -------------------------------
    //
    // Builds a synthetic release archive shaped exactly like the Windows
    // asset (fake vex.exe + DirectML.dll in a tar.gz), signs it with a
    // throwaway zipsign key, then drives verify → extract → sidecar
    // install against a scratch "install dir". Covers everything in
    // `apply_update` except the real download, the final `self_replace`
    // (which would replace the test runner's own binary), the mapped-DLL
    // case where the `.old` reap fails and the file survives until a later
    // run, and sidecar placement when the exe is reached via a
    // symlink/junction (the `fs::canonicalize` step — needs a real
    // symlinked exe) — none of which can be simulated portably; all four
    // are part of the documented post-release manual smoke.

    const ASSET_NAME: &str = "vex-x86_64-pc-windows-msvc.tar.gz";
    const FAKE_EXE: &[u8] = b"fake vex.exe payload";
    const FAKE_DLL: &[u8] = b"fake DirectML.dll payload";

    fn test_keypair() -> (zipsign_api::SigningKey, [u8; 32]) {
        // Deterministic — no rand dep; any 32 bytes are a valid ed25519 seed.
        let signing = zipsign_api::SigningKey::from_bytes(&[42u8; 32]);
        let pubkey = signing.verifying_key().to_bytes();
        (signing, pubkey)
    }

    /// tar.gz the given entries and zipsign the result with `context`.
    fn signed_archive(entries: &[(&str, &[u8])], context: &[u8]) -> Vec<u8> {
        let mut tar_gz = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut tar_gz, flate2::Compression::fast());
            let mut tar = tar::Builder::new(enc);
            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, name, Cursor::new(data))
                    .unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        let (signing, _) = test_keypair();
        let mut signed = Cursor::new(Vec::new());
        zipsign_api::sign::copy_and_sign_tar(
            &mut Cursor::new(tar_gz),
            &mut signed,
            &[signing],
            Some(context),
        )
        .unwrap();
        signed.into_inner()
    }

    /// Write a signed archive into a fresh temp dir under `ASSET_NAME`.
    fn write_signed_archive(entries: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ASSET_NAME);
        fs::write(&path, signed_archive(entries, ASSET_NAME.as_bytes())).unwrap();
        (dir, path)
    }

    #[test]
    fn verify_signature_accepts_matching_key_and_context() {
        let (_dir, archive) = write_signed_archive(&[("vex.exe", FAKE_EXE)]);
        let (_, pubkey) = test_keypair();
        verify_signature(&archive, &[pubkey]).unwrap();
    }

    #[test]
    fn verify_signature_rejects_wrong_context() {
        // Signed under a different file name → the rename-the-asset attack.
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join(ASSET_NAME);
        let bytes = signed_archive(&[("vex.exe", FAKE_EXE)], b"some-other-asset.tar.gz");
        fs::write(&archive, bytes).unwrap();
        let (_, pubkey) = test_keypair();
        assert!(verify_signature(&archive, &[pubkey]).is_err());
    }

    #[test]
    fn verify_signature_rejects_wrong_key() {
        let (_dir, archive) = write_signed_archive(&[("vex.exe", FAKE_EXE)]);
        let other_pub = zipsign_api::SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(verify_signature(&archive, &[other_pub]).is_err());
    }

    /// Run verify → extract → sidecar install, returning the binary path
    /// `apply_update` would hand to `self_replace`.
    fn run_pipeline(tmp: &Path, archive: &Path, install_dir: &Path) -> Result<PathBuf> {
        let (_, pubkey) = test_keypair();
        verify_signature(archive, &[pubkey])?;
        self_update::Extract::from_source(archive).extract_into(tmp)?;
        install_from_extracted(tmp, ASSET_NAME, "vex", install_dir)
    }

    #[test]
    fn pipeline_installs_missing_sidecar_and_finds_binary() {
        let (tmp, archive) =
            write_signed_archive(&[("vex.exe", FAKE_EXE), ("DirectML.dll", FAKE_DLL)]);
        let install = tempfile::tempdir().unwrap(); // degraded: no DLL

        let binary = run_pipeline(tmp.path(), &archive, install.path()).unwrap();

        assert_eq!(fs::read(binary).unwrap(), FAKE_EXE);
        let dll = install.path().join("DirectML.dll");
        assert_eq!(
            fs::read(dll).unwrap(),
            FAKE_DLL,
            "heal path installs the DLL"
        );
    }

    #[test]
    fn pipeline_skips_identical_sidecar() {
        let (tmp, archive) =
            write_signed_archive(&[("vex.exe", FAKE_EXE), ("DirectML.dll", FAKE_DLL)]);
        let install = tempfile::tempdir().unwrap();
        let dll = install.path().join("DirectML.dll");
        fs::write(&dll, FAKE_DLL).unwrap();
        // Backdate the mtime far beyond any filesystem timestamp resolution
        // (FAT32/APFS: up to 2 s). A rewrite stamps the file with "now",
        // which can never equal the two-hour-old value — a plain
        // before/after comparison would false-pass whenever both writes
        // land in the same coarse timestamp tick.
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        let f = fs::OpenOptions::new().write(true).open(&dll).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(backdated))
            .unwrap();
        drop(f);
        let mtime_before = fs::metadata(&dll).unwrap().modified().unwrap();

        run_pipeline(tmp.path(), &archive, install.path()).unwrap();

        let mtime_after = fs::metadata(&dll).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "identical DLL must not be rewritten"
        );
    }

    #[test]
    fn pipeline_replaces_outdated_sidecar() {
        let (tmp, archive) =
            write_signed_archive(&[("vex.exe", FAKE_EXE), ("DirectML.dll", FAKE_DLL)]);
        let install = tempfile::tempdir().unwrap();
        let dll = install.path().join("DirectML.dll");
        fs::write(&dll, b"stale 1.0-era dll").unwrap();

        run_pipeline(tmp.path(), &archive, install.path()).unwrap();

        assert_eq!(fs::read(&dll).unwrap(), FAKE_DLL);
        assert!(
            !install.path().join("DirectML.dll.self-update.old").exists(),
            "set-aside copy is reaped when nothing maps the old DLL"
        );
    }

    #[test]
    fn pipeline_errors_when_binary_missing_from_archive() {
        let (tmp, archive) = write_signed_archive(&[("DirectML.dll", FAKE_DLL)]);
        let install = tempfile::tempdir().unwrap();
        let err = run_pipeline(tmp.path(), &archive, install.path()).unwrap_err();
        assert!(err.to_string().contains("did not contain"), "{err}");
    }

    #[test]
    fn pipeline_rejects_nested_archive_layout() {
        // The flat-archive release contract: a nested entry would extract
        // into a subdirectory the non-recursive scan never visits, so the
        // sidecar would be silently dropped — fail loudly instead.
        let (tmp, archive) =
            write_signed_archive(&[("vex.exe", FAKE_EXE), ("runtimes/DirectML.dll", FAKE_DLL)]);
        let install = tempfile::tempdir().unwrap();
        let err = run_pipeline(tmp.path(), &archive, install.path()).unwrap_err();
        assert!(
            err.to_string().contains("contains a directory"),
            "nested layout must be rejected, got: {err}"
        );
        assert_eq!(
            fs::read_dir(install.path()).unwrap().count(),
            0,
            "nothing may be installed from a malformed archive"
        );
    }

    /// The other half of the flat-archive contract: entries are judged on
    /// their OWN file type, so a symlink — even one pointing at a regular
    /// file — is rejected, not silently dereferenced into a sidecar.
    /// Unix-only because symlink creation on Windows needs privileges; the
    /// branch under test is platform-independent.
    #[cfg(unix)]
    #[test]
    fn extracted_files_rejects_symlink_entries() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("vex.exe"), FAKE_EXE).unwrap();
        std::os::unix::fs::symlink("vex.exe", tmp.path().join("DirectML.dll")).unwrap();

        let err = extracted_files(tmp.path(), ASSET_NAME).unwrap_err();

        assert!(
            err.to_string().contains("non-regular entry"),
            "symlink must be rejected, got: {err}"
        );
    }

    #[test]
    fn unix_archive_with_only_binary_installs_nothing_extra() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("vex-x86_64-unknown-linux-gnu.tar.gz");
        fs::write(
            &archive,
            signed_archive(&[("vex", FAKE_EXE)], b"vex-x86_64-unknown-linux-gnu.tar.gz"),
        )
        .unwrap();
        let install = tempfile::tempdir().unwrap();

        let (_, pubkey) = test_keypair();
        verify_signature(&archive, &[pubkey]).unwrap();
        self_update::Extract::from_source(&archive)
            .extract_into(dir.path())
            .unwrap();
        let binary = install_from_extracted(
            dir.path(),
            "vex-x86_64-unknown-linux-gnu.tar.gz",
            "vex",
            install.path(),
        )
        .unwrap();

        assert_eq!(fs::read(binary).unwrap(), FAKE_EXE);
        assert_eq!(
            fs::read_dir(install.path()).unwrap().count(),
            0,
            "no sidecars may be written for a unix archive"
        );
    }
}
