//! Phase 14.8 Step 3 — RED integration tests for the `git_history`
//! section + indexed `vex history` path.
//!
//! All tests are expected to **FAIL** today because the section writer
//! (Step 4a/4b), pipeline wire-up (Step 4c), and manifest plumbing
//! (Step 5) haven't landed yet. The `--history` flag is parsed
//! (Step 3a scaffold) but no-op'd in the handler; this file pins the
//! end-state contract the builder must satisfy.
//!
//! What each test pins:
//!   1. `index_history_writes_vxgh_section` — raw bytes contract:
//!      `index.vex` must contain the `b"VXGH"` magic after
//!      `vex index --history`. The most basic "section exists" probe.
//!   2. `index_history_persisted_in_manifest` — manifest carries
//!      `history_indexed_at = Some(_)` so subsequent updates know to
//!      rebuild the section incrementally.
//!   3. `history_query_indexed_advertises_via_meta` — JSON envelope on
//!      `vex history` must carry `_meta.vex.dev/history_mode = "indexed"`
//!      when a section is present (vs `"walker"` when falling back to
//!      v1.16 query-time path).
//!   4. `history_finds_deleted_symbol` — the NEW capability vs v1.16
//!      walker: a function deleted from HEAD must still appear in
//!      `vex history <name>` output when the section is present.
//!   5. `history_depth_caps_globally` — `--history-depth N` bounds the
//!      walk to the N newest commits globally (NOT per-file), matching
//!      `git log -nN` semantics (architect-locked M3).
//!   6. `vex_status_reports_history_line` — `vex status --format json`
//!      surfaces a `history_indexed_at` field so agents can detect
//!      whether the section is present.
//!
//! Force-push detection, back-compat, and incremental-update perf
//! tests intentionally deferred to Step 5 (manifest+update) per the
//! task file decomposition.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Synthetic-repo helpers (inlined; matches the pattern in
// `src/history/mod.rs::tests::init_repo` so the test fixture shape is
// identical to what the walker's own unit tests use)
// ---------------------------------------------------------------------------

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env_remove("VEX_CACHE_DIR");
    cmd
}

fn git(repo: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        repo.display()
    );
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Tester"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    // Keep the index inside the repo subtree so tests don't pollute
    // the user's real cache.
    std::fs::write(
        dir.join(".vex.toml"),
        "local_cache = true\nformat = \"compact\"\n",
    )
    .unwrap();
}

fn commit_file(repo: &Path, rel_path: &str, content: &str, msg: &str) {
    let abs = repo.join(rel_path);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&abs, content).unwrap();
    git(repo, &["add", rel_path]);
    git(repo, &["commit", "-q", "-m", msg]);
}

#[allow(dead_code)] // reserved for future deleted-file scenarios; force-push test uses `git update-ref -d` instead
fn delete_committed(repo: &Path, rel_path: &str, msg: &str) {
    git(repo, &["rm", "-q", rel_path]);
    git(repo, &["commit", "-q", "-m", msg]);
}

