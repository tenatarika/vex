use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;
use vex::index::manifest::Manifest;
use vex::index::pipeline::{self, IndexOptions};
use vex::index::staleness::{self, Freshness};
use vex::store::format::{
    CallGraphHeader, ClusterHeader, Header, HierarchyHeader, PatternSkeletonHeader,
    UnresolvedHierarchyHeader, UnresolvedRefsHeader, V5SectionHeader,
};
use vex::store::reader::IndexReader;
use vex::util::config;

fn index(root: &Path) {
    pipeline::run(root, IndexOptions::default(), "minilm-l6-v2", &[]).unwrap();
}

fn update(root: &Path) -> anyhow::Result<(usize, usize, usize)> {
    pipeline::update(root, IndexOptions::default(), "minilm-l6-v2", &[])
}

fn index_without_clusters(root: &Path) {
    pipeline::run(
        root,
        IndexOptions {
            with_clusters: false,
            with_pattern_index: false,
            ..IndexOptions::default()
        },
        "minilm-l6-v2",
        &[],
    )
    .unwrap();
}

fn update_without_clusters(root: &Path) -> anyhow::Result<(usize, usize, usize)> {
    pipeline::update(
        root,
        IndexOptions {
            with_clusters: false,
            with_pattern_index: false,
            ..IndexOptions::default()
        },
        "minilm-l6-v2",
        &[],
    )
}

fn index_path(root: &Path) -> PathBuf {
    config::index_path(&root.canonicalize().unwrap())
}

fn index_dir(root: &Path) -> PathBuf {
    config::index_dir(&root.canonicalize().unwrap())
}

fn without_cluster_stale_flag(bytes: &[u8]) -> Vec<u8> {
    let flags_offset = Header::SIZE
        + CallGraphHeader::SIZE
        + V5SectionHeader::SIZE
        + PatternSkeletonHeader::SIZE
        + UnresolvedRefsHeader::SIZE
        + HierarchyHeader::SIZE
        + UnresolvedHierarchyHeader::SIZE
        + std::mem::offset_of!(ClusterHeader, flags);
    let mut normalized = bytes.to_vec();
    let flags = u32::from_le_bytes(
        normalized[flags_offset..flags_offset + 4]
            .try_into()
            .expect("cluster flags are present"),
    );
    normalized[flags_offset..flags_offset + 4]
        .copy_from_slice(&(flags & !ClusterHeader::FLAG_STALE).to_le_bytes());
    normalized
}
fn target_refs(root: &Path) -> Vec<(String, u32)> {
    target_refs_at(&index_path(root))
}

fn target_refs_at(path: &Path) -> Vec<(String, u32)> {
    let reader = IndexReader::open(path).unwrap();
    let paths = reader.file_paths();
    let mut refs: Vec<_> = reader
        .ref_reader()
        .unwrap()
        .find("target_processor")
        .into_iter()
        .map(|r| (paths[r.file_id as usize].clone(), r.line))
        .collect();
    refs.sort_unstable();
    refs
}

fn current_marker() -> [u8; 6] {
    [b'V', b'E', b'X', b'R', 1, 0]
}

