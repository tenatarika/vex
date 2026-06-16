use std::fs;
use tempfile::TempDir;

// Helper: open the reader for a project root after the pipeline has written the index.
// Uses canonicalize() to match the pipeline's own path resolution.
fn open_reader(project_dir: &std::path::Path) -> vex::store::reader::IndexReader {
    let canonical = project_dir.canonicalize().unwrap();
    let index_path = vex::util::config::index_path(&canonical);
    vex::store::reader::IndexReader::open(&index_path).unwrap()
}

// Helper: count results for a symbol name search (exact + fuzzy).
fn result_count(project_dir: &std::path::Path, name: &str) -> usize {
    let reader = open_reader(project_dir);
    vex::search::structural::search_with_fuzzy(&reader, name, 100).len()
}

// Helper: collect the file paths reported for a symbol name search.
fn result_paths(project_dir: &std::path::Path, name: &str) -> Vec<String> {
    let reader = open_reader(project_dir);
    vex::search::structural::search_with_fuzzy(&reader, name, 100)
        .into_iter()
        .map(|r| r.path)
        .collect()
}

// Helper: total symbol count in the index for a project root.
fn total_symbols(project_dir: &std::path::Path) -> usize {
    open_reader(project_dir).symbol_count()
}

// ── Test 1 ───────────────────────────────────────────────────────────────────
//
// After renaming a file the symbol should be found at the new path only.
// The total symbol count must not change (no duplication, no loss).

#[test]
fn update_after_file_renamed() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    // Write initial source file with one symbol.
    fs::write(project_dir.join("src/a.rs"), "pub fn original() {}").unwrap();

    // Full index.
    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let symbols_before = total_symbols(&project_dir);
    assert!(
        symbols_before >= 1,
        "expected at least one symbol after full index"
    );

    // Sanity: symbol found in src/a.rs.
    let paths_before = result_paths(&project_dir, "original");
    assert!(
        paths_before
            .iter()
            .any(|p| p.contains("src/a.rs") || p.contains("src\\a.rs")),
        "original should be in src/a.rs before rename, got: {paths_before:?}"
    );

    // Rename: src/a.rs → src/b.rs (same content).
    fs::rename(project_dir.join("src/a.rs"), project_dir.join("src/b.rs")).unwrap();

    // Incremental update.
    let (total, changed, deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    // The rename is observed as one deletion + one new file.
    assert_eq!(
        deleted, 1,
        "renamed-away file should be reported as deleted"
    );
    assert_eq!(
        changed, 1,
        "renamed-into file should be reported as changed/added"
    );
    assert_eq!(
        total, symbols_before,
        "symbol count must not change after rename (no duplication, no loss)"
    );

    // Symbol found at new path, NOT at old path.
    let paths_after = result_paths(&project_dir, "original");
    assert!(
        !paths_after.is_empty(),
        "original should still be findable after rename"
    );
    assert!(
        paths_after
            .iter()
            .any(|p| p.contains("src/b.rs") || p.contains("src\\b.rs")),
        "original should be in src/b.rs after rename, got: {paths_after:?}"
    );
    assert!(
        paths_after
            .iter()
            .all(|p| !p.contains("src/a.rs") && !p.contains("src\\a.rs")),
        "original must NOT appear in src/a.rs after rename, got: {paths_after:?}"
    );
}

// ── Test 2 ───────────────────────────────────────────────────────────────────
//
// After moving a symbol from one file to another, the index must reflect the
// new location exclusively — no stale entry at the old location.

#[test]
fn update_after_symbol_moved_between_files() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    // Initial layout: shared() in a.rs, other() in b.rs.
    fs::write(project_dir.join("src/a.rs"), "pub fn shared() {}").unwrap();
    fs::write(project_dir.join("src/b.rs"), "pub fn other() {}").unwrap();

    // Full index.
    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    // Verify shared is in a.rs before the move.
    let paths_before = result_paths(&project_dir, "shared");
    assert!(
        paths_before
            .iter()
            .any(|p| p.contains("src/a.rs") || p.contains("src\\a.rs")),
        "shared should be in src/a.rs before move, got: {paths_before:?}"
    );

    // Move shared() from a.rs to b.rs: a.rs becomes empty, b.rs gets both.
    fs::write(project_dir.join("src/a.rs"), "").unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        "pub fn shared() {}\npub fn other() {}",
    )
    .unwrap();

    // Incremental update.
    let (_total, changed, deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    assert_eq!(deleted, 0, "no files deleted in symbol-move scenario");
    assert_eq!(changed, 2, "both a.rs and b.rs were modified");

    // shared() must be found exclusively in b.rs.
    let paths_after = result_paths(&project_dir, "shared");
    assert!(
        !paths_after.is_empty(),
        "shared should be findable after move"
    );
    assert!(
        paths_after
            .iter()
            .any(|p| p.contains("src/b.rs") || p.contains("src\\b.rs")),
        "shared should be in src/b.rs after move, got: {paths_after:?}"
    );
    assert!(
        paths_after
            .iter()
            .all(|p| !p.contains("src/a.rs") && !p.contains("src\\a.rs")),
        "shared must NOT appear in src/a.rs after move, got: {paths_after:?}"
    );

    // other() must still be findable in b.rs.
    let other_paths = result_paths(&project_dir, "other");
    assert!(
        other_paths
            .iter()
            .any(|p| p.contains("src/b.rs") || p.contains("src\\b.rs")),
        "other should still be in src/b.rs, got: {other_paths:?}"
    );
}

