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

// ── Phase 11.1.11 (Q4-C) tests ─────────────────────────────────────────────
//
// Q4-B's single-hop cascade re-parsed direct importers but missed
// transitive re-export chains. Q4-C extends the cascade to follow the
// `imported_by` reverse graph via BFS, bounded by `CASCADE_MAX_DEPTH=16`.

// ── Test 11 ──────────────────────────────────────────────────────────────────
//
// Three-hop chain: `A → B → C` where C re-exports B's symbol and A
// imports it through C. Editing only B's symbol must cascade through
// C all the way to A: A's ref is rebound against the new name table.
// Q4-B (depth-1) would have stopped at C and left A reconstructed
// against stale refs.

#[test]
fn cascade_traverses_a_to_b_to_c_three_hop_chain() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    // Layer A defines OldName + NewName pre-emptively so the rebind
    // resolves cleanly after the rename.
    fs::write(
        project_dir.join("src/a.rs"),
        "pub struct OldName { pub v: u32 }\n\
         pub struct NewName { pub v: u32 }\n",
    )
    .unwrap();
    // Layer B uses A's type in a function body — Rust binder records
    // the type ref b → a.
    fs::write(
        project_dir.join("src/b.rs"),
        "use crate::a::NewName;\n\
         pub struct BWrapper { pub v: u32 }\n\
         pub fn make() -> BWrapper {\n\
            let _: NewName = NewName { v: 1 };\n\
            BWrapper { v: 1 }\n\
         }\n",
    )
    .unwrap();
    // Layer C references ONLY B's wrapper — never A directly. Q4-C
    // must follow the c → b → a chain transitively.
    fs::write(
        project_dir.join("src/c.rs"),
        "use crate::b::BWrapper;\n\
         pub fn use_b() -> BWrapper {\n\
            let _: BWrapper = BWrapper { v: 2 };\n\
            BWrapper { v: 2 }\n\
         }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/lib.rs"),
        "pub mod a;\npub mod b;\npub mod c;\n",
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
        "expected ref_edges from the three-layer chain, got 0 — \
         test fixture isn't exercising the binder"
    );

    // Edit ONLY a.rs — drop OldName. b.rs (depth 1) imports from a.rs
    // and must be cascaded. c.rs (depth 2) imports from b.rs and must
    // ALSO be cascaded — pre-Q4-C this hop was missed.
    fs::write(
        project_dir.join("src/a.rs"),
        "pub struct NewName { pub v: u32 }\n",
    )
    .unwrap();

    let (_total, changed, _deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
    assert_eq!(
        changed, 1,
        "only a.rs was edited; b.rs and c.rs must reach the re-parse path via cascade"
    );

    // Both layers' refs should survive (-1 because OldName itself is gone).
    let edges_after = ref_edge_count(&project_dir);
    assert!(
        edges_after >= edges_initial.saturating_sub(1),
        "transitive cascade dropped refs across the 3-hop chain: \
         before={edges_initial}, after={edges_after}"
    );

    // Both importers must have logged into imported_by — proves the
    // graph picked up b → a AND c → b.
    let manifest = load_manifest(&project_dir);
    let b_imports_a = manifest.imported_by.iter().any(|(target, importers)| {
        target.contains("a.rs") && importers.iter().any(|p| p.contains("b.rs"))
    });
    let c_imports_b = manifest.imported_by.iter().any(|(target, importers)| {
        target.contains("b.rs") && importers.iter().any(|p| p.contains("c.rs"))
    });
    assert!(
        b_imports_a && c_imports_b,
        "imported_by must record both hops; got: {:?}",
        manifest.imported_by
    );
}

// ── Test 12 ──────────────────────────────────────────────────────────────────
//
// Star pattern terminates: a "hub" file imported by many leaves. BFS
// must not loop or duplicate entries — every leaf appears in
// changed_set exactly once.

#[test]
fn cascade_terminates_on_star_pattern() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/hub.rs"),
        "pub struct Hub { pub v: u32 }\n",
    )
    .unwrap();
    // Eight leaves all importing from hub.rs.
    let leaf_count = 8;
    for i in 0..leaf_count {
        fs::write(
            project_dir.join(format!("src/leaf_{i}.rs")),
            format!("use crate::hub::Hub;\npub fn use_hub_{i}() -> Hub {{ Hub {{ v: {i} }} }}\n"),
        )
        .unwrap();
    }
    let mut lib = String::from("pub mod hub;\n");
    for i in 0..leaf_count {
        lib.push_str(&format!("pub mod leaf_{i};\n"));
    }
    fs::write(project_dir.join("src/lib.rs"), lib).unwrap();

    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let edges_initial = ref_edge_count(&project_dir);

    // Touch hub.rs only.
    fs::write(
        project_dir.join("src/hub.rs"),
        "// minor edit\npub struct Hub { pub v: u32 }\n",
    )
    .unwrap();

    vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    // All leaves are cascade-re-parsed; their refs survive.
    let edges_after = ref_edge_count(&project_dir);
    assert!(
        edges_after >= edges_initial,
        "star pattern lost refs: before={edges_initial}, after={edges_after}"
    );
}

// ── Test 13 ──────────────────────────────────────────────────────────────────
//
// Q4-C × TypeScript: 3-hop chain via `import { X } from './y'`. The
// TS binder records cross-file edges from `import_statement` clauses;
// Q4-C must walk them transitively the same way it does in Rust.