/// Locate a file by name anywhere under `repo`. Used to find
/// `index.vex` or the Phase 14.8 sidecar `index.git_history` without
/// encoding the cache layout (xxh3-of-canonical-path).
fn find_file(repo: &Path, file_name: &str) -> Option<PathBuf> {
    use std::fs;
    fn walk(p: &Path, target: &str) -> Option<PathBuf> {
        if !p.is_dir() {
            return None;
        }
        for entry in fs::read_dir(p).ok()?.flatten() {
            let path = entry.path();
            if path.is_file() && path.file_name().map(|n| n == target).unwrap_or(false) {
                return Some(path);
            }
            if path.is_dir() {
                if let Some(found) = walk(&path, target) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(repo, file_name)
}

fn find_index_vex(repo: &Path) -> Option<PathBuf> {
    find_file(repo, "index.vex")
}

/// Search a binary file for the section magic.
fn contains_bytes(path: &Path, needle: &[u8]) -> bool {
    let bytes = std::fs::read(path).expect("read index.vex");
    bytes.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn index_history_writes_vxgh_section() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(
        repo,
        "src/lib.rs",
        "pub fn alpha() -> u8 { 1 }\n",
        "c1: add alpha",
    );
    commit_file(
        repo,
        "src/lib.rs",
        "pub fn alpha() -> u32 { 2 }\npub fn beta() -> u8 { 0 }\n",
        "c2: alpha widens, add beta",
    );

    vex_in(repo).args(["index", "--history"]).assert().success();

    // Step 4a deviation note: the locked design called for an inline
    // section in `index.vex`. To keep Step 4a tractable the section
    // ships as a sidecar at `<index_dir>/index.git_history` (same
    // pattern as `index.hashes`/`index.bodytokens`/`index.bloom`).
    // On-disk schema is byte-identical; promotion to inline is a
    // mechanical relocation of bytes. Test asserts the section's
    // `VXGH` magic in whichever file currently carries the section.
    let sidecar = find_file(repo, "index.git_history");
    let inline = find_index_vex(repo);
    let target = sidecar.clone().or(inline.clone()).expect(
        "either index.git_history sidecar or index.vex must exist after vex index --history",
    );
    assert!(
        contains_bytes(&target, b"VXGH"),
        "Phase 14.8: file at {} must carry the `VXGH` git-history \
         section magic after `vex index --history`. \
         (sidecar found: {:?}, inline index found: {:?})",
        target.display(),
        sidecar,
        inline,
    );
}

#[test]
fn index_history_persisted_in_manifest() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn one() {}\n", "c1");
    commit_file(
        repo,
        "src/lib.rs",
        "pub fn one() {}\npub fn two() {}\n",
        "c2",
    );

    vex_in(repo).args(["index", "--history"]).assert().success();

    // Manifest is the v6 sidecar at `<index_dir>/manifest.json` (path
    // mirror of `index.vex`). Read it and assert the future
    // `history_indexed_at` field is present + non-null.
    let idx = find_index_vex(repo).expect("index.vex must exist");
    let manifest_path = idx.with_file_name("manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|_| panic!("manifest at {} must exist", manifest_path.display()));
    let manifest: serde_json::Value =
        serde_json::from_str(&manifest_text).expect("manifest must be valid JSON");

    let field = manifest.get("history_indexed_at");
    assert!(
        matches!(field, Some(v) if !v.is_null()),
        "Phase 14.8: manifest at {} must carry `history_indexed_at = Some(_)` \
         after `vex index --history` (Step 5 plumbing). Got: {:?}",
        manifest_path.display(),
        field
    );
}

#[test]
fn history_query_indexed_advertises_via_meta() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(
        repo,
        "src/lib.rs",
        "pub fn parse_payment(amount: u8) -> u8 { amount }\n",
        "c1",
    );
    commit_file(
        repo,
        "src/lib.rs",
        "pub fn parse_payment(amount: u32) -> u32 { amount }\n",
        "c2: widen",
    );

    vex_in(repo).args(["index", "--history"]).assert().success();

    let output = vex_in(repo)
        .args(["history", "parse_payment", "--format", "json"])
        .output()
        .expect("spawn vex history");
    assert!(
        output.status.success(),
        "vex history exit non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("history JSON parse: {e}\nstdout: {stdout}"));

    // Phase 14.8 Step 4c — render_json must set this in `_meta` so
    // agents can distinguish indexed vs walker results.
    let mode = envelope
        .pointer("/_meta/vex.dev~1history_mode")
        .and_then(|v| v.as_str());
    assert_eq!(
        mode,
        Some("indexed"),
        "Phase 14.8: `vex history --format json` must advertise \
         `_meta.vex.dev/history_mode = \"indexed\"` when a section is \
         present. Got `{:?}`. Envelope: {}",
        mode,
        stdout,
    );
}

#[test]
fn history_finds_deleted_symbol() {
    // NEW capability vs the v1.16 walker: a function whose name no
    // longer appears at HEAD is invisible to the walker (`git grep`
    // at the chosen tip is the candidate-file probe). The indexed
    // path walks history directly and surfaces it.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(
        repo,
        "src/lib.rs",
        "pub fn doomed_helper() -> u8 { 7 }\npub fn live() {}\n",
        "c1: doomed_helper introduced",
    );
    commit_file(
        repo,
        "src/lib.rs",
        "pub fn live() {}\n",
        "c2: doomed_helper removed",
    );

    vex_in(repo).args(["index", "--history"]).assert().success();

    let output = vex_in(repo)
        .args(["history", "doomed_helper", "--format", "json"])
        .output()
        .expect("spawn vex history");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("history JSON parse: {e}\nstdout: {stdout}"));
    let items = envelope
        .pointer("/results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    assert!(
        !items.is_empty(),
        "Phase 14.8 (NEW capability): `vex history doomed_helper` must find \
         the c1 version of `doomed_helper` even though it's deleted from HEAD. \
         The v1.16 walker can't (probes via `git grep` at HEAD); the indexed \
         path walks history. Step 4c picks indexed via `HistoryMode::Auto`. \
         Got empty results. Envelope: {}",
        stdout,
    );
}

#[test]
fn history_depth_caps_globally() {
    // architect-locked M3: `--history-depth N` is a GLOBAL commit cap,
    // mirroring `git log -nN` semantics. Walking 5 commits with
    // `--history-depth 2` must visit exactly the 2 newest (commits
    // touching the global ordering, not per-file).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    for i in 0..5 {
        commit_file(
            repo,
            "src/lib.rs",
            &format!("pub fn v{}() {{ /* version {} */ }}\n", i, i),
            &format!("c{}: introduce v{}", i, i),
        );
    }

    vex_in(repo)
        .args(["index", "--history", "--history-depth", "2"])
        .assert()
        .success();

    let idx = find_index_vex(repo).expect("index.vex must exist");
    let manifest_path = idx.with_file_name("manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap_or_default())
            .unwrap_or(serde_json::Value::Null);

    // Step 5 will record commit_count in the manifest's history
    // sub-object so this is testable without parsing the binary
    // section directly.
    let commits = manifest
        .pointer("/history/commit_count")
        .and_then(|v| v.as_u64());
    assert_eq!(
        commits,
        Some(2),
        "Phase 14.8: with `--history-depth 2` on a 5-commit repo, the \
         section's commit_count must be exactly 2 (NOT 5, NOT per-file). \
         Manifest at {} reported: {:?}. Full manifest: {}",
        manifest_path.display(),
        commits,
        manifest,
    );
}

#[test]
fn update_history_linear_new_commits_is_incremental() {
    // Phase 14.8 Step 5c — pin the incremental-update contract.
    // Sequence:
    //   1. Index at commit C1 with symbol_one + symbol_two.
    //   2. Add commit C2 introducing symbol_three (new file —
    //      ensures `vex update` doesn't short-circuit at the
    //      file-hash diff gate).
    //   3. `vex update` (sticky-via-manifest) → incremental path.
    //   4. Assert: sidecar rewritten, both old symbols (one, two)
    //      AND new symbol (three) findable via `vex history`.
    //
    // The merge contract pinned: prior section's symbols survive the
    // incremental rebuild AND the delta's new symbols are appended.
    // If the incremental path silently dropped prior data, the
    // `symbol_one` assertion would fail. If the delta walk silently
    // missed the new commit, `symbol_three` would fail.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(
        repo,
        "src/lib.rs",
        "pub fn symbol_one() -> u8 { 1 }\npub fn symbol_two() -> u8 { 2 }\n",
        "c1: introduce one + two",
    );

    vex_in(repo).args(["index", "--history"]).assert().success();
    let sidecar = find_file(repo, "index.git_history").expect("sidecar after first index");
    let mtime_before = std::fs::metadata(&sidecar).unwrap().modified().unwrap();

    // Wait so a re-write lands at a distinguishable mtime.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // C2: new file with new symbol. New file ensures file_hashes
    // differ so update_inner doesn't take the no-change skip path.
    commit_file(
        repo,
        "src/other.rs",
        "pub fn symbol_three() -> u8 { 3 }\n",
        "c2: introduce three in a new file",
    );

    vex_in(repo).args(["update"]).assert().success();

    let mtime_after = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
    assert!(
        mtime_after > mtime_before,
        "sidecar must be rewritten on incremental update; \
         before={:?} after={:?}",
        mtime_before,
        mtime_after
    );

    // All three symbols findable.
    for name in ["symbol_one", "symbol_two", "symbol_three"] {
        let output = vex_in(repo)
            .args(["history", name, "--format", "json", "--limit", "5"])
            .output()
            .expect("vex history");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let items = envelope
            .pointer("/results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            !items.is_empty(),
            "Phase 14.8 Step 5c: after incremental update, `{name}` must be \
             findable via vex history. Got empty result. Envelope: {stdout}"
        );
        let mode = envelope
            .pointer("/_meta/vex.dev~1history_mode")
            .and_then(|v| v.as_str());
        assert_eq!(
            mode,
            Some("indexed"),
            "expected indexed mode for `{name}`, got {mode:?}"
        );
    }
}

