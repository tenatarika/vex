//! `--exclude-tests`: the shared scope flag that drops test files via the
//! canonical `util::test_paths::is_test_path` predicate. Isolated cache dir
//! per test, same pattern as `cli_scope_test.rs` / `cli_modules_test.rs`.

use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

/// `payment_processor` defined in `src/api.rs` and called from a production
/// file, an integration-test dir file and a sibling `tests.rs` module file.
fn write_project(dir: &Path) {
    write(
        &dir.join("src/api.rs"),
        "pub fn payment_processor() {}\npub fn run_checkout() { payment_processor(); }\n",
    );
    write(
        &dir.join("tests/integration.rs"),
        "fn check_payment() { payment_processor(); }\n",
    );
    write(
        &dir.join("src/tests.rs"),
        "fn unit_payment() { payment_processor(); }\n",
    );
    vex_in(dir).args(["index"]).assert().success();
}

fn json(dir: &Path, args: &[&str]) -> Value {
    let out = vex_in(dir)
        .args(args)
        .args(["--no-stale-check", "--format", "json"])
        .output()
        .unwrap();
    // Exit 1 is the documented "no results" code; the envelope is still valid.
    assert!(
        matches!(out.status.code(), Some(0 | 1)),
        "vex {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("valid JSON envelope")
}

fn paths_of(env: &Value) -> Vec<String> {
    env["results"]
        .as_array()
        .expect("results array")
        .iter()
        .filter_map(|r| r["path"].as_str())
        .map(|p| p.replace('\\', "/"))
        .collect()
}

fn is_test_like(p: &str) -> bool {
    p.starts_with("tests/") || p.ends_with("/tests.rs")
}

#[test]
fn usages_exclude_tests_drops_test_file_hits() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());

    let all = paths_of(&json(
        tmp.path(),
        &["usages", "payment_processor", "--strict"],
    ));
    assert!(all.iter().any(|p| p == "tests/integration.rs"), "{all:?}");
    assert!(all.iter().any(|p| p == "src/tests.rs"), "{all:?}");

    let kept = paths_of(&json(
        tmp.path(),
        &["usages", "payment_processor", "--strict", "--exclude-tests"],
    ));
    assert_eq!(kept, vec!["src/api.rs".to_string()], "{kept:?}");
}

#[test]
fn usages_why_trace_records_exclude_tests() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());
    let env = json(
        tmp.path(),
        &[
            "usages",
            "payment_processor",
            "--strict",
            "--exclude-tests",
            "--why",
        ],
    );
    let trace = &env["_meta"]["vex.dev/why_trace"];
    assert_eq!(trace["filter_applied"]["exclude_tests"], true, "{trace}");
    assert_eq!(trace["scope_dropped"], 2, "{trace}");
}

#[test]
fn search_exclude_tests_drops_test_definitions() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());

    let all = paths_of(&json(tmp.path(), &["search", "unit_payment"]));
    assert!(all.iter().any(|p| p == "src/tests.rs"), "{all:?}");

    let kept = paths_of(&json(
        tmp.path(),
        &["search", "unit_payment", "--exclude-tests"],
    ));
    assert!(kept.iter().all(|p| !is_test_like(p)), "{kept:?}");
}

#[test]
fn search_exclude_tests_composes_with_include() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());
    // `--include tests/**` alone finds the integration file; adding
    // `--exclude-tests` must remove it (exclude wins over include).
    let inc = paths_of(&json(
        tmp.path(),
        &["search", "check_payment", "--include", "tests/**"],
    ));
    assert!(inc.iter().any(|p| p == "tests/integration.rs"), "{inc:?}");
    let both = paths_of(&json(
        tmp.path(),
        &[
            "search",
            "check_payment",
            "--include",
            "tests/**",
            "--exclude-tests",
        ],
    ));
    assert!(both.is_empty(), "{both:?}");
}

#[test]
fn callers_exclude_tests_drops_test_callers() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());
    let kept = json(
        tmp.path(),
        &["callers", "payment_processor", "--exclude-tests"],
    );
    let names: Vec<&str> = kept["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert!(names.contains(&"run_checkout"), "{names:?}");
    assert!(!names.contains(&"check_payment"), "{names:?}");
    assert!(!names.contains(&"unit_payment"), "{names:?}");
}

