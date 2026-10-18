use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PRODUCTION_BUILD_RS: &str = include_str!("../build.rs");
const PACKAGE_VERSION: &str = "1.28.0";

struct Fixture {
    root: tempfile::TempDir,
    package: PathBuf,
    target: PathBuf,
    git_env: Vec<(String, String)>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create private fixture root");
        let package = root.path().join("package");
        let target = root.path().join("cargo-target");
        fs::create_dir_all(&package).expect("create package directory");

        let empty = root.path().join("empty-config");
        fs::write(&empty, "").expect("create isolated empty Git config");
        let git_env = vec![
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            (
                "GIT_CONFIG_GLOBAL".into(),
                empty.to_string_lossy().into_owned(),
            ),
            (
                "GIT_CONFIG_SYSTEM".into(),
                empty.to_string_lossy().into_owned(),
            ),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ];

        fs::write(
            package.join("Cargo.toml"),
            format!(
                "[package]\nname = \"version-fixture\"\nversion = \"{PACKAGE_VERSION}\"\nedition = \"2021\"\nbuild = \"build.rs\"\ninclude = [\"build.rs\", \"src/**\", \"Cargo.toml\"]\n"
            ),
        )
        .expect("write fixture manifest");
        fs::write(package.join("build.rs"), PRODUCTION_BUILD_RS)
            .expect("copy exact production build script");
        fs::create_dir_all(package.join("src")).expect("create fixture source directory");
        fs::write(
            package.join("src/main.rs"),
            "fn main() { println!(\"{}\", env!(\"VEX_VERSION\")); }\n",
        )
        .expect("write fixture binary");

        Self {
            root,
            package,
            target,
            git_env,
        }
    }

    fn command(&self, program: &str, args: &[&OsStr], cwd: &Path) -> Output {
        let mut command = Command::new(program);
        command
            .current_dir(cwd)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &self.git_env {
            command.env(key, value);
        }
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            command.env_remove(key);
        }
        let mut child = command
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {program}: {error}"));
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().expect("poll child") {
                let output = child.wait_with_output().expect("collect child output");
                assert!(
                    status.success(),
                    "{program} {:?} failed: {}",
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
                return output;
            }
            if started.elapsed() >= Duration::from_secs(60) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{program} {:?} exceeded 60 seconds", args);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn git(&self, args: &[&str], cwd: &Path) -> String {
        let output = self.command("git", &args.iter().map(OsStr::new).collect::<Vec<_>>(), cwd);
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn git_init(&self, path: &Path) {
        let templates = self.root.path().join("empty-templates");
        fs::create_dir_all(&templates).expect("create empty Git template directory");
        let template_arg = format!("--template={}", templates.to_string_lossy());
        self.git(
            &["init", "--quiet", "--initial-branch=main", &template_arg],
            path,
        );
        self.git(&["config", "user.name", "Build version fixture"], path);
        self.git(
            &["config", "user.email", "build-version@example.invalid"],
            path,
        );
        let hooks = self.root.path().join("empty-hooks");
        fs::create_dir_all(&hooks).expect("create empty hook directory");
        self.git(&["config", "core.hooksPath", hooks.to_str().unwrap()], path);
    }

    fn commit_file(&self, relative: &str, contents: &str, message: &str, cwd: &Path) {
        let path = cwd.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create committed file parent");
        }
        fs::write(path, contents).expect("write committed fixture file");
        self.git(&["add", "--", relative], cwd);
        self.git(&["commit", "--quiet", "-m", message], cwd);
    }

    fn build(&self) -> String {
        let output = self.command(
            "cargo",
            &[
                OsStr::new("build"),
                OsStr::new("--verbose"),
                OsStr::new("--manifest-path"),
                self.package.join("Cargo.toml").as_os_str(),
                OsStr::new("--target-dir"),
                self.target.as_os_str(),
            ],
            &self.package,
        );
        String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr)
    }

    fn binary_version(&self) -> String {
        let binary = self.target.join("debug").join(if cfg!(windows) {
            "version-fixture.exe"
        } else {
            "version-fixture"
        });
        let output = self.command(&binary.to_string_lossy(), &[], &self.package);
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn described_version(&self, cwd: &Path) -> String {
        self.git(&["describe", "--tags", "--always"], cwd)
    }
}

