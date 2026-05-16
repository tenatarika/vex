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
    vex::index::pipeline::run(&project_dir, false, "minilm-l6-v2", &[]).unwrap();

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
    let (total, changed, deleted) =
        vex::index::pipeline::update(&project_dir, false, "minilm-l6-v2", &[]).unwrap();

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
    vex::index::pipeline::run(&project_dir, false, "minilm-l6-v2", &[]).unwrap();

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
    let (_total, changed, deleted) =
        vex::index::pipeline::update(&project_dir, false, "minilm-l6-v2", &[]).unwrap();

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
    vex::index::pipeline::run(&project_dir, false, "minilm-l6-v2", &[]).unwrap();

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
    let (total, changed, deleted) =
        vex::index::pipeline::update(&project_dir, false, "minilm-l6-v2", &[]).unwrap();

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
    vex::index::pipeline::run(&project_dir, false, "minilm-l6-v2", &[]).unwrap();

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
    let (total, changed, deleted) =
        vex::index::pipeline::update(&project_dir, false, "minilm-l6-v2", &[]).unwrap();

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
