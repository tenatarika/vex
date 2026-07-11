//! P2a (`docs/HIERARCHY-EDGES.md` §8, architect CRITICAL-1) regression
//! suite — `vex update` carry-forward of typed hierarchy edges
//! (`extends`/`implements`/`uses`) for UNCHANGED files.
//!
//! Before P2a, `reconstruct_unchanged` rebuilt every unchanged file's
//! `ParsedFile` with `hierarchy_captures: Vec::new()` (no re-parse), so the
//! writer's `resolve_hierarchy_captures` pass produced nothing for them —
//! the FIRST `vex update` after shipping P2 would silently drop every
//! unchanged file's hierarchy edges. These tests pin the corrected
//! behavior using the same `pipeline::run` / `pipeline::update` harness as
//! `incremental_consistency_ref_edges.rs` (the ref-edge equivalent, Q4-A).

use std::fs;
use tempfile::TempDir;

fn open_reader(project_dir: &std::path::Path) -> vex::store::reader::IndexReader {
    let canonical = project_dir.canonicalize().unwrap();
    let index_path = vex::util::config::index_path(&canonical);
    vex::store::reader::IndexReader::open(&index_path).unwrap()
}

/// Resolve `name` to its `SymbolRecord` position by scanning the on-disk
/// symbol records. Returns `None` when not found (the symbol may have
/// moved/renamed across the update under test).
fn find_symbol_idx(reader: &vex::store::reader::IndexReader, name: &str) -> Option<u32> {
    for i in 0..reader.symbol_count() {
        if let Some(rec) = reader.symbol(i) {
            if reader.read_string(rec.name_offset) == name {
                return Some(i as u32);
            }
        }
    }
    None
}

// ── Headline test ───────────────────────────────────────────────────────
//
// file A defines `trait Base`; file B defines `struct Derived` with
// `impl Base for Derived` (a hierarchy edge child=Derived, parent=Base).
// `vex update` touches an UNRELATED third file C. B's edge must survive —
// this is the exact regression P2a exists to prevent.

#[test]
fn update_preserves_hierarchy_edge_for_unchanged_file() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a.rs"),
        "pub trait Base {\n    fn greet(&self);\n}\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        "use crate::a::Base;\n\
         pub struct Derived;\n\
         impl Base for Derived {\n    fn greet(&self) {}\n}\n",
    )
    .unwrap();
    fs::write(project_dir.join("src/c.rs"), "pub fn noop() {}\n").unwrap();
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

    let reader = open_reader(&project_dir);
    assert!(
        reader.has_hierarchy_edges(),
        "expected a hierarchy edge from the initial full index — \
         test fixture isn't exercising the Rust `impl Trait for Struct` extraction"
    );
    let base_idx = find_symbol_idx(&reader, "Base").expect("Base symbol must exist");
    let edges_before = reader.find_hierarchy_edges_by_symbol(base_idx);
    assert_eq!(
        edges_before.len(),
        1,
        "expected exactly one Derived->Base edge before update"
    );
    drop(reader);

    // Edit c.rs — completely unrelated to a.rs/b.rs. Neither the parent
    // (Base) nor the child (Derived) file changes.
    fs::write(
        project_dir.join("src/c.rs"),
        "pub fn noop() {}\npub fn also_noop() {}\n",
    )
    .unwrap();

    vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let reader = open_reader(&project_dir);
    let base_idx = find_symbol_idx(&reader, "Base").expect("Base symbol must survive update");
    let edges_after = reader.find_hierarchy_edges_by_symbol(base_idx);
    assert_eq!(
        edges_after.len(),
        1,
        "vex update silently dropped the unchanged file's hierarchy edge \
         (Derived implements Base) — this is the P2a regression"
    );
    let derived_idx = find_symbol_idx(&reader, "Derived").expect("Derived symbol must survive");
    assert_eq!(
        edges_after[0].from_sym_idx, derived_idx,
        "surviving edge must still point at Derived as the child"
    );
}

// ── Parent moves to a different (changed) file ──────────────────────────
//
// The parent `Base` is deleted from a.rs and redefined in a NEW file
// a2.rs. b.rs (the child, Derived) never changes. The edge must
// re-resolve against the NEW location rather than going stale or
// vanishing — this is exactly why P2a re-resolves captures by NAME
// instead of copying the OLD resolved `to_sym_idx` verbatim.

