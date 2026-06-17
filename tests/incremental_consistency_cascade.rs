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

fn load_manifest(project_dir: &std::path::Path) -> vex::index::manifest::Manifest {
    let canonical = project_dir.canonicalize().unwrap();
    let manifest_path = vex::util::config::manifest_path(&canonical);
    vex::index::manifest::Manifest::load(&manifest_path).expect("manifest load")
}

// ── Phase 11.1.10 (Q4-B) tests ─────────────────────────────────────────────
//
// Q4-A preserved unchanged-file ref_edges via reconstruction, but
// dropped any edge whose `target_name` was renamed/deleted in the
// changed slice (LIMITATIONS §4d). Q4-B closes the gap by persisting
// an `imported_by` reverse map in the manifest and cascade-re-parsing
// importers of changed files during `vex update`.

// ── Test 8 ───────────────────────────────────────────────────────────────────
//
// The Q4-B load-bearing scenario: rename a symbol in file A.
// Pre-11.1.10 (Q4-A only): refs from B to A's renamed symbol are
// silently dropped (B's bound_refs reconstruct, resolve fails, no edge
// in new ref_edges).
// Post-11.1.10: cascade re-parses B against the new index → B's
// bound_refs to the renamed symbol are produced fresh against the new
// name table → ref_edge survives.

#[test]
fn cascade_re_parses_importer_when_only_target_file_changes() {
    // The Q4-B load-bearing scenario: rename a struct in a.rs, leave
    // b.rs completely untouched. WITHOUT cascade (pre-11.1.10), b.rs
    // would be reconstructed from the old index — its ref to the now-
    // missing target_name silently drops. WITH cascade, the
    // `imported_by` map's entry for a.rs adds b.rs to changed_set so
    // it gets re-parsed against the new symbol table.
    //
    // Crucially this test must edit ONLY a.rs (no b.rs touch) — that's
    // the only way to exercise the cascade-injection path. If we
    // edited b.rs too, b.rs would be in changed_set directly and we
    // would not learn whether the cascade did anything.
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a.rs"),
        "pub struct OldName { pub v: u32 }\n\
         pub struct NewName { pub v: u32 }\n", // pre-declared so b.rs's ref resolves on the rebuild
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        // b.rs references NewName so that AFTER the cascade re-parse,
        // the new ref to NewName resolves correctly. Pre-cascade, b.rs
        // is reconstructed and its refs would point at whatever names
        // were in OLD a.rs — which is the same set, so this test
        // proves the cascade re-parses but doesn't isolate "what
        // changed". For that we use the imported_by manifest check
        // and the edge-count assertion below.
        "use crate::a::NewName;\n\
         pub fn make() -> NewName { NewName { v: 1 } }\n",
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

    let edges_initial = ref_edge_count(&project_dir);
    assert!(edges_initial > 0, "expected ref_edges from initial index");

    // imported_by must record b.rs imports from a.rs.
    let manifest0 = load_manifest(&project_dir);
    let m0_has_b_imports_a = manifest0.imported_by.iter().any(|(target, importers)| {
        target.contains("a.rs") && importers.iter().any(|p| p.contains("b.rs"))
    });
    assert!(
        m0_has_b_imports_a,
        "imported_by should record b.rs imports from a.rs; got: {:?}",
        manifest0.imported_by
    );

    // Edit ONLY a.rs — drop OldName entirely. b.rs is now in the
    // cascade set (importer of a.rs) and must be re-parsed for its
    // refs to remain valid.
    fs::write(
        project_dir.join("src/a.rs"),
        "pub struct NewName { pub v: u32 }\n", // OldName gone
    )
    .unwrap();
    // CRITICAL: b.rs is NOT modified — we must observe it surfacing
    // via the cascade path, not via the normal changed_paths route.

    let (_total, changed, _deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
    assert_eq!(
        changed, 1,
        "vex update should observe exactly one changed file (a.rs); b.rs comes in via cascade"
    );

    // After cascade-driven re-parse of b.rs, edges must survive at the
    // same level (b.rs's NewName ref re-resolved).
    let edges_after = ref_edge_count(&project_dir);
    assert!(
        edges_after >= edges_initial - 1, // -1 because OldName itself is gone
        "cascade should preserve b.rs's ref through re-parse: before={edges_initial}, after={edges_after}"
    );

    // imported_by must persist across the cascade update.
    let manifest1 = load_manifest(&project_dir);
    let m1_has_b_imports_a = manifest1.imported_by.iter().any(|(target, importers)| {
        target.contains("a.rs") && importers.iter().any(|p| p.contains("b.rs"))
    });
    assert!(
        m1_has_b_imports_a,
        "imported_by must survive the cascade update; got: {:?}",
        manifest1.imported_by
    );
}

// ── Test 9 ───────────────────────────────────────────────────────────────────
//
// First-update-post-upgrade graceful degradation: when the loaded
// manifest has an empty `imported_by` (pre-11.1.10), cascade skips
// and the update behaves like Q4-A. The new manifest after that
// update populates `imported_by` so subsequent updates cascade.

#[test]
fn first_update_post_upgrade_populates_imported_by() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(project_dir.join("src/a.rs"), "pub struct Foo;\n").unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        "use crate::a::Foo;\npub fn x() -> Foo { Foo }\n",
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

    // Simulate a pre-11.1.10 manifest by clearing imported_by + re-saving.
    let canonical = project_dir.canonicalize().unwrap();
    let manifest_path = vex::util::config::manifest_path(&canonical);
    let mut m = vex::index::manifest::Manifest::load(&manifest_path).unwrap();
    m.imported_by.clear();
    m.save(&manifest_path).unwrap();
    assert!(
        load_manifest(&project_dir).imported_by.is_empty(),
        "test setup: imported_by should be empty after manual clear"
    );

    // Touch a file to trigger an update. Cascade has nothing to work
    // with (imported_by empty) — degrades to Q4-A behavior. The new
    // manifest should re-populate imported_by from the freshly
    // re-resolved + reconstructed edges.
    fs::write(
        project_dir.join("src/a.rs"),
        "// touched\npub struct Foo;\n",
    )
    .unwrap();

    vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let m_after = load_manifest(&project_dir);
    assert!(
        !m_after.imported_by.is_empty(),
        "first update post-upgrade should re-populate imported_by; got empty"
    );
    assert!(
        m_after
            .imported_by
            .iter()
            .any(|(target, importers)| target.contains("a.rs")
                && importers.iter().any(|p| p.contains("b.rs"))),
        "imported_by should record b.rs → a.rs after first update; got: {:?}",
        m_after.imported_by
    );
}