fn usages_with_source_context(root: &Path) -> (serde_json::Value, Vec<String>) {
    let output = Command::cargo_bin("vex")
        .unwrap()
        .current_dir(root)
        .args([
            "usages",
            "target_processor",
            "--format",
            "json",
            "--no-stale-check",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "usages CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut response: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("usages CLI should emit JSON");
    let results = response["results"]
        .as_array()
        .expect("usages response should contain results");
    assert!(
        !results.is_empty(),
        "usages response must contain references"
    );
    let contexts = results
        .iter()
        .map(|result| {
            let path = result["path"].as_str().expect("result path");
            let line = result["line"].as_u64().expect("result line") as usize;
            fs::read_to_string(root.join(path))
                .unwrap()
                .lines()
                .nth(line - 1)
                .expect("result line should exist")
                .to_owned()
        })
        .collect();

    // Index age advances between CLI invocations; compare every other field.
    if let Some(meta) = response
        .get_mut("_meta")
        .and_then(serde_json::Value::as_object_mut)
    {
        meta.remove("vex.dev/index_age_ms");
    }
    (response, contexts)
}

fn publish_test_marker(root: &Path) {
    fs::create_dir_all(index_dir(root)).unwrap();
    fs::write(
        index_dir(root).join("index.parse-revision"),
        current_marker(),
    )
    .unwrap();
}

#[test]
fn unchanged_reference_only_file_survives_update() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/target.rs"), "pub fn target_processor() {}\n").unwrap();
    fs::write(
        root.join("src/calls.js"),
        "target_processor(); target_processor();\ntarget_processor();\n",
    )
    .unwrap();
    fs::write(root.join("src/changed.rs"), "pub fn before() {}\n").unwrap();

    index(root);
    let before = target_refs(root);
    assert_eq!(
        before,
        vec![
            ("src/calls.js".into(), 1),
            ("src/calls.js".into(), 1),
            ("src/calls.js".into(), 2),
            ("src/target.rs".into(), 1),
        ]
    );

    fs::write(root.join("src/changed.rs"), "pub fn after() {}\n").unwrap();
    update(root).unwrap();

    assert_eq!(target_refs(root), before);
}

#[test]
fn index_update_update_is_byte_deterministic() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn target_processor() {}\n").unwrap();
    fs::write(
        root.join("src/caller.rs"),
        "fn caller() { target_processor(); }\n",
    )
    .unwrap();
    fs::write(root.join("src/zz_unrelated.js"), "// unrelated\n").unwrap();

    index_without_clusters(root);
    let path = index_path(root);
    let indexed = fs::read(&path).unwrap();
    let _ = fs::remove_file(index_dir(root).join("index.parse-revision"));
    let manifest = Manifest::load(&config::manifest_path(&root.canonicalize().unwrap())).unwrap();
    assert!(matches!(
        staleness::check(&root.canonicalize().unwrap(), &manifest, false),
        Freshness::Stale { .. }
    ));

    let (count, changed, deleted) = update_without_clusters(root).unwrap();
    assert_eq!(count, 2);
    assert_eq!(
        changed, 3,
        "missing revision must force a true-write migration"
    );
    assert_eq!(deleted, 0);
    let first_update = fs::read(&path).unwrap();
    assert_eq!(
        first_update, indexed,
        "full index and migration update bytes differ"
    );

    let (count, changed, deleted) = update_without_clusters(root).unwrap();
    assert_eq!(count, 2);
    assert_eq!((changed, deleted), (0, 0));
    assert_eq!(fs::read(&path).unwrap(), first_update);

    // A real changed file forces ordinary unchanged-reference reconstruction.
    // This lexically last zero-symbol file changes only trailing trivia, so
    // symbols, reference positions, and the deterministic index bytes stay fixed.
    fs::write(root.join("src/zz_unrelated.js"), "// unrelated \n").unwrap();
    let (count, changed, deleted) = update_without_clusters(root).unwrap();
    assert_eq!(count, 2);
    assert_eq!(changed, 1);
    assert_eq!(deleted, 0);
    assert_eq!(fs::read(&path).unwrap(), first_update);

    let (count, changed, deleted) = update_without_clusters(root).unwrap();
    assert_eq!(count, 2);
    assert_eq!((changed, deleted), (0, 0));
    assert_eq!(fs::read(&path).unwrap(), first_update);
}

#[test]
fn default_options_order_ref_only_files_between_symbol_files() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/z.rs"), "pub fn other_processor() {}\n").unwrap();
    fs::write(
        root.join("src/middle.rs"),
        "use crate::target_processor;\nuse crate::other_processor;\n",
    )
    .unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target_processor() {}\n").unwrap();

    index(root);
    let path = index_path(root);
    let indexed = fs::read(&path).unwrap();
    let reader = IndexReader::open(&path).unwrap();
    assert!(reader.has_clusters(), "default index includes clusters");
    assert!(
        reader.pattern_skeleton_reader().is_some(),
        "default index includes the pattern index"
    );
    assert!(
        (0..reader.symbol_count()).all(|index| {
            let symbol = reader.symbol(index).expect("symbol index is valid");
            reader.read_string(symbol.file_offset) != "src/middle.rs"
        }),
        "middle.rs is reference-only"
    );
    assert_eq!(
        reader.file_paths(),
        ["src/a.rs", "src/middle.rs", "src/z.rs"]
    );
    assert_eq!(
        target_refs_at_name(&path, "target_processor"),
        [
            ("src/a.rs".to_string(), 1),
            ("src/middle.rs".to_string(), 1),
            ("src/middle.rs".to_string(), 1),
        ]
    );
    assert_eq!(
        target_refs_at_name(&path, "other_processor"),
        [
            ("src/middle.rs".to_string(), 2),
            ("src/middle.rs".to_string(), 2),
            ("src/z.rs".to_string(), 1),
        ]
    );

    let _ = fs::remove_file(index_dir(root).join("index.parse-revision"));
    let (count, changed, deleted) = update(root).unwrap();
    assert_eq!(count, 2);
    assert_eq!((changed, deleted), (3, 0));
    let migrated = fs::read(&path).unwrap();
    assert_eq!(
        reader.cluster_header().unwrap().flags,
        ClusterHeader::FLAG_COMPUTED,
        "fresh clustering is computed and current"
    );
    let migrated_reader = IndexReader::open(&path).unwrap();
    assert_eq!(
        migrated_reader.cluster_header().unwrap().flags,
        ClusterHeader::FLAG_COMPUTED | ClusterHeader::FLAG_STALE,
        "migration carries clusters and marks them stale"
    );
    assert_eq!(
        without_cluster_stale_flag(&migrated),
        without_cluster_stale_flag(&indexed),
        "migration bytes differ beyond the documented cluster stale flag"
    );

    fs::write(
        root.join("src/middle.rs"),
        "use crate::target_processor; // changed trivia\nuse crate::other_processor;\n",
    )
    .unwrap();
    let (count, changed, deleted) = update(root).unwrap();
    assert_eq!(count, 2);
    assert_eq!((changed, deleted), (1, 0));
    let changed_ref_only = without_cluster_stale_flag(&fs::read(&path).unwrap());
    assert_eq!(
        target_refs_at_name(&path, "target_processor"),
        [
            ("src/a.rs".to_string(), 1),
            ("src/middle.rs".to_string(), 1),
            ("src/middle.rs".to_string(), 1),
        ]
    );
    assert_eq!(
        target_refs_at_name(&path, "other_processor"),
        [
            ("src/middle.rs".to_string(), 2),
            ("src/middle.rs".to_string(), 2),
            ("src/z.rs".to_string(), 1),
        ]
    );

    let (count, changed, deleted) = update(root).unwrap();
    assert_eq!(count, 2);
    assert_eq!((changed, deleted), (0, 0));
    assert_eq!(
        without_cluster_stale_flag(&fs::read(&path).unwrap()),
        changed_ref_only,
        "no-op update bytes differ beyond the documented cluster stale flag"
    );
}