/// A fully connected call clique spread over three files in `dir`.
fn write_clique(dir: &Path, prefix: &str, names: &[&str]) {
    let mut bodies = [String::new(), String::new(), String::new()];
    for (i, n) in names.iter().enumerate() {
        let calls: Vec<String> = names
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, m)| format!("{prefix}_{m}();"))
            .collect();
        bodies[i % 3].push_str(&format!(
            "pub fn {prefix}_{n}() {{ {} }}\n",
            calls.join(" ")
        ));
    }
    write(&dir.join("part_a.rs"), &bodies[0]);
    write(&dir.join("part_b.rs"), &bodies[1]);
    write(&dir.join("part_c.rs"), &bodies[2]);
}

#[test]
fn modules_exclude_tests_hides_test_only_clusters_and_hubs() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    write_clique(
        &dir.join("src/net"),
        "net",
        &["connect", "send", "recv", "close", "retry"],
    );
    write_clique(
        &dir.join("tests/net"),
        "probe",
        &["connect", "send", "recv", "close"],
    );
    vex_in(dir).args(["index"]).assert().success();

    let labels = |env: &Value| -> Vec<String> {
        env["results"]["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["label"].as_str().unwrap().replace('\\', "/"))
            .collect()
    };
    let all = json(dir, &["modules", "--min-size", "1"]);
    assert!(
        labels(&all).iter().any(|l| l.starts_with("tests/")),
        "fixture must cluster the tests dir: {:?}",
        labels(&all)
    );

    let kept = json(dir, &["modules", "--min-size", "1", "--exclude-tests"]);
    let ls = labels(&kept);
    assert!(ls.iter().any(|l| l == "src/net/"), "{ls:?}");
    assert!(ls.iter().all(|l| !l.starts_with("tests/")), "{ls:?}");
    for c in kept["results"]["clusters"].as_array().unwrap() {
        for h in c["hubs"].as_array().unwrap() {
            let p = h["path"].as_str().unwrap().replace('\\', "/");
            assert!(!is_test_like(&p), "test hub leaked: {p}");
        }
    }
}

// ── Coverage for the remaining scoped commands ───────────────────────────

use std::process::Command as StdCommand;

fn git(dir: &Path, args: &[&str]) {
    let st = StdCommand::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("invoke git");
    assert!(st.success(), "git {args:?} failed");
}

const TEST_FILES: [&str; 2] = ["tests/integration.rs", "src/tests.rs"];

/// `(stdout, exit code)` of `vex <args>` with the raw text/json output.
fn raw(dir: &Path, args: &[&str]) -> (String, i32) {
    let out = vex_in(dir).args(args).output().unwrap();
    let code = out.status.code().unwrap();
    assert!(
        matches!(code, 0 | 1),
        "vex {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).replace('\\', "/"),
        code,
    )
}

fn mentions_test_file(s: &str) -> bool {
    TEST_FILES.iter().any(|f| s.contains(f))
}

/// Adds a trait with a production and a test implementor, and a name
/// (`shared_helper_fn`) defined in both a production and a test file.
fn write_extras(dir: &Path) {
    write(
        &dir.join("src/gateway.rs"),
        "pub trait PaymentGateway {}\npub struct LiveGateway;\nimpl PaymentGateway for LiveGateway {}\n\
         pub fn shared_helper_fn() {}\npub fn run_gateway() { fixture_builder_fn(); }\n",
    );
    write(
        &dir.join("tests/integration.rs"),
        "fn check_payment() { payment_processor(); }\nstruct MockGateway;\n\
         impl PaymentGateway for MockGateway {}\npub fn shared_helper_fn() {}\n\
         pub fn fixture_builder_fn() {}\n",
    );
    vex_in(dir).args(["index"]).assert().success();
}