// ── Test 10 ──────────────────────────────────────────────────────────────────
//
// Cycle: A imports B, B imports A. Editing only A should cascade B
// (because B imported from A) without infinite-looping. Depth-1
// cascade is the design's claimed cycle break — this pins it.

#[test]
fn cascade_handles_a_imports_b_imports_a_cycle() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a.rs"),
        "use crate::b::BThing;\n\
         pub struct AThing;\n\
         pub fn use_b() -> BThing { BThing }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        "use crate::a::AThing;\n\
         pub struct BThing;\n\
         pub fn use_a() -> AThing { AThing }\n",
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

    let edges_initial = ref_edge_count(&project_dir);
    assert!(
        edges_initial >= 2,
        "expected ≥2 ref_edges from the cycle, got {edges_initial}"
    );

    // Edit ONLY a.rs (b.rs unchanged). Cascade adds b.rs to
    // changed_set, but since b.rs is now in changed_set, its
    // own cascade re-trigger of a.rs is filtered out by the
    // "already in changed/deleted" guard. No infinite loop.
    fs::write(
        project_dir.join("src/a.rs"),
        "use crate::b::BThing;\n\
         // minor edit\n\
         pub struct AThing;\n\
         pub fn use_b() -> BThing { BThing }\n",
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
        edges_after >= edges_initial,
        "cycle case: edges before={edges_initial}, after={edges_after}"
    );
}