fn target_refs_at_name(path: &Path, name: &str) -> Vec<(String, u32)> {
    let reader = IndexReader::open(path).unwrap();
    let paths = reader.file_paths();
    let mut refs: Vec<_> = reader
        .ref_reader()
        .unwrap()
        .find(name)
        .into_iter()
        .map(|r| (paths[r.file_id as usize].clone(), r.line))
        .collect();
    refs.sort_unstable();
    refs
}

#[test]
fn reconstructed_ref_context_has_no_observable_loss() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn target_processor() {}\nfn caller() {\n    target_processor();\n}\n",
    )
    .unwrap();
    fs::write(root.join("src/other.rs"), "pub fn before() {}\n").unwrap();

    Command::cargo_bin("vex")
        .unwrap()
        .current_dir(root)
        .args(["index"])
        .assert()
        .success();
    let local_index = root.join(".vex_cache").join("index.vex");
    assert!(
        local_index.is_file(),
        "CLI index did not create local index"
    );
    let before = target_refs_at(&local_index);
    assert_eq!(
        before,
        vec![("src/lib.rs".into(), 1), ("src/lib.rs".into(), 3)]
    );
    let (before_usages, before_context) = usages_with_source_context(root);
    assert_eq!(
        before_usages["results"],
        serde_json::json!([{ "path": "src/lib.rs", "line": 3 }])
    );
    assert_eq!(before_context, vec!["    target_processor();"]);

    fs::write(root.join("src/other.rs"), "pub fn after() {}\n").unwrap();
    Command::cargo_bin("vex")
        .unwrap()
        .current_dir(root)
        .args(["update"])
        .assert()
        .success();

    assert_eq!(target_refs_at(&local_index), before);
    let (after_usages, after_context) = usages_with_source_context(root);
    assert_eq!(after_usages, before_usages);
    assert_eq!(after_context, before_context);
}

#[test]
fn current_revision_marker_is_invalidated_before_partial_commit() {
    for temp_name in ["index.vex.tmp", "manifest.json.tmp", "index.state.tmp"] {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn before() {}\n").unwrap();
        index(root);
        publish_test_marker(root);
        fs::write(root.join("src/lib.rs"), "pub fn after() {}\n").unwrap();

        let blocker = index_dir(root).join(temp_name);
        fs::create_dir(&blocker).unwrap();
        assert!(
            update(root).is_err(),
            "blocked write unexpectedly succeeded: {temp_name}"
        );
        assert_ne!(
            fs::read(index_dir(root).join("index.parse-revision")).unwrap(),
            current_marker(),
            "partial commit left revision current after {temp_name} failure"
        );

        fs::remove_dir(&blocker).unwrap();
        update(root).unwrap();
        assert_eq!(
            fs::read(index_dir(root).join("index.parse-revision")).unwrap(),
            current_marker(),
            "successful retry did not publish current revision after {temp_name} failure"
        );
    }
}
