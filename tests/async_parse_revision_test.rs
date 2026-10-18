use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use tempfile::TempDir;
use vex::index::manifest::Manifest;
use vex::index::staleness::{self, Freshness};

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.parent().unwrap().join("cache"));
    cmd
}

fn git(dir: &Path, args: &[&str]) {
    let output = ProcessCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = ProcessCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn find_index_dir(project: &Path) -> std::path::PathBuf {
    fn walk(dir: &Path, depth: usize) -> Option<std::path::PathBuf> {
        if depth > 3 {
            return None;
        }
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.join("manifest.json").is_file() {
                    return Some(path);
                }
                if let Some(found) = walk(&path, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(&project.parent().unwrap().join("cache"), 0).expect("index directory after index")
}

fn wait_for_child(pid: u32) {
    #[cfg(windows)]
    {
        let wait = ProcessCommand::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Wait-Process -Id {pid} -Timeout 90 -ErrorAction SilentlyContinue; if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }}"
                ),
            ])
            .output()
            .unwrap();
        assert!(
            wait.status.success(),
            "refresh child {pid} did not exit: {}",
            String::from_utf8_lossy(&wait.stderr)
        );
    }
    #[cfg(unix)]
    {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            let status = ProcessCommand::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .unwrap();
            if !status.success() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("refresh child {pid} did not exit within 90 seconds");
    }
    #[cfg(not(any(windows, unix)))]
    panic!("no child-liveness implementation for this platform");
}