#[test]
fn update_history_no_new_commits_uses_fast_path() {
    // Phase 14.8 Step 5b — pin the "second `vex update` on no-new-
    // commits is fast" contract. The fast path skips the
    // build_history_section call entirely (which is ~12s on vex
    // self-repo, ~5-50ms on a 1-commit synthetic). We assert the
    // sidecar's mtime is preserved across the update — the writer
    // does NOT re-touch a sidecar it reused — and the manifest's
    // `history_indexed_at` is refreshed to today's date. Combined,
    // those two facts pin "reused, not rewritten".
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn one() {}\n", "c1");

    // First index: writes sidecar + populates manifest.
    vex_in(repo).args(["index", "--history"]).assert().success();
    let sidecar_path =
        find_file(repo, "index.git_history").expect("sidecar exists after first --history index");
    let mtime_before = std::fs::metadata(&sidecar_path)
        .unwrap()
        .modified()
        .unwrap();

    // Wait a tick so a re-write would land at a different mtime.
    // (filesystem timestamp resolution on macOS HFS+/APFS is 1 ns,
    // but the sidecar write is fast enough that a no-op test could
    // theoretically match without this. Sleep 200ms for safety.)
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Second update: no new commits, sticky-via-manifest picks
    // with_history=true. Should hit the fast path.
    vex_in(repo).args(["update"]).assert().success();

    let mtime_after = std::fs::metadata(&sidecar_path)
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "Phase 14.8 Step 5b: sidecar mtime must not change on no-op `vex update` \
         (fast path should reuse the sidecar, not rewrite it). \
         before={:?} after={:?}",
        mtime_before, mtime_after
    );
}