#[test]
fn update_reresolves_hierarchy_edge_when_parent_moves_to_new_file() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a.rs"),
        "pub trait Base {\n    fn greet(&self);\n}\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        "use crate::a::Base;\n\
         pub struct Derived;\n\
         impl Base for Derived {\n    fn greet(&self) {}\n}\n",
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

    let reader = open_reader(&project_dir);
    assert!(reader.has_hierarchy_edges());
    drop(reader);

    // Move Base out of a.rs into a brand-new a2.rs. a.rs becomes empty
    // (still tracked so it's a "changed" file, not a deletion of the
    // whole module — lib.rs must reference the new module too).
    fs::write(project_dir.join("src/a.rs"), "// Base moved to a2.rs\n").unwrap();
    fs::write(
        project_dir.join("src/a2.rs"),
        "pub trait Base {\n    fn greet(&self);\n}\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/lib.rs"),
        "pub mod a;\npub mod a2;\npub mod b;\n",
    )
    .unwrap();

    vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let reader = open_reader(&project_dir);
    let base_idx = find_symbol_idx(&reader, "Base").expect("Base must exist at its new location");
    let edges_after = reader.find_hierarchy_edges_by_symbol(base_idx);
    assert_eq!(
        edges_after.len(),
        1,
        "Derived's edge must re-resolve against Base's NEW file after the move, \
         not go stale or vanish"
    );
    let derived_idx = find_symbol_idx(&reader, "Derived").expect("Derived must survive");
    assert_eq!(edges_after[0].from_sym_idx, derived_idx);
}

// ── Parent deleted entirely — must spill unresolved, not vanish ─────────
//
// Base is removed from the project entirely (its whole defining file is
// deleted). Derived's file never changes. The edge must NOT resolve to a
// stale/wrong symbol and must NOT be silently dropped — it must land in
// the unresolved_hierarchy section keyed by the verbatim name "Base",
// exactly as if `Derived` were freshly parsed against a corpus that never
// had `Base` defined locally.

#[test]
fn update_spills_unresolved_when_parent_file_is_deleted() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a.rs"),
        "pub trait Base {\n    fn greet(&self);\n}\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        "use crate::a::Base;\n\
         pub struct Derived;\n\
         impl Base for Derived {\n    fn greet(&self) {}\n}\n",
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

    let reader = open_reader(&project_dir);
    assert!(
        reader.has_hierarchy_edges(),
        "must have a Derived->Base edge before deletion"
    );
    drop(reader);

    // Delete a.rs (Base's only definition) entirely.
    fs::remove_file(project_dir.join("src/a.rs")).unwrap();
    fs::write(project_dir.join("src/lib.rs"), "pub mod b;\n").unwrap();

    vex::index::pipeline::update(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

    let reader = open_reader(&project_dir);
    // Base no longer exists as a local symbol.
    assert!(
        find_symbol_idx(&reader, "Base").is_none(),
        "Base should no longer be a local symbol after its file was deleted"
    );
    // The edge must have spilled to unresolved_hierarchy keyed by "Base",
    // not vanished and not left as a stale resolved edge pointing at a
    // reused/wrong sym_idx.
    let spilled = reader.find_unresolved_hierarchy_by_name("Base");
    assert_eq!(
        spilled.len(),
        1,
        "Derived's edge must spill to unresolved_hierarchy (keyed by verbatim \"Base\") \
         after the parent's defining file is deleted, not vanish silently"
    );
    let derived_idx = find_symbol_idx(&reader, "Derived").expect("Derived must survive");
    assert_eq!(spilled[0].from_sym_idx, derived_idx);
}

// ── Multiple update iterations — stability under self-input ─────────────
//
// Mirrors `update_preserves_ref_edges_across_multiple_iterations` in
// incremental_consistency_ref_edges.rs: the carry-forward must remain
// stable when its OWN output becomes the next iteration's input.

#[test]
fn update_preserves_hierarchy_edge_across_multiple_iterations() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("project");
    fs::create_dir_all(project_dir.join("src")).unwrap();

    fs::write(
        project_dir.join("src/a.rs"),
        "pub trait Base {\n    fn greet(&self);\n}\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/b.rs"),
        "use crate::a::Base;\n\
         pub struct Derived;\n\
         impl Base for Derived {\n    fn greet(&self) {}\n}\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/touch_me.rs"),
        "pub fn version_1() {}\n",
    )
    .unwrap();
    fs::write(
        project_dir.join("src/lib.rs"),
        "pub mod a;\npub mod b;\npub mod touch_me;\n",
    )
    .unwrap();

    vex::index::pipeline::run(
        &project_dir,
        vex::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .unwrap();

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

        let reader = open_reader(&project_dir);
        let base_idx = find_symbol_idx(&reader, "Base")
            .unwrap_or_else(|| panic!("Base must survive update iteration {version}"));
        let edges = reader.find_hierarchy_edges_by_symbol(base_idx);
        assert_eq!(
            edges.len(),
            1,
            "hierarchy edge eroded after update iteration {version} — \
             carry-forward is not stable across self-input"
        );
    }
}
