use assert_cmd::Command;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn compile_sentinel(root: &Path) -> PathBuf {
    let source = root.join("sentinel.rs");
    let executable = root.join(format!("dart{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(
        &source,
        "fn main() { let marker = std::env::var_os(\"VEX_SENTINEL_MARKER\").unwrap(); std::fs::write(marker, b\"executed\").unwrap(); }\n",
    )
    .unwrap();
    let status = std::process::Command::new("rustc")
        .args([
            source.as_os_str(),
            std::ffi::OsStr::new("-o"),
            executable.as_os_str(),
        ])
        .status()
        .expect("compile harmless sentinel executable");
    assert!(status.success(), "sentinel compilation failed");
    executable
}

fn project_with_fvm_sdk(root: &Path, sdk_parent: &Path) -> PathBuf {
    let sdk = sdk_parent.join(".fvm/flutter_sdk/bin/cache/dart-sdk");
    let bin = sdk.join("bin");
    std::fs::create_dir_all(bin.join("snapshots")).unwrap();
    compile_sentinel(&bin);
    std::fs::write(
        bin.join("snapshots/analysis_server.dart.snapshot"),
        b"fixture",
    )
    .unwrap();
    std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
    std::fs::write(
        root.join("target.dart"),
        "void target() {}\nvoid caller() { target(); }\n",
    )
    .unwrap();
    root.to_path_buf()
}

fn run_cli(root: &Path, marker: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(root)
        .env("VEX_CACHE_DIR", root.join(".vex-test-cache"))
        .env("VEX_SENTINEL_MARKER", marker)
        .args(args);
    cmd.output().unwrap()
}

#[test]
fn dart_fvm_sdk_is_never_executed_by_strict_usages_or_impact() {
    let fixture = TempDir::new().unwrap();
    let local_root = fixture.path().join("local");
    std::fs::create_dir_all(&local_root).unwrap();
    let local_root = project_with_fvm_sdk(&local_root, &local_root);

    let ancestor = fixture.path().join("ancestor");
    let nested_root = ancestor.join("project");
    std::fs::create_dir_all(&nested_root).unwrap();
    let nested_root = project_with_fvm_sdk(&nested_root, &ancestor);

    for (index, root) in [local_root, nested_root].into_iter().enumerate() {
        let marker = fixture.path().join(format!("sentinel-{index}.marker"));
        let mut index_cmd = Command::cargo_bin("vex").unwrap();
        index_cmd
            .current_dir(&root)
            .env("VEX_CACHE_DIR", root.join(".vex-test-cache"))
            .env("VEX_SENTINEL_MARKER", &marker);
        index_cmd.arg("index").assert().success();

        let _usages = run_cli(&root, &marker, &["usages", "target", "--strict"]);
        let impact = run_cli(&root, &marker, &["impact", "target", "--format", "json"]);
        assert!(!_usages.status.success());
        assert!(String::from_utf8_lossy(&_usages.stderr).contains("no strict binder for Dart yet"));
        let impact_json: serde_json::Value = serde_json::from_slice(&impact.stdout).unwrap();
        let strict = &impact_json["results"]["channels"]["strict_refs"];
        assert_eq!(strict["available"], false);
        assert!(strict.get("backend").is_none());

        assert!(
            impact.status.success(),
            "impact failed: {}",
            String::from_utf8_lossy(&impact.stderr)
        );
        assert!(
            !marker.exists(),
            "Dart SDK sentinel executed from {}",
            root.display()
        );
    }
}

#[test]
fn mixed_dart_project_keeps_native_strict_refs() {
    let fixture = TempDir::new().unwrap();
    let root = fixture.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    project_with_fvm_sdk(&root, &root);
    std::fs::write(
        root.join("native.rs"),
        "fn native_target() {}\nfn caller() {\n  native_target();\n}\n",
    )
    .unwrap();
    let mut index = Command::cargo_bin("vex").unwrap();
    index
        .current_dir(&root)
        .env("VEX_CACHE_DIR", root.join(".vex-test-cache"))
        .arg("index")
        .assert()
        .success();

    let result = run_cli(
        &root,
        &fixture.path().join("must-not-exist"),
        &["usages", "native_target", "--strict"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("native.rs:3"));
}

#[test]
fn mixed_dart_project_preserves_exact_native_subtoken_refs() {
    let fixture = TempDir::new().unwrap();
    let root = fixture.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    project_with_fvm_sdk(&root, &root);
    std::fs::write(
        root.join("native.rs"),
        "struct Service;\nstruct DerivedService;\nfn use_service(_: Service) {}\nfn use_derived(_: DerivedService) {}\n",
    )
    .unwrap();
    let mut index = Command::cargo_bin("vex").unwrap();
    index
        .current_dir(&root)
        .env("VEX_CACHE_DIR", root.join(".vex-test-cache"))
        .arg("index")
        .assert()
        .success();

    for (symbol, expected) in [
        (
            "Service",
            serde_json::json!([{"path": "native.rs", "line": 3}]),
        ),
        (
            "DerivedService",
            serde_json::json!([{"path": "native.rs", "line": 4}]),
        ),
    ] {
        let result = run_cli(
            &root,
            &fixture.path().join("must-not-exist"),
            &["usages", symbol, "--strict", "--format", "json"],
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(json["results"], expected, "wrong refs for {symbol}");
    }
}