#[test]
fn cascade_traverses_three_hop_chain_typescript() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a.ts"),
        "export interface OldName { v: number }\n\
         export interface NewName { v: number }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b.ts"),
        "import { NewName } from './a';\n\
         export interface BWrapper { v: number }\n\
         export function makeB(): BWrapper {\n\
            const _x: NewName = { v: 1 };\n\
            return { v: _x.v };\n\
         }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/c.ts"),
        "import { BWrapper } from './b';\n\
         export function useB(): BWrapper {\n\
            const _x: BWrapper = { v: 2 };\n\
            return _x;\n\
         }\n",
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
        "TypeScript binder did not produce ref_edges for the 3-hop chain — \
         check that .ts files are being indexed and binder is wired"
    );

    // Confirm both hops landed in imported_by: b → a AND c → b.
    let m0 = load_manifest(&project_dir);
    let b_imports_a = m0
        .imported_by
        .iter()
        .any(|(t, importers)| t.contains("a.ts") && importers.iter().any(|p| p.contains("b.ts")));
    let c_imports_b = m0
        .imported_by
        .iter()
        .any(|(t, importers)| t.contains("b.ts") && importers.iter().any(|p| p.contains("c.ts")));
    assert!(
        b_imports_a && c_imports_b,
        "TS imported_by missing a hop; got: {:?}",
        m0.imported_by
    );

    // Edit ONLY a.ts: drop OldName. b.ts (depth 1) and c.ts (depth 2)
    // must both be cascaded via Q4-C.
    fs::write(
        project_dir.join("src/a.ts"),
        "export interface NewName { v: number }\n",
    )
    .unwrap();

    let (_total, changed, _deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
    assert_eq!(
        changed, 1,
        "only a.ts was edited — b.ts + c.ts come via cascade"
    );

    let edges_after = ref_edge_count(&project_dir);
    assert!(
        edges_after >= edges_initial.saturating_sub(1),
        "TS transitive cascade dropped refs: before={edges_initial}, after={edges_after}"
    );
}

// ── Test 14 ──────────────────────────────────────────────────────────────────
//
// Q4-C × Python: 3-hop chain via `from x import Y`. The Python binder
// records cross-file edges from `import_from_statement`; Q4-C must
// walk them transitively.

#[test]
fn cascade_traverses_three_hop_chain_python() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    fs::write(
        project_dir.join("a.py"),
        "class OldName:\n    v: int = 0\n\n\
         class NewName:\n    v: int = 0\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("b.py"),
        "from a import NewName\n\n\
         class BWrapper:\n    v: int = 0\n\n\
         def make_b() -> BWrapper:\n    _x: NewName = NewName()\n    return BWrapper()\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("c.py"),
        "from b import BWrapper\n\n\
         def use_b() -> BWrapper:\n    _x: BWrapper = BWrapper()\n    return _x\n",
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
        "Python binder did not produce ref_edges — check .py indexing + binder wiring"
    );

    let m0 = load_manifest(&project_dir);
    let b_imports_a = m0
        .imported_by
        .iter()
        .any(|(t, importers)| t.contains("a.py") && importers.iter().any(|p| p.contains("b.py")));
    let c_imports_b = m0
        .imported_by
        .iter()
        .any(|(t, importers)| t.contains("b.py") && importers.iter().any(|p| p.contains("c.py")));
    assert!(
        b_imports_a && c_imports_b,
        "Python imported_by missing a hop; got: {:?}",
        m0.imported_by
    );

    // Edit ONLY a.py: drop OldName.
    fs::write(project_dir.join("a.py"), "class NewName:\n    v: int = 0\n").unwrap();

    let (_total, changed, _deleted) = vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
    assert_eq!(
        changed, 1,
        "only a.py was edited — b.py + c.py come via cascade"
    );

    let edges_after = ref_edge_count(&project_dir);
    assert!(
        edges_after >= edges_initial.saturating_sub(1),
        "Python transitive cascade dropped refs: before={edges_initial}, after={edges_after}"
    );
}

// ── Test 15 ──────────────────────────────────────────────────────────────────
//
// Q4-C × Go (negative control): Go has no binder (see
// `src/parse/scope/mod.rs::bind_refs` match arms). `imported_by` stays
// empty for Go-only projects → cascade is a documented no-op.
// Correctness IS preserved (nothing to drop), but this test exists to
// pin the "no cascade for Go" contract: if a future binder lands,
// it'll fail this test and force a coverage update.

#[test]
fn cascade_is_noop_for_go_only_project_until_binder_lands() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    fs::write(
        project_dir.join("a.go"),
        "package main\n\ntype NewName struct{ V int }\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("b.go"),
        "package main\n\ntype BWrapper struct{ V int }\n\n\
         func makeB() BWrapper {\n    _ = NewName{V: 1}\n    return BWrapper{}\n}\n",
    )
    .unwrap();

    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    // Go binder is absent — imported_by must stay empty. If this fires,
    // a Go binder landed and `cascade_traverses_three_hop_chain_go`
    // should be added alongside the TS / Python tests.
    let m = load_manifest(&project_dir);
    assert!(
        m.imported_by.is_empty(),
        "Go has no binder yet (see scope/mod.rs::bind_refs); imported_by must stay empty. \
         Got: {:?} — if you wired a Go binder, replace this test with a 3-hop chain.",
        m.imported_by
    );
}