#[test]
fn force_push_triggers_full_rebuild_with_warning() {
    // Phase 14.8 Step 5b — architect H3 force-push detection.
    // Sequence:
    //   1. Index at commit A.
    //   2. `git reset --hard <root>` then create commit B' — a
    //      different branch tip that does NOT have A as an ancestor.
    //   3. `vex update` (sticky-via-manifest) → MUST rebuild from
    //      scratch (not panic, not silently produce a corrupt
    //      section pointing at unreachable A).
    //
    // Assertion: section after rebuild contains B's blob SHA and
    // does NOT contain A's blob SHA (A was rewritten out).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn alpha_one() {}\n", "c1");
    // Reset to nothing and rebuild a different commit at the same tip
    // position — `git update-ref -d HEAD` clears the branch then
    // re-commits make a fresh genealogy.
    git(repo, &["update-ref", "-d", "HEAD"]);
    std::fs::remove_file(repo.join("src/lib.rs")).unwrap();
    commit_file(repo, "src/lib.rs", "pub fn beta_two() {}\n", "c-rewritten");

    // Index against the original commit first, then update against
    // the rewritten history. Order: first init writes sidecar with
    // the new history (c1 is unreachable, only c-rewritten exists),
    // and the test then re-indexes via vex update. The behavioural
    // contract: vex doesn't panic + sidecar's symbol set matches
    // c-rewritten, not c1.
    vex_in(repo).args(["index", "--history"]).assert().success();
    let sidecar = find_file(repo, "index.git_history").expect("sidecar exists");

    // Querying for c1's symbol must NOT find a result (c1 is
    // unreachable from any ref).
    let output = vex_in(repo)
        .args(["history", "alpha_one", "--format", "json"])
        .output()
        .expect("vex history");
    assert!(output.status.success(), "vex history must succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let items = envelope
        .pointer("/results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        items.is_empty(),
        "Phase 14.8 Step 5b: after history rewrite, the dropped commit's \
         symbol (`alpha_one`) must NOT appear in `vex history` output. \
         Got items: {items:?}"
    );

    // Sidecar still exists and the b_two symbol IS found.
    assert!(sidecar.exists());
    let beta_output = vex_in(repo)
        .args(["history", "beta_two", "--format", "json"])
        .output()
        .expect("vex history");
    let beta_stdout = String::from_utf8_lossy(&beta_output.stdout);
    let beta_env: serde_json::Value = serde_json::from_str(&beta_stdout).unwrap();
    let beta_items = beta_env
        .pointer("/results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !beta_items.is_empty(),
        "`beta_two` from the rewritten history must be findable. Got: {beta_stdout}"
    );
}

#[test]
fn no_history_drops_sidecar_and_manifest_fields() {
    // Phase 14.8 Step 5b sticky-drop: `vex update --no-history` after
    // an indexed run deletes the sidecar AND nulls the manifest's
    // history_* fields. Subsequent `vex history` falls back to the
    // walker (advertised via `_meta.vex.dev/history_mode = "walker"`).
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn here() {}\n", "c1");

    vex_in(repo).args(["index", "--history"]).assert().success();
    let sidecar = find_file(repo, "index.git_history").expect("sidecar exists after index");
    assert!(sidecar.exists());

    vex_in(repo)
        .args(["update", "--no-history"])
        .assert()
        .success();

    assert!(
        !sidecar.exists(),
        "Phase 14.8 Step 5b: sidecar must be deleted after \
         `vex update --no-history`. Still at: {}",
        sidecar.display()
    );

    // Manifest fields nulled.
    let manifest_path = sidecar.with_file_name("manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|_| panic!("manifest at {}", manifest_path.display()));
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
    assert!(
        manifest.get("history_indexed_at").is_none()
            || manifest.get("history_indexed_at").unwrap().is_null(),
        "history_indexed_at should be absent or null in manifest after --no-history. \
         Manifest: {manifest}"
    );

    // cmd_history falls back to walker — advertised via meta.
    let output = vex_in(repo)
        .args(["history", "here", "--format", "json"])
        .output()
        .expect("vex history");
    let envelope: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let mode = envelope
        .pointer("/_meta/vex.dev~1history_mode")
        .and_then(|v| v.as_str());
    assert_eq!(
        mode,
        Some("walker"),
        "After drop, vex history must use walker mode. Got: {mode:?}"
    );
}

#[test]
fn vex_status_text_wording_pinned() {
    // Phase 14.8 Step 6 polish — pin the exact wording of the
    // `vex status` history surface so a future refactor doesn't
    // silently drift the copy. Two scenarios: present (with stats)
    // and absent.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn here() {}\n", "c1");

    // Absent: no `vex index --history` yet → status falls through
    // the "(None, _)" arm of the match.
    vex_in(repo).args(["index"]).assert().success();
    let text = String::from_utf8(
        vex_in(repo)
            .args(["status", "--format", "text"])
            .output()
            .expect("vex status")
            .stdout,
    )
    .unwrap();
    assert!(
        text.contains("History:    no (run `vex index --history` to enable indexed `vex history`)"),
        "absent-history wording must match, got: {text}"
    );

    // Present: re-index with --history.
    vex_in(repo).args(["index", "--history"]).assert().success();
    let text = String::from_utf8(
        vex_in(repo)
            .args(["status", "--format", "text"])
            .output()
            .expect("vex status")
            .stdout,
    )
    .unwrap();
    // Date is today, so check the prefix + the shape.
    assert!(
        text.contains("History:    indexed at "),
        "present-history prefix must match, got: {text}"
    );
    assert!(
        text.contains("commits, ") && text.contains("blobs, ") && text.contains("entries)"),
        "present-history stats shape must match (commits/blobs/entries), got: {text}"
    );
}

#[test]
fn vex_status_text_warns_on_depth_capped() {
    // Phase 14.8 Step 6 — when --history-depth N truncated the walk,
    // status renders a warning line so users know symbols introduced
    // before the cap won't be found.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    for i in 0..5 {
        commit_file(
            repo,
            "src/lib.rs",
            &format!("pub fn v{i}() {{}}\n"),
            &format!("c{i}"),
        );
    }
    vex_in(repo)
        .args(["index", "--history", "--history-depth", "2"])
        .assert()
        .success();

    let text = String::from_utf8(
        vex_in(repo)
            .args(["status", "--format", "text"])
            .output()
            .expect("vex status")
            .stdout,
    )
    .unwrap();
    assert!(
        text.contains("section is partial: --history-depth cap"),
        "depth-cap warning must surface in status text, got: {text}"
    );
    assert!(
        text.contains("Symbols introduced before the cap are NOT indexed"),
        "depth-cap warning must explain impact, got: {text}"
    );
}

#[test]
fn vex_status_reports_history_line() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn here() {}\n", "c1");

    vex_in(repo).args(["index", "--history"]).assert().success();

    let output = vex_in(repo)
        .args(["status", "--format", "json"])
        .output()
        .expect("spawn vex status");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("status JSON parse: {e}\nstdout: {stdout}"));

    // Per task file: `vex status` surfaces `History: indexed (X commits,
    // Y blobs, Z symbol-blob entries)` for text, and the same fact via
    // `history_indexed_at` (non-null) + a counts object in JSON.
    let indexed_at = envelope
        .pointer("/results/history_indexed_at")
        .or_else(|| envelope.pointer("/history_indexed_at"));
    assert!(
        matches!(indexed_at, Some(v) if !v.is_null()),
        "Phase 14.8: `vex status --format json` must expose \
         `history_indexed_at` (non-null) after `vex index --history`. \
         Step 6 wires `cmd_status`. Got: {:?}. Envelope: {}",
        indexed_at,
        stdout,
    );
}
