//! `hash_files` reuses the previous run's content hash for files whose
//! `(len, mtime)` is unchanged, instead of re-reading every tracked file.
//!
//! The optimisation is only worth having if it cannot miss a real edit, so
//! these drive the real binary through the cases that would break detection.

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn setup(dir: &Path) {
    std::fs::write(dir.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(dir.join("a.rs"), "fn alpha_one() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn beta_two() {}\n").unwrap();
    std::fs::write(dir.join("c.rs"), "fn gamma_three() {}\n").unwrap();
}

/// Uses `vex check`, not `vex search`: `check` is the honest hit/miss existence
/// probe. `search` prints `No results for "<name>"` on stdout, so a naive
/// substring test against its output reports a hit for every miss.
fn indexed(dir: &Path, symbol: &str) -> bool {
    let out = Command::cargo_bin("vex")
        .unwrap()
        .current_dir(dir)
        .env("VEX_CACHE_DIR", dir.join(".vex-test-cache"))
        .args(["check", symbol, "--format", "compact"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `--format compact` marks each probed name: `+ name` present, `- name`
    // absent. Match the marker, not the bare name.
    stdout
        .lines()
        .any(|l| l.trim_start().starts_with("+ ") && l.contains(symbol))
}

/// The load-bearing case: an ordinary edit changes both length and mtime, so
/// the stat cache must miss and the file must be re-hashed and re-indexed.
#[test]
fn an_edited_file_is_still_detected() {
    let tmp = TempDir::new().unwrap();
    setup(tmp.path());
    vex_in(tmp.path())
        .args(["index", "--path", "."])
        .assert()
        .success();
    assert!(!indexed(tmp.path(), "delta_four"));

    std::fs::write(
        tmp.path().join("b.rs"),
        "fn beta_two() {}\nfn delta_four() {}\n",
    )
    .unwrap();
    vex_in(tmp.path())
        .args(["update", "--path", "."])
        .assert()
        .success();

    assert!(
        indexed(tmp.path(), "delta_four"),
        "an edited file must be re-indexed; the stat cache must not swallow it"
    );
    assert!(
        indexed(tmp.path(), "alpha_one"),
        "untouched files must survive the update"
    );
}

/// A same-length edit is the case a naive size-only check would miss. mtime
/// still moves, so detection must hold.
#[test]
fn a_same_length_edit_is_detected() {
    let tmp = TempDir::new().unwrap();
    setup(tmp.path());
    vex_in(tmp.path())
        .args(["index", "--path", "."])
        .assert()
        .success();

    // Exactly the same byte count as the original, different identifier.
    let original = std::fs::read(tmp.path().join("c.rs")).unwrap();
    let replacement = "fn gamma_threx() {}\n";
    assert_eq!(
        original.len(),
        replacement.len(),
        "test setup must keep the length identical"
    );
    std::fs::write(tmp.path().join("c.rs"), replacement).unwrap();

    vex_in(tmp.path())
        .args(["update", "--path", "."])
        .assert()
        .success();

    assert!(
        indexed(tmp.path(), "gamma_threx"),
        "a same-length edit must still be detected via mtime"
    );
}

/// A new file has no cache entry at all, so it can only be hashed fresh.
#[test]
fn a_new_file_is_indexed() {
    let tmp = TempDir::new().unwrap();
    setup(tmp.path());
    vex_in(tmp.path())
        .args(["index", "--path", "."])
        .assert()
        .success();

    std::fs::write(tmp.path().join("d.rs"), "fn epsilon_five() {}\n").unwrap();
    vex_in(tmp.path())
        .args(["update", "--path", "."])
        .assert()
        .success();

    assert!(indexed(tmp.path(), "epsilon_five"));
}

/// Deleting a file must drop its symbols even though the deleted path never
/// reaches the stat-cache path at all.
#[test]
fn a_deleted_file_is_dropped() {
    let tmp = TempDir::new().unwrap();
    setup(tmp.path());
    vex_in(tmp.path())
        .args(["index", "--path", "."])
        .assert()
        .success();
    assert!(indexed(tmp.path(), "alpha_one"));

    std::fs::remove_file(tmp.path().join("a.rs")).unwrap();
    vex_in(tmp.path())
        .args(["update", "--path", "."])
        .assert()
        .success();

    assert!(
        !indexed(tmp.path(), "alpha_one"),
        "symbols from a deleted file must not survive"
    );
}

/// Proves the optimisation is actually *active*, independent of timing.
///
/// Every other test here exercises the safe path: subprocess overhead between
/// `index` and `update` pushes each edited file's mtime past the cutoff, so
/// they would all still pass if reuse were hard-disabled. This one revokes read
/// permission on an unchanged file — a stat-only reuse succeeds without ever
/// opening it, while a fallback to reading the bytes cannot.
#[cfg(unix)]
#[test]
fn an_unchanged_file_is_not_reopened() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    setup(tmp.path());

    // The racily-clean cutoff is stamped just before the indexing run hashes,
    // at second granularity. A file written in that same second is deliberately
    // NOT reuse-eligible, so age the fixtures past it before indexing —
    // otherwise this test proves nothing about reuse, only about the guard.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    vex_in(tmp.path())
        .args(["index", "--path", "."])
        .assert()
        .success();

    // Touch an unrelated file so the update has work to do at all.
    std::fs::write(
        tmp.path().join("b.rs"),
        "fn beta_two() {}\nfn zeta_six() {}\n",
    )
    .unwrap();

    let locked = tmp.path().join("a.rs");
    let original = std::fs::metadata(&locked).unwrap().permissions();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let result = vex_in(tmp.path())
        .args(["update", "--path", "."])
        .output()
        .unwrap();

    // Restore before asserting so a failure cannot leave an unreadable tempdir.
    std::fs::set_permissions(&locked, original).unwrap();

    assert!(
        result.status.success(),
        "update must succeed without reading the unchanged file; stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        indexed(tmp.path(), "alpha_one"),
        "the unreadable-but-unchanged file's symbols must survive via the stat cache"
    );
    assert!(
        indexed(tmp.path(), "zeta_six"),
        "the file that did change must still be picked up"
    );
}

/// Two consecutive updates with no changes in between must be stable — the
/// second one runs entirely off the stat cache and must not resurrect or drop
/// anything.
#[test]
fn repeated_updates_are_stable() {
    let tmp = TempDir::new().unwrap();
    setup(tmp.path());
    vex_in(tmp.path())
        .args(["index", "--path", "."])
        .assert()
        .success();

    for _ in 0..3 {
        vex_in(tmp.path())
            .args(["update", "--path", "."])
            .assert()
            .success();
    }

    for needle in ["alpha_one", "beta_two", "gamma_three"] {
        assert!(
            indexed(tmp.path(), needle),
            "{needle} must survive repeated no-op updates"
        );
    }
}