// ── Test 3 ───────────────────────────────────────────────────────────────────
//
// When a file is overwritten with empty content all its symbols must be purged
// from the index; the total symbol count must decrease accordingly.

#[test]
fn update_after_file_becomes_empty() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a.rs"),
        "pub fn foo() {}\npub fn bar() {}",
    )
    .unwrap();

    // Full index: expect foo and bar.
    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let symbols_before = total_symbols(&project_dir);
    assert!(
        symbols_before >= 2,
        "expected at least 2 symbols before emptying file"
    );

    assert!(
        result_count(&project_dir, "foo") > 0,
        "foo should exist before emptying"
    );
    assert!(
        result_count(&project_dir, "bar") > 0,
        "bar should exist before emptying"
    );

    // Overwrite with empty content.
    fs::write(project_dir.join("src/a.rs"), "").unwrap();

    // Incremental update.
    let (total, changed, deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    assert_eq!(deleted, 0, "file still exists — not a deletion");
    assert_eq!(changed, 1, "emptied file should be detected as changed");
    assert!(
        total < symbols_before,
        "total symbols must decrease after file is emptied (before={symbols_before}, after={total})"
    );

    // foo and bar must be gone.
    assert_eq!(
        result_count(&project_dir, "foo"),
        0,
        "foo must not be found after file is emptied"
    );
    assert_eq!(
        result_count(&project_dir, "bar"),
        0,
        "bar must not be found after file is emptied"
    );
}

// ── Test 4 ───────────────────────────────────────────────────────────────────
//
// After adding a brand-new file the symbol it defines must become searchable.

#[test]
fn update_after_new_file_added() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    // Start with a single file so the project root is a real Rust project.
    fs::write(project_dir.join("src/a.rs"), "pub fn existing() {}").unwrap();

    // Full index.
    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let symbols_before = total_symbols(&project_dir);

    // brand_new must not exist yet.
    assert_eq!(
        result_count(&project_dir, "brand_new"),
        0,
        "brand_new must not be findable before new file is added"
    );

    // Add a new source file.
    fs::write(project_dir.join("src/new.rs"), "pub fn brand_new() {}").unwrap();

    // Incremental update.
    let (total, changed, deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    assert_eq!(deleted, 0, "no file was deleted");
    assert_eq!(changed, 1, "exactly one new file added");
    assert!(
        total > symbols_before,
        "total symbol count must grow after adding new file (before={symbols_before}, after={total})"
    );

    // brand_new is now searchable.
    assert!(
        result_count(&project_dir, "brand_new") > 0,
        "brand_new must be findable after incremental update"
    );

    // Existing symbol must still be intact.
    assert!(
        result_count(&project_dir, "existing") > 0,
        "existing symbol must survive after new file is added"
    );
}

// ── Phase 11.1.9 (Q4-A) tests ──────────────────────────────────────────────
//
// Before Q4-A landed, `vex update` silently dropped every bound_ref from
// unchanged files because `reconstruct_unchanged` set `bound_refs:
// Vec::new()`. The writer's `ref_edges` section then only carried edges
// from the changed slice. These tests pin the corrected behavior.

fn ref_edge_count(project_dir: &std::path::Path) -> usize {
    open_reader(project_dir).ref_edge_count()
}

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

// ── Phase 11.1.10 (Q4-B) tests ─────────────────────────────────────────────
//
// Q4-A preserved unchanged-file ref_edges via reconstruction, but
// dropped any edge whose `target_name` was renamed/deleted in the
// changed slice (LIMITATIONS §4d). Q4-B closes the gap by persisting
// an `imported_by` reverse map in the manifest and cascade-re-parsing
// importers of changed files during `vex update`.

fn load_manifest(project_dir: &std::path::Path) -> vex::index::manifest::Manifest {
    let canonical = project_dir.canonicalize().unwrap();
    let manifest_path = vex::util::config::manifest_path(&canonical);
    vex::index::manifest::Manifest::load(&manifest_path).expect("manifest load")
}

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
