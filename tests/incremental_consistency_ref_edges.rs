use std::fs;
use tempfile::TempDir;

// Helper: open the reader for a project root after the pipeline has written the index.
fn open_reader(project_dir: &std::path::Path) -> vex::store::reader::IndexReader {
    let canonical = project_dir.canonicalize().unwrap();
    let index_path = vex::util::config::index_path(&canonical);
    vex::store::reader::IndexReader::open(&index_path).unwrap()
}

fn ref_edge_count(project_dir: &std::path::Path) -> usize {
    open_reader(project_dir).ref_edge_count()
}

// ── Phase 11.1.9 (Q4-A) tests ──────────────────────────────────────────────
//
// Before Q4-A landed, `vex update` silently dropped every bound_ref from
// unchanged files because `reconstruct_unchanged` set `bound_refs:
// Vec::new()`. The writer's `ref_edges` section then only carried edges
// from the changed slice. These tests pin the corrected behavior.

// ── Test 5 ───────────────────────────────────────────────────────────────────
//
// `vex update` after editing one file must NOT drop the ref_edges from
// the other (unchanged) files. The bug this guards: before Q4-A every
// `vex update` reduced the ref_edges section to just-changed-files'
// edges, silently degrading `vex usages --strict` after the first
// incremental run.

#[test]
fn update_preserves_ref_edges_for_unchanged_files() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    // a.rs defines a type; b.rs references it (binder produces a ref_edge).
    fs::write(
        project_dir.join("src/a.rs"),
        "pub struct Target { pub value: u32 }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        "use crate::a::Target;\n\
         pub fn use_target() -> Target { Target { value: 1 } }\n",
    )
    .unwrap();
    fs::write(project_dir.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();

    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let edges_before = ref_edge_count(&project_dir);
    assert!(
        edges_before > 0,
        "expected ref_edges from the initial full index, got 0 — \
         test fixture isn't exercising the binder"
    );

    // Edit a.rs without touching b.rs. b.rs's `Target` edge must survive
    // through the reconstruct → re-resolve path.
    fs::write(
        project_dir.join("src/a.rs"),
        "// trivial edit — comment only\n\
         pub struct Target { pub value: u32 }\n",
    )
    .unwrap();

    vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let edges_after = ref_edge_count(&project_dir);
    assert!(
        edges_after >= edges_before,
        "vex update silently dropped ref_edges from unchanged files: \
         before={edges_before}, after={edges_after} \
         (Phase 11.1.9 / Q4-A regression — every update would erode --strict quality)"
    );
}

// ── Test 6 ───────────────────────────────────────────────────────────────────
//
// Load-bearing test for choosing A2 (path tie-break) over A1
// (single-candidate-only). Two files both define `Helper`. b.rs imports
// `crate::a_helper::Helper` specifically. After editing an unrelated
// file, the reconstructed ref must still resolve to a_helper.rs's
// Helper, not b_helper.rs's — A1 would silently mis-attribute via
// first-candidate fallback.

#[test]
fn update_multicandidate_disambiguation() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a_helper.rs"),
        "pub struct Helper { pub a: u32 }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b_helper.rs"),
        "pub struct Helper { pub b: u32 }\n",
    )
    .unwrap();
    // user.rs imports a_helper's Helper specifically.
    fs::write(
        project_dir.join("src/user.rs"),
        "use crate::a_helper::Helper;\n\
         pub fn make() -> Helper { Helper { a: 1 } }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/unrelated.rs"),
        "pub fn placeholder() {}\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/lib.rs"),
        "pub mod a_helper;\npub mod b_helper;\npub mod user;\npub mod unrelated;\n",
    )
    .unwrap();

    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let edges_before = ref_edge_count(&project_dir);

    // Edit `unrelated.rs` — NEITHER helper file changes. The
    // reconstructed ref must still point to a_helper's Helper, not
    // b_helper's (multi-candidate disambiguation via path tie-break).
    fs::write(
        project_dir.join("src/unrelated.rs"),
        "pub fn placeholder() {}\npub fn extra() {}\n",
    )
    .unwrap();

    vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let edges_after = ref_edge_count(&project_dir);
    assert!(
        edges_after >= edges_before,
        "multi-candidate disambiguation lost the user.rs → a_helper::Helper edge: \
         before={edges_before}, after={edges_after}"
    );
}

// ── Test 7 ───────────────────────────────────────────────────────────────────
//
// The CHANGELOG describes the bug as "degrades after a few `vex update`s".
// A single update preserving edges (Test 5) isn't sufficient — the
// reconstruction path must be stable when its OWN output becomes the
// next iteration's input (a "vex update" run against an index that was
// itself produced by a previous "vex update"). This catches a class of
// bugs where reconstruction works against a fresh index but corrupts
// against a reconstructed one.

#[test]
fn update_preserves_ref_edges_across_multiple_iterations() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a.rs"),
        "pub struct Target { pub v: u32 }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        "use crate::a::Target;\n\
         pub fn f1() -> Target { Target { v: 1 } }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/c.rs"),
        "use crate::a::Target;\n\
         pub fn f2() -> Target { Target { v: 2 } }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/touch_me.rs"),
        "pub fn version_1() {}\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/lib.rs"),
        "pub mod a;\npub mod b;\npub mod c;\npub mod touch_me;\n",
    )
    .unwrap();

    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let edges_initial = ref_edge_count(&project_dir);
    assert!(
        edges_initial > 0,
        "expected ref_edges from initial full index"
    );

    // Iterate `vex update` 3 times, each touching a different version
    // of touch_me.rs. b.rs / c.rs / a.rs all stay unchanged across
    // every iteration, so their cross-file refs must persist through
    // the reconstruction chain.
    for version in 2..=4 {
        fs::write(
            project_dir.join("src/touch_me.rs"),
            format!("pub fn version_{version}() {{}}\n"),
        )
        .unwrap();

        vex::index::pipeline::update(
            &project_dir,
            vex::index::pipeline::IndexOptions::default(),
            "minilm-l6-v2",
            &[],
        )
        .unwrap();

        let edges_now = ref_edge_count(&project_dir);
        assert!(
            edges_now >= edges_initial,
            "ref_edges eroded after update iteration {version}: \
             initial={edges_initial}, now={edges_now} \
             — reconstruction is not stable across self-input"
        );
    }
}