#[test]
fn current_revision_async_refresh_runs_exactly_once() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(".vex.toml"), "\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn payment_processor() {}\n").unwrap();
    git(&root, &["init", "--quiet"]);
    git(&root, &["config", "user.email", "vex-test@example.invalid"]);
    git(&root, &["config", "user.name", "Vex Test"]);
    git(&root, &["add", ".vex.toml", "src/lib.rs"]);
    git(&root, &["commit", "--quiet", "-m", "initial"]);

    vex_in(&root).arg("index").assert().success();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn payment_processor() {}\npub fn settlement_reconciler() {}\n",
    )
    .unwrap();
    git(&root, &["add", "src/lib.rs"]);
    git(&root, &["commit", "--quiet", "-m", "change"]);

    let initial = vex_in(&root)
        .args([
            "search",
            "payment_processor",
            "--auto-update",
            "--async-update",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&initial.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(envelope["_meta"]["vex.dev/stale"], true, "{stdout}");
    let index_dir = find_index_dir(&root);
    let attempt_path = index_dir.join("async_update.attempt");
    let child_pid: u32 = fs::read_to_string(&attempt_path)
        .unwrap()
        .trim()
        .parse()
        .expect("attempt marker records the spawned child PID");

    wait_for_child(child_pid);

    assert_eq!(
        fs::read(index_dir.join("index.parse-revision")).unwrap(),
        [b'V', b'E', b'X', b'R', 6, 0],
        "the child refresh must publish the current private revision marker"
    );
    let manifest = Manifest::load(&index_dir.join("manifest.json")).unwrap();
    assert!(
        matches!(staleness::check(&root, &manifest, false), Freshness::Fresh),
        "the one child refresh must leave a fresh index"
    );
    let pid_after_refresh = fs::read_to_string(&attempt_path).unwrap();
    let marker_mtime_after_refresh = fs::metadata(&attempt_path).unwrap().modified().unwrap();

    let settled_search = || {
        let settled = vex_in(&root)
            .args([
                "search",
                "settlement_reconciler",
                "--auto-update",
                "--async-update",
                "--format",
                "json",
            ])
            .assert()
            .success();
        let settled_stdout = String::from_utf8_lossy(&settled.get_output().stdout).into_owned();
        let settled_envelope: serde_json::Value = serde_json::from_str(&settled_stdout).unwrap();
        assert_ne!(
            settled_envelope["_meta"]["vex.dev/stale"], true,
            "{settled_stdout}"
        );
    };
    settled_search();
    settled_search();
    settled_search();

    assert_eq!(
        fs::read_to_string(&attempt_path).unwrap(),
        pid_after_refresh
    );
    let marker_mtime_after_settled = fs::metadata(&attempt_path).unwrap().modified().unwrap();
    assert_eq!(marker_mtime_after_settled, marker_mtime_after_refresh);
}

#[test]
fn marker_only_async_refresh_runs_exactly_once() {
    if let Some(root) = std::env::var_os("VEX_MARKER_ONLY_PROBE_ROOT") {
        let root = std::path::PathBuf::from(root);
        let index_dir = std::path::PathBuf::from(
            std::env::var_os("VEX_MARKER_ONLY_PROBE_INDEX_DIR")
                .expect("probe index directory is supplied by the parent test"),
        );
        let cache_dir = std::path::PathBuf::from(
            std::env::var_os("VEX_CACHE_DIR").expect("probe cache directory is supplied"),
        );
        assert!(cache_dir.starts_with(root.parent().unwrap()));
        assert!(index_dir.starts_with(&cache_dir));
        vex::util::config::set_cache_override(index_dir.clone(), true);
        assert_eq!(vex::util::config::index_dir(&root), index_dir);
        let manifest = Manifest::load(&index_dir.join("manifest.json")).unwrap();
        assert!(index_dir.join("index.vex").is_file());
        match std::env::var("VEX_MARKER_ONLY_PROBE_EXPECTED").as_deref() {
            Ok("stale") => assert!(
                matches!(
                    staleness::check(&root, &manifest, false),
                    Freshness::Stale {
                        changed_count: None
                    }
                ),
                "a missing private revision marker must be marker-only staleness"
            ),
            Ok("fresh") => assert!(
                matches!(staleness::check(&root, &manifest, false), Freshness::Fresh),
                "the completed child refresh must leave a fresh index"
            ),
            value => panic!("unexpected marker-only probe state: {value:?}"),
        }
        return;
    }

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    let source_path = root.join("src/lib.rs");
    let source_bytes = b"pub fn payment_processor() {}\npub fn settlement_reconciler() {}\n";
    fs::write(&source_path, source_bytes).unwrap();
    git(&root, &["init", "--quiet"]);
    git(&root, &["config", "user.email", "vex-test@example.invalid"]);
    git(&root, &["config", "user.name", "Vex Test"]);
    git(&root, &["add", ".vex.toml", "src/lib.rs"]);
    git(&root, &["commit", "--quiet", "-m", "initial"]);
    let head_before = git_stdout(&root, &["rev-parse", "HEAD"]);

    vex_in(&root).arg("index").assert().success();
    let index_dir = find_index_dir(&root);
    let revision_path = index_dir.join("index.parse-revision");
    fs::remove_file(&revision_path).unwrap();

    let manifest = Manifest::load(&index_dir.join("manifest.json")).unwrap();
    assert_eq!(manifest.git_head.as_deref(), Some(head_before.as_str()));
    assert!(index_dir.join("index.vex").is_file());
    assert_eq!(fs::read(&source_path).unwrap(), source_bytes);
    assert_eq!(git_stdout(&root, &["rev-parse", "HEAD"]), head_before);

    let staleness_probe = |expected: &str| {
        let probe = ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "marker_only_async_refresh_runs_exactly_once",
                "--nocapture",
            ])
            .env("VEX_MARKER_ONLY_PROBE_ROOT", &root)
            .env("VEX_MARKER_ONLY_PROBE_INDEX_DIR", &index_dir)
            .env("VEX_MARKER_ONLY_PROBE_EXPECTED", expected)
            .env("VEX_CACHE_DIR", root.parent().unwrap().join("cache"))
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "isolated marker-only {expected} probe failed: {}{}",
            String::from_utf8_lossy(&probe.stdout),
            String::from_utf8_lossy(&probe.stderr)
        );
    };
    staleness_probe("stale");

    let initial = vex_in(&root)
        .args([
            "search",
            "payment_processor",
            "--auto-update",
            "--async-update",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&initial.get_output().stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(envelope["_meta"]["vex.dev/stale"], true, "{stdout}");
    let attempt_path = index_dir.join("async_update.attempt");
    let child_pid: u32 = fs::read_to_string(&attempt_path)
        .unwrap()
        .trim()
        .parse()
        .expect("attempt marker records the spawned child PID");
    assert!(child_pid > 0, "refresh child PID must be positive");
    assert_eq!(fs::read(&source_path).unwrap(), source_bytes);
    assert_eq!(git_stdout(&root, &["rev-parse", "HEAD"]), head_before);

    wait_for_child(child_pid);

    assert_eq!(
        fs::read(&revision_path).unwrap(),
        [b'V', b'E', b'X', b'R', 6, 0],
        "the child refresh must publish the current private revision marker"
    );
    let manifest = Manifest::load(&index_dir.join("manifest.json")).unwrap();
    assert_eq!(manifest.git_head.as_deref(), Some(head_before.as_str()));
    staleness_probe("fresh");
    let pid_after_refresh = fs::read_to_string(&attempt_path).unwrap();
    let marker_mtime_after_refresh = fs::metadata(&attempt_path).unwrap().modified().unwrap();

    let settled_search = || {
        let settled = vex_in(&root)
            .args([
                "search",
                "settlement_reconciler",
                "--auto-update",
                "--async-update",
                "--format",
                "json",
            ])
            .assert()
            .success();
        let settled_stdout = String::from_utf8_lossy(&settled.get_output().stdout).into_owned();
        let settled_envelope: serde_json::Value = serde_json::from_str(&settled_stdout).unwrap();
        assert_ne!(
            settled_envelope["_meta"]["vex.dev/stale"], true,
            "{settled_stdout}"
        );
    };
    settled_search();
    settled_search();
    settled_search();

    assert_eq!(
        fs::read_to_string(&attempt_path).unwrap(),
        pid_after_refresh
    );
    let marker_mtime_after_settled = fs::metadata(&attempt_path).unwrap().modified().unwrap();
    assert_eq!(marker_mtime_after_settled, marker_mtime_after_refresh);
    assert_eq!(fs::read(&source_path).unwrap(), source_bytes);
    assert_eq!(git_stdout(&root, &["rev-parse", "HEAD"]), head_before);
}