#[test]
fn cargo_refreshes_embedded_git_version_for_refs_and_worktrees() {
    let fixture = Fixture::new();
    fixture.git_init(&fixture.package);
    fixture.commit_file("README.md", "initial\n", "initial source", &fixture.package);
    fixture.git(&["tag", "v1.0.0"], &fixture.package);

    fixture.build();
    assert_eq!(
        fixture.binary_version(),
        fixture.described_version(&fixture.package)
    );

    // Source-only metadata changes must invalidate the build-script result.
    fixture.commit_file(
        "docs/notes.md",
        "docs changed\n",
        "docs only",
        &fixture.package,
    );
    fixture.build();
    assert_eq!(
        fixture.binary_version(),
        fixture.described_version(&fixture.package)
    );

    // A true no-op build must remain fresh and preserve the binary's value.
    let before = fixture.binary_version();
    let no_op = fixture.build();
    assert!(
        no_op.contains("Fresh"),
        "unchanged inputs should keep Cargo fresh: {no_op}"
    );
    assert_eq!(fixture.binary_version(), before);

    // Adding, moving, and deleting loose tags all affect git describe.
    fixture.git(&["tag", "v2.0.0"], &fixture.package);
    fixture.build();
    assert_eq!(
        fixture.binary_version(),
        fixture.described_version(&fixture.package)
    );
    fixture.git(&["tag", "--force", "v2.0.0", "HEAD~1"], &fixture.package);
    fixture.build();
    assert_eq!(
        fixture.binary_version(),
        fixture.described_version(&fixture.package)
    );
    fixture.git(&["tag", "--delete", "v2.0.0"], &fixture.package);
    fixture.build();
    assert_eq!(
        fixture.binary_version(),
        fixture.described_version(&fixture.package)
    );

    // Packed refs are a separate metadata path from loose refs.
    fixture.git(&["tag", "v3.0.0"], &fixture.package);
    fixture.git(&["pack-refs", "--all", "--prune"], &fixture.package);
    fixture.build();
    assert_eq!(
        fixture.binary_version(),
        fixture.described_version(&fixture.package)
    );
    fixture.git(&["tag", "--force", "v3.0.0", "HEAD~1"], &fixture.package);
    fixture.build();
    assert_eq!(
        fixture.binary_version(),
        fixture.described_version(&fixture.package)
    );
    fixture.git(&["tag", "--delete", "v3.0.0"], &fixture.package);
    fixture.build();
    assert_eq!(
        fixture.binary_version(),
        fixture.described_version(&fixture.package)
    );

    // A linked worktree stores HEAD under the common Git directory and has a
    // .git pointer file at the package root.
    let worktree = fixture.root.path().join("linked-worktree");
    fixture.git(
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked",
            worktree.to_str().unwrap(),
            "HEAD",
        ],
        &fixture.package,
    );
    fs::write(
        worktree.join("Cargo.toml"),
        fs::read(fixture.package.join("Cargo.toml")).unwrap(),
    )
    .expect("copy manifest into linked worktree");
    fs::write(worktree.join("build.rs"), PRODUCTION_BUILD_RS)
        .expect("copy build script into linked worktree");
    fs::create_dir_all(worktree.join("src")).expect("create linked worktree source directory");
    fs::write(
        worktree.join("src/main.rs"),
        "fn main() { println!(\"{}\", env!(\"VEX_VERSION\")); }\n",
    )
    .expect("write linked worktree binary");
    let linked = Fixture {
        root: tempfile::tempdir().expect("private linked target root"),
        package: worktree.clone(),
        target: fixture.root.path().join("linked-target"),
        git_env: fixture.git_env.clone(),
    };
    linked.build();
    assert_eq!(linked.binary_version(), linked.described_version(&worktree));
    fixture.git(&["checkout", "--quiet", "--detach", "HEAD~1"], &worktree);
    linked.build();
    assert_eq!(linked.binary_version(), linked.described_version(&worktree));
}

#[test]
fn package_without_git_uses_manifest_version_inside_unrelated_repository() {
    let fixture = Fixture::new();
    let outer = fixture.root.path().join("unrelated-outer-repo");
    fs::create_dir_all(&outer).expect("create unrelated outer repository");
    fixture.git_init(&outer);
    fixture.commit_file(
        "README.md",
        "outer repo\n",
        "unrelated outer commit",
        &outer,
    );
    fixture.git(&["tag", "outer-v9.9.9"], &outer);
    assert_eq!(fixture.described_version(&outer), "outer-v9.9.9");
    let package = outer.join("nested-package");
    fs::create_dir_all(&package).expect("create nested package without .git");
    fs::write(
        package.join("Cargo.toml"),
        fs::read(fixture.package.join("Cargo.toml")).unwrap(),
    )
    .expect("copy nested manifest");
    fs::write(package.join("build.rs"), PRODUCTION_BUILD_RS).expect("copy production build script");
    fs::create_dir_all(package.join("src")).expect("create nested source directory");
    fs::write(
        package.join("src/main.rs"),
        "fn main() { println!(\"{}\", env!(\"VEX_VERSION\")); }\n",
    )
    .expect("write nested binary");
    let nested = Fixture {
        root: tempfile::tempdir().expect("private nested target root"),
        package,
        target: fixture.root.path().join("nested-target"),
        git_env: fixture.git_env.clone(),
    };

    nested.build();
    assert_eq!(nested.binary_version(), PACKAGE_VERSION);
    let no_op = nested.build();
    assert!(
        no_op.contains("Fresh"),
        "unchanged no-Git inputs should keep Cargo fresh: {no_op}"
    );
}