#[test]
fn scoped_commands_drop_test_file_hits() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    write_project(dir);
    write_extras(dir);

    // (label, args without the flag, a needle that must be visible without it)
    let cases: [(&str, Vec<&str>, &str); 5] = [
        (
            "grep",
            vec!["grep", "payment_processor"],
            "tests/integration.rs",
        ),
        // `--limit 5`: the default `--limit 1` shows only the top-ranked
        // definition, and the test-path demotion ranks src/gateway.rs first.
        (
            "show",
            vec!["show", "shared_helper_fn", "--limit", "5"],
            "tests/integration.rs",
        ),
        (
            "impact",
            vec!["impact", "payment_processor", "--format", "json"],
            "tests/integration.rs",
        ),
        (
            "callees",
            vec!["callees", "check_payment"],
            "tests/integration.rs",
        ),
        (
            "implementations",
            vec!["implementations", "PaymentGateway"],
            "tests/integration.rs",
        ),
    ];
    for (label, args, needle) in cases {
        let (before, _) = raw(dir, &args);
        assert!(
            before.contains(needle),
            "{label}: baseline lacks {needle}: {before}"
        );
        let mut with_flag = args.clone();
        with_flag.push("--exclude-tests");
        let (after, _) = raw(dir, &with_flag);
        assert!(
            !mentions_test_file(&after),
            "{label}: test file leaked: {after}"
        );
    }
}

/// Default `show` (`--limit 1`) picks the production definition on every OS.
/// It used to return the first posting in index order, which is `readdir`
/// order: the test file on APFS/ext4, the production file on NTFS.
#[test]
fn show_default_limit_prefers_production_definition() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    write_project(dir);
    write_extras(dir);
    let (out, code) = raw(dir, &["show", "shared_helper_fn"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("src/gateway.rs"), "{out}");
    assert!(!out.contains("tests/integration.rs"), "{out}");
}

#[test]
fn tests_for_rejects_exclude_tests() {
    let tmp = TempDir::new().unwrap();
    write_project(tmp.path());
    let out = vex_in(tmp.path())
        .args(["tests-for", "payment_processor", "--exclude-tests"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "must be an error, not exit 1");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("contradicts"), "{err}");
}

fn init_git(dir: &Path) {
    git(dir, &["init", "-q", "--initial-branch=main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

#[test]
fn diff_and_pr_impact_honour_exclude_tests() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    write(&dir.join("src/lib.rs"), "pub fn alpha_fn() {}\n");
    write(&dir.join("tests/it.rs"), "fn beta_fn() { alpha_fn(); }\n");
    write(&dir.join("src/tests.rs"), "fn gamma_fn() { alpha_fn(); }\n");
    init_git(dir);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "base"]);
    // Change production and test files; alpha_fn's body changes so the
    // pr-impact BFS reaches its test callers.
    write(
        &dir.join("src/lib.rs"),
        "pub fn alpha_fn() { let _x = 1; }\n",
    );
    write(
        &dir.join("tests/it.rs"),
        "fn beta_fn() { alpha_fn(); }\nfn delta_fn() {}\n",
    );
    vex_in(dir).args(["index"]).assert().success();

    let (d_all, _) = raw(dir, &["diff", "--base", "HEAD", "--format", "json"]);
    assert!(d_all.contains("tests/it.rs"), "{d_all}");
    let (d_kept, _) = raw(
        dir,
        &[
            "diff",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--exclude-tests",
        ],
    );
    assert!(!d_kept.contains("tests/it.rs"), "{d_kept}");
    assert!(d_kept.contains("src/lib.rs"), "{d_kept}");

    let pr = [
        "bundle",
        "--mode",
        "pr-impact",
        "--base",
        "HEAD",
        "--format",
        "json",
    ];
    let (p_all, _) = raw(dir, &pr);
    assert!(
        p_all.contains("src/tests.rs"),
        "baseline must reach the unchanged test caller via the BFS: {p_all}"
    );
    let mut with_flag = pr.to_vec();
    with_flag.push("--exclude-tests");
    let (p_kept, _) = raw(dir, &with_flag);
    assert!(
        !p_kept.contains("tests/it.rs") && !p_kept.contains("src/tests.rs"),
        "BFS caller/test rows must honour the scope: {p_kept}"
    );
    assert!(p_kept.contains("src/lib.rs"), "{p_kept}");
}
